#!/usr/bin/env bats

# End-to-end test for space-scoped notes.
#
# A note belongs to exactly one of `(project_id, space_id)`. Notes are
# managed via two surfaces:
#
# - Project notes: `drua_admin_note` (and the GraphQL `noteDelete`
#   mutation). Pin state is set via `pin` / `unpin` commands.
# - Space notes: the `spaces` tool (`spaces edit op=write|delete`).
#   The on-disk markdown file is the source of truth — pin state lives
#   in the frontmatter (`pinned: true`). The library's reverse-sync
#   importer is the only path that mutates the DB.
#
# Drives the production reverse-sync pathway end-to-end. No LLM round
# trips — assertions read the agent's `systemBlocks` via GraphQL and
# observe importer side-effects via psql.

load helpers

setup_file() {
  if [ "${SKIP_COMPOSE:-0}" = "1" ]; then
    psql "${PG_CON:-postgres://user:password@localhost:5432/drua}" \
      -q -c "TRUNCATE job_events, job_executions CASCADE;" \
      -c "DELETE FROM jobs;" \
      -c "DELETE FROM note_events; DELETE FROM notes;" \
      -c "DELETE FROM library_documents;" \
      > /dev/null 2>&1 || true
  fi

  UPSTREAM_REPO="$BATS_FILE_TMPDIR/upstream.git"
  WORKING_REPO="$BATS_FILE_TMPDIR/upstream-working"
  mkdir -p "$WORKING_REPO"
  git init --bare --initial-branch=main "$UPSTREAM_REPO" >/dev/null
  git -C "$WORKING_REPO" init -q -b main
  git -C "$WORKING_REPO" config user.email test@drua
  git -C "$WORKING_REPO" config user.name "Drua Test"
  mkdir -p "$WORKING_REPO/spaces"
  : > "$WORKING_REPO/spaces/.gitkeep"
  git -C "$WORKING_REPO" add -A
  git -C "$WORKING_REPO" commit -q -m "init"
  git -C "$WORKING_REPO" remote add origin "$UPSTREAM_REPO"
  git -C "$WORKING_REPO" push -q origin main

  cat > "$BATS_FILE_TMPDIR/drua.yml" <<EOF
server:
  port: 4200
  host: "0.0.0.0"
  secure_cookies: false
  mcp_endpoint: "http://localhost:4200/mcp"
oauth:
  login: github
  github_client_id: "test-client-id"
  github_redirect_uri: "http://localhost:4200/auth/github/callback"
  github_allowed_teams: []
agents:
  builtin_roles:
    project_lead:
      model: test-model
      compaction:
        prune_after_seconds: 600
    agent:
      model: test-model
      compaction:
        prune_after_seconds: 600
providers:
  - name: openai
    base_url: http://127.0.0.1:9
    models:
      - name: test-model
        max_tokens_per_response: 1024
        context_window_tokens: 4096
library:
  data_dir: "$BATS_FILE_TMPDIR/library"
  repo_url: "file://$UPSTREAM_REPO"
  skill_sync_interval_secs: 1
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
EOF
  export DRUA_CONFIG="$BATS_FILE_TMPDIR/drua.yml"
  start_server
}

teardown_file() {
  stop_server
}

# Calls a searchable admin tool.
admin_call() {
  local tool_name="$1"
  local args_json="$2"
  local body
  body="$(jq -nc --arg t "drua_admin_$tool_name" --argjson a "$args_json" '{
    name: "call_tool",
    arguments: { tool_name: $t, arguments: $a }
  }')"
  mcp_call "$AGENT_TOKEN" "tools/call" "$body"
}

# Drop a note markdown into a space via `spaces edit op=write`.
write_note_to_space() {
  local space_slug="$1"
  local relpath="$2"
  local content="$3"
  admin_call "spaces" "$(jq -nc --arg slug "$space_slug" --arg path "$relpath" --arg content "$content" '{
    command: "edit",
    slug: $slug,
    edit_op: "write",
    op_args: { path: $path, content: $content }
  }')"
}

# Render canonical note markdown. `pinned=true` emits `pinned: true` in
# the frontmatter — picked up by `parse_note_markdown` and applied at
# first-import time only.
render_note_md() {
  local id="$1"
  local title="$2"
  local body="$3"
  local pinned="${4:-false}"
  if [ "$pinned" = "true" ]; then
    printf -- '---\nid: %s\ntags: []\ncreated: \nupdated: \npinned: true\n---\n\n# %s\n\n%s\n' \
      "$id" "$title" "$body"
  else
    printf -- '---\nid: %s\ntags: []\ncreated: \nupdated: \n---\n\n# %s\n\n%s\n' \
      "$id" "$title" "$body"
  fi
}

# Polls the `notes` table for an Initialized event with the given title.
wait_for_note_row() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT 1 FROM note_events WHERE event_type = 'initialized' AND event->>'title' = '$needle' LIMIT 1;" 2>/dev/null || true)"
    if [ -n "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# Polls until the matching row is soft-deleted or absent. Reverse-sync
# delete-in path drives this when a markdown file is removed in git
# via the spaces tool.
wait_for_note_row_gone() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT 1 FROM notes n JOIN note_events ne ON ne.id = n.id WHERE ne.event_type='initialized' AND ne.event->>'title' = '$needle' AND n.deleted = FALSE LIMIT 1;" 2>/dev/null || true)"
    if [ -z "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# Polls library_documents until the row for the given title is gone.
wait_for_note_search_gone() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT 1 FROM library_documents WHERE doc_type='note' AND name='$needle' LIMIT 1;" 2>/dev/null || true)"
    if [ -z "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

agent_notes_block() {
  local agent_id="$1"
  graphql_query "{ agent(id: \"$agent_id\") { session { systemBlocks { kind text index } } } }" "$AGENT_TOKEN" \
    | jq -r '
      .data.agent.session.systemBlocks
      | map(select(.kind == "NOTES"))
      | sort_by(.index)
      | (last.text // "")
    '
}

note_id_by_title() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  psql "$pg" -At -c "SELECT id FROM note_events WHERE event_type = 'initialized' AND event->>'title' = '$needle' ORDER BY recorded_at DESC LIMIT 1;" 2>/dev/null || true
}

@test "notes: space-scoped notes appear in mounting projects' agent prompts; pin via frontmatter" {
  create_test_agent

  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="pn-$suffix"
  local slug_a="ns-a-$suffix"
  local slug_b="ns-b-$suffix"

  # 1. Project P.
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Spaces.
  run admin_call "spaces" "$(jq -nc --arg s "$slug_a" '{command:"create",slug:$s,description:"Alpha"}')"
  echo "$output"
  [[ "$output" == *"$slug_a"* ]]
  run admin_call "spaces" "$(jq -nc --arg s "$slug_b" '{command:"create",slug:$s,description:"Beta"}')"
  echo "$output"

  # 3. Drop notes into spaces. Alpha is pinned via frontmatter; beta
  #    is unpinned (the default) — title shows in Recent index only.
  alpha_note_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  alpha_note_title="alpha-note-$suffix"
  alpha_note_md="$(render_note_md "$alpha_note_id" "$alpha_note_title" "Alpha note body" true)"
  run write_note_to_space "$slug_a" "notes/alpha.md" "$alpha_note_md"
  echo "$output"
  [[ "$output" == *"Wrote space:$slug_a/notes/alpha.md"* ]]

  beta_note_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  beta_note_title="beta-note-$suffix"
  beta_note_md="$(render_note_md "$beta_note_id" "$beta_note_title" "Beta note body" false)"
  run write_note_to_space "$slug_b" "notes/beta.md" "$beta_note_md"
  echo "$output"

  # 4. Mount space-A onto P (B remains unmounted).
  run admin_call "spaces" "$(jq -nc --arg slug "$slug_a" --arg pid "$project_id" '{
    command: "mount", slug: $slug, project_id: $pid
  }')"
  echo "$output"

  # 5. Wait for both note rows to land in the DB.
  run wait_for_note_row "$alpha_note_title"
  [ "$status" -eq 0 ] || { echo "importer never landed alpha note"; return 1; }
  run wait_for_note_row "$beta_note_title"
  [ "$status" -eq 0 ] || { echo "importer never landed beta note"; return 1; }

  # 6. Observer-1: P mounts space-A → alpha title + body appear in
  #    Pinned section (frontmatter said pinned: true). Beta does NOT
  #    appear because space-B isn't mounted.
  run graphql_query "mutation { agentCreate(input: { projectId: \"$project_id\", name: \"obs-n1\" }) { agent { id } } }" "$AGENT_TOKEN"
  echo "$output"
  obs_n1="$(echo "$output" | jq -r '.data.agentCreate.agent.id')"
  block_n1="$(agent_notes_block "$obs_n1")"
  echo "obs-n1 NOTES block: $block_n1"
  [[ "$block_n1" == *"$alpha_note_title"* ]]
  [[ "$block_n1" == *"Alpha note body"* ]]
  [[ "$block_n1" != *"$beta_note_title"* ]]

  # 6b. `library_search` with `projectId` set must surface
  #     space-scoped notes from the project's mounted spaces. Without
  #     the mount fold-in, the project filter silently hides them
  #     (the bug `notes.search` had — same fix at the GraphQL layer).
  alpha_search="$(graphql_query "query { librarySearch(input: { query: \"Alpha note body\", projectId: \"$project_id\", types: [NOTE] }) { id title spaceSlug projectId } }" "$AGENT_TOKEN")"
  echo "alpha library_search: $alpha_search"
  [[ "$alpha_search" == *"$alpha_note_title"* ]]
  [[ "$alpha_search" == *"$slug_a"* ]]

  # And the unmounted space's note (beta) must NOT surface — it lives
  # in space-B which P doesn't mount yet.
  beta_search="$(graphql_query "query { librarySearch(input: { query: \"Beta note body\", projectId: \"$project_id\", types: [NOTE] }) { id title } }" "$AGENT_TOKEN")"
  echo "beta library_search (unmounted): $beta_search"
  [[ "$beta_search" != *"$beta_note_title"* ]]

  # 7. Mount space-B; both notes now visible. Beta surfaces as a title
  #    in Recent index (unpinned), alpha keeps its body in Pinned.
  run admin_call "spaces" "$(jq -nc --arg slug "$slug_b" --arg pid "$project_id" '{
    command: "mount", slug: $slug, project_id: $pid
  }')"
  echo "$output"

  run graphql_query "mutation { agentCreate(input: { projectId: \"$project_id\", name: \"obs-n2\" }) { agent { id } } }" "$AGENT_TOKEN"
  echo "$output"
  obs_n2="$(echo "$output" | jq -r '.data.agentCreate.agent.id')"
  block_n2="$(agent_notes_block "$obs_n2")"
  echo "obs-n2 NOTES block: $block_n2"
  [[ "$block_n2" == *"$alpha_note_title"* ]]
  [[ "$block_n2" == *"Alpha note body"* ]]
  [[ "$block_n2" == *"$beta_note_title"* ]]
}

@test "notes: note.delete + spaces edit op=delete both wipe library search rows" {
  create_test_agent

  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="ndel-$suffix"
  local space_slug="ndel-s-$suffix"

  # 1. Project + lead.
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Project note via admin `note create`. Title carries the run
  #    suffix because library_documents.name is unique-per-doc-type
  #    (different rows per scope, but same name across runs collides
  #    on the search-store upsert path).
  proj_note_title="ProjNote-$suffix"
  run admin_call "note" "$(jq -nc --arg pid "$project_id" --arg t "$proj_note_title" '{
    command: "create", project_id: $pid, title: $t, content: "project body"
  }')"
  echo "$output"
  proj_note_id="$(echo "$output" \
    | jq -r '.result.content[0].text' \
    | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' \
    | head -n1)"
  [ -n "$proj_note_id" ] || { echo "could not extract project note id from: $output"; return 1; }

  # 3. Space + space-scoped note via spaces.edit (importer creates the
  #    row with `space_id` set).
  run admin_call "spaces" "$(jq -nc --arg s "$space_slug" '{command:"create",slug:$s,description:"Delete-test space"}')"
  echo "$output"

  space_note_uuid="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  space_note_title="SpaceNote-$suffix"
  space_note_md="$(render_note_md "$space_note_uuid" "$space_note_title" "space body" false)"
  run write_note_to_space "$space_slug" "notes/space-note.md" "$space_note_md"
  echo "$output"
  [[ "$output" == *"Wrote space:$space_slug/notes/space-note.md"* ]]

  # 4. Both rows must land before we test delete.
  run wait_for_note_row "$proj_note_title"
  [ "$status" -eq 0 ] || { echo "$proj_note_title never landed in notes"; return 1; }
  run wait_for_note_row "$space_note_title"
  [ "$status" -eq 0 ] || { echo "$space_note_title never landed in notes"; return 1; }

  # 5. Delete the project note via admin `note delete`. Forward path
  #    must wipe both `notes` (soft) and `library_documents` (hard).
  run admin_call "note" "$(jq -nc --arg pid "$project_id" --arg nid "$proj_note_id" '{
    command: "delete", project_id: $pid, note_id: $nid
  }')"
  echo "$output"
  [[ "$output" == *"Note deleted"* ]]

  run wait_for_note_search_gone "$proj_note_title"
  [ "$status" -eq 0 ] || { echo "$proj_note_title still in library_documents after note.delete"; return 1; }

  # 6. Trying to delete the space note via the project surface must
  #    surface the dedicated space-scope guardrail rather than a
  #    generic Forbidden auth error. Use the GraphQL mutation so we
  #    actually hit `Notes::delete` (admin route filters by
  #    project_id; passing a wrong project would 403 first).
  space_note_db_id="$(note_id_by_title "$space_note_title")"
  [ -n "$space_note_db_id" ]
  # Mount the space onto P so the project surface can find the row,
  # then Notes::delete should reject it with SpaceScopedDeleteViaSpaces.
  run admin_call "spaces" "$(jq -nc --arg slug "$space_slug" --arg pid "$project_id" '{
    command: "mount", slug: $slug, project_id: $pid
  }')"
  echo "$output"

  # GraphQL noteDelete rejects with the dedicated error message.
  run graphql_query "mutation { noteDelete(input: { id: \"$space_note_db_id\", projectId: \"$project_id\" }) { deletedId } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *"spaces tool"* ]]

  # 7. Delete the space note via `spaces edit op=delete`. File goes
  #    out via direct GitEngine, importer's `delete_in_op` then wipes
  #    the row + search row.
  run admin_call "spaces" "$(jq -nc --arg s "$space_slug" '{
    command: "edit", slug: $s, edit_op: "delete",
    op_args: { path: "notes/space-note.md" }
  }')"
  echo "$output"
  [[ "$output" != *"PathNotFound"* ]]

  run wait_for_note_row_gone "$space_note_title"
  [ "$status" -eq 0 ] || { echo "$space_note_title row still present after spaces delete"; return 1; }
  run wait_for_note_search_gone "$space_note_title"
  [ "$status" -eq 0 ] || { echo "$space_note_title still in library_documents after spaces delete"; return 1; }
}
