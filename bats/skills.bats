#!/usr/bin/env bats

# End-to-end glue test for the multi-tier skills system.
#
# Drives the production reverse-sync pathway: skills are dropped into a
# space via `spaces edit op=write`, the library importer (poll-driven)
# lands them as `Skill` rows with `space_id` set, and agents created in
# projects that mount that space see them in their `<available_skills>`
# system block. Agents in projects that don't mount the space don't.
#
# No LLM round trips — assertions read the agent's `systemBlocks` via
# GraphQL. Each `agentCreate` snapshots its initial system blocks at
# creation time, so we observe the resolution by creating fresh agents
# at each stage rather than re-prompting.
#
# Library upstream is a bare git repo seeded under `$BATS_FILE_TMPDIR`,
# wired via `library.repo_url: file://...` in a per-test drua.yml — no
# GitHub dependency.

load helpers

setup_file() {
  # When sharing PG with other tests / repeated runs (SKIP_COMPOSE),
  # `spawn_unique` on `library.sync` from a prior run leaves a stale
  # row in `jobs` that prevents the job from re-queuing — so the
  # importer never runs. Wipe the prior library/job state before
  # the server boots. Skill / project rows from prior test cycles
  # also linger; clean them to avoid name-uniqueness collisions.
  if [ "${SKIP_COMPOSE:-0}" = "1" ]; then
    psql "${PG_CON:-postgres://user:password@localhost:5432/drua}" \
      -q -c "TRUNCATE job_events, job_executions CASCADE;" \
      -c "DELETE FROM jobs;" \
      -c "DELETE FROM skill_events; DELETE FROM skills;" \
      -c "DELETE FROM library_documents;" \
      > /dev/null 2>&1 || true
  fi

  # Stand up an isolated upstream library — a bare git repo seeded with
  # one commit on `main`. The library service clones from this; writes
  # flow back via git push (which works on the bare repo).
  UPSTREAM_REPO="$BATS_FILE_TMPDIR/upstream.git"
  WORKING_REPO="$BATS_FILE_TMPDIR/upstream-working"
  mkdir -p "$WORKING_REPO"
  git init --bare --initial-branch=main "$UPSTREAM_REPO" >/dev/null
  git -C "$WORKING_REPO" init -q -b main
  git -C "$WORKING_REPO" config user.email test@drua
  git -C "$WORKING_REPO" config user.name "Drua Test"
  # Spaces live at the repo root (`spaces/<slug>/...`) — not under
  # `runtime/`. Seed the directory so the bare repo's HEAD has a tree
  # the importer can diff against.
  mkdir -p "$WORKING_REPO/spaces"
  : > "$WORKING_REPO/spaces/.gitkeep"
  git -C "$WORKING_REPO" add -A
  git -C "$WORKING_REPO" commit -q -m "init"
  git -C "$WORKING_REPO" remote add origin "$UPSTREAM_REPO"
  git -C "$WORKING_REPO" push -q origin main

  # Render a per-file drua.yml pointing at the upstream + an isolated
  # data_dir. `skill_sync_interval_secs: 1` makes the importer tick
  # every second so polls finish quickly.
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
  default_chain:
    primary: { name: test-model }
  builtin_roles:
    project_lead:
      compaction:
        prune_after_seconds: 600
    agent:
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

# Render the canonical skill markdown the importer accepts.
# Frontmatter `id`/`name` makes the parse path deterministic and avoids
# the importer needing to canonicalise (`needs_rewrite`).
render_skill_md() {
  local id="$1"
  local name="$2"
  local description="$3"
  local body="$4"
  printf -- '---\nid: %s\nname: "%s"\ndescription: "%s"\ncreated: \nupdated: \n---\n\n%s\n' \
    "$id" "$name" "$description" "$body"
}

# Calls a searchable admin tool. The MCP gateway exposes admin tools
# under the `drua_admin_<tool>` prefixed name dispatched through the
# top-level `call_tool` meta-tool — same shape as other searchable
# toolsets (honeycomb, github, etc.).
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

# Drop a skill markdown file into a space via `spaces edit op=write`.
# Goes through the production reverse-sync pathway: the library commits
# the file, then the importer lands a `Skill { space_id }` row.
write_skill_to_space() {
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

# Returns the SKILLS block text for `$1` (agent id) — empty string when
# no SKILLS block has been pushed yet (the agent hasn't picked up any
# skills at all).
#
# Reads the session-level `systemBlocks` field, which exposes every
# block ever pushed in the session (the flat materialised list) — does
# NOT require a thread, so it works on a freshly-created agent before
# any prompt has fired. Filters to the latest SKILLS block by `index`,
# since `SystemBlocksUpdated` events append a new block of the same
# kind rather than mutating the prior one.
agent_skills_block() {
  local agent_id="$1"
  graphql_query "{ agent(id: \"$agent_id\") { session { systemBlocks { kind text index } } } }" "$AGENT_TOKEN" \
    | jq -r '
      .data.agent.session.systemBlocks
      | map(select(.kind == "SKILLS"))
      | sort_by(.index)
      | (last.text // "")
    '
}

# Polls the `skills` table (via psql) until a row with name=$1 appears.
# Returns 0 on hit, 1 on 30s timeout. Used to gate `agentCreate` calls
# behind the importer landing the skill row — agent system blocks are
# frozen at creation time, so the skill MUST exist in the DB before
# the observer is spawned. Direct PG read because we're observing the
# importer's side-effect, not the public API surface; the assertions
# about the agent's system blocks still go through GraphQL.
wait_for_skill_row() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT name FROM skills WHERE name = '$needle' AND deleted = FALSE LIMIT 1;" 2>/dev/null || true)"
    if [ -n "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

@test "skills: space-scoped skills appear in agent prompt only when project mounts the space" {
  create_test_agent

  # Unique-per-run names so re-running the test against a persisted PG
  # (developer iteration) doesn't collide on `idx_projects_name_unique` or
  # the spaces slug constraint.
  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="p-$suffix"
  local slug_a="space-a-$suffix"
  local slug_b="space-b-$suffix"

  # 1. Project P (auto-creates the lead agent — graphql gives JSON).
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Space-A (will be mounted on the project).
  run admin_call "spaces" "$(jq -nc --arg s "$slug_a" '{command:"create",slug:$s,description:"Alpha"}')"
  echo "$output"
  [[ "$output" == *"$slug_a"* ]]

  # 3. Drop a skill markdown into space-A (production path: spaces.edit
  #    → library write → importer creates the `Skill { space_id }` row).
  #    Body uses $ARGUMENTS so the invoke assertion below sees substitution.
  alpha_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  # Filename is the canonical slug (Model A) — `alpha-skill.md` →
  # `alpha-skill` is the invocation handle.
  alpha_md="$(render_skill_md "$alpha_id" "alpha-skill" "alpha description" "echo alpha \$ARGUMENTS")"
  run write_skill_to_space "$slug_a" "skills/alpha-skill.md" "$alpha_md"
  echo "$output"
  [[ "$output" == *"Wrote space:$slug_a/skills/alpha-skill.md"* ]]

  # 4. Mount space-A onto project P.
  run admin_call "spaces" "$(jq -nc --arg slug "$slug_a" --arg pid "$project_id" '{
    command: "mount", slug: $slug, project_id: $pid
  }')"
  echo "$output"

  # 5. Wait for the importer to land the alpha skill row before
  #    creating the observer — the agent's session blocks are frozen at
  #    creation time.
  run wait_for_skill_row "alpha-skill"
  [ "$status" -eq 0 ] || { echo "importer never landed alpha-skill"; return 1; }

  # 6. Observer-1: project P mounts space-A → sees alpha.
  run graphql_query "mutation { agentCreate(input: { projectId: \"$project_id\", name: \"obs-1\" }) { agent { id } } }" "$AGENT_TOKEN"
  echo "$output"
  obs_1="$(echo "$output" | jq -r '.data.agentCreate.agent.id')"
  [ -n "$obs_1" ] && [ "$obs_1" != "null" ]
  block_1="$(agent_skills_block "$obs_1")"
  echo "obs-1 SKILLS block: $block_1"
  [[ "$block_1" == *"alpha-skill"* ]]

  # 6b. Invoke alpha via admin `skill invoke` — exercises
  #     `Skills::interpolate_skill` end-to-end, including the
  #     mounted-space resolution path that `find_by_name` had skipped
  #     before this PR (cursor finding r3185924186).
  run admin_call "skill" "$(jq -nc --arg pid "$project_id" '{
    command: "invoke", project_id: $pid, name: "alpha-skill", arguments: "PR-7"
  }')"
  echo "$output"
  [[ "$output" == *"echo alpha PR-7"* ]]

  # 7. Space-B (NOT mounted yet) + skill in it.
  run admin_call "spaces" "$(jq -nc --arg s "$slug_b" '{command:"create",slug:$s,description:"Beta"}')"
  echo "$output"
  beta_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  beta_md="$(render_skill_md "$beta_id" "beta-skill" "beta description" "echo beta")"
  run write_skill_to_space "$slug_b" "skills/beta-skill.md" "$beta_md"
  echo "$output"
  [[ "$output" == *"Wrote space:$slug_b/skills/beta-skill.md"* ]]
  run wait_for_skill_row "beta-skill"
  [ "$status" -eq 0 ] || { echo "importer never landed beta-skill"; return 1; }

  # 8. Observer-2: project P mounts space-A only — beta exists in DB
  #    but space-B is NOT mounted, so obs-2 sees alpha and NOT beta.
  run graphql_query "mutation { agentCreate(input: { projectId: \"$project_id\", name: \"obs-2\" }) { agent { id } } }" "$AGENT_TOKEN"
  echo "$output"
  obs_2="$(echo "$output" | jq -r '.data.agentCreate.agent.id')"
  block_2="$(agent_skills_block "$obs_2")"
  echo "obs-2 SKILLS block: $block_2"
  [[ "$block_2" == *"alpha-skill"* ]]
  [[ "$block_2" != *"beta-skill"* ]]

  # 8b. Invoke beta via admin `skill invoke` from project P — beta lives
  #     in space-B which P has NOT mounted, so resolution must fail.
  run admin_call "skill" "$(jq -nc --arg pid "$project_id" '{
    command: "invoke", project_id: $pid, name: "beta-skill"
  }')"
  echo "$output"
  [[ "$output" == *"Unknown skill: beta-skill"* ]]

  # 9. Mount space-B onto project P.
  run admin_call "spaces" "$(jq -nc --arg slug "$slug_b" --arg pid "$project_id" '{
    command: "mount", slug: $slug, project_id: $pid
  }')"
  echo "$output"

  # 10. Observer-3: created post-mount. Sees BOTH skills.
  run graphql_query "mutation { agentCreate(input: { projectId: \"$project_id\", name: \"obs-3\" }) { agent { id } } }" "$AGENT_TOKEN"
  echo "$output"
  obs_3="$(echo "$output" | jq -r '.data.agentCreate.agent.id')"
  block_3="$(agent_skills_block "$obs_3")"
  echo "obs-3 SKILLS block: $block_3"
  [[ "$block_3" == *"alpha-skill"* ]]
  [[ "$block_3" == *"beta-skill"* ]]

  # 10b. After mount, beta resolves via admin `skill invoke`.
  run admin_call "skill" "$(jq -nc --arg pid "$project_id" '{
    command: "invoke", project_id: $pid, name: "beta-skill"
  }')"
  echo "$output"
  [[ "$output" == *"echo beta"* ]]
}

# Polls library_documents until a skill row with `name=$1` is gone.
# Returns 0 when the row is absent, 1 on 30s timeout. Used to gate
# search-removed assertions behind the post-delete commit hook +
# reverse-sync importer's `delete_in_op` actually wiping the row.
wait_for_skill_search_gone() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT 1 FROM library_documents WHERE doc_type='skill' AND name='$needle' LIMIT 1;" 2>/dev/null || true)"
    if [ -z "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# Polls until the `skills` row is soft-deleted (deleted = TRUE) or
# absent. Reverse-sync delete-in path drives this when a markdown
# file is removed in git via the spaces tool.
wait_for_skill_row_gone() {
  local needle="$1"
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  for _ in $(seq 1 30); do
    local hit
    hit="$(psql "$pg" -At -c "SELECT 1 FROM skills WHERE name = '$needle' AND deleted = FALSE LIMIT 1;" 2>/dev/null || true)"
    if [ -z "$hit" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

@test "skills: skill.delete + spaces edit op=delete both wipe library search rows" {
  create_test_agent

  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="pdel-$suffix"
  local space_slug="sdel-$suffix"

  # 1. Project + lead.
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Project skill via admin `skill create` (this is the path the
  #    lead agent's tool would take during the manual test). The MCP
  #    `tools/call` envelope wraps the result in `result.content[0].text`
  #    — extract the skill UUID from that, not from the JSON id field.
  run admin_call "skill" "$(jq -nc --arg pid "$project_id" '{
    command: "create", project_id: $pid,
    name: "hello-world",
    description: "Greeting skill",
    body: "echo hello"
  }')"
  echo "$output"
  hello_id="$(echo "$output" \
    | jq -r '.result.content[0].text' \
    | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' \
    | head -n1)"
  [ -n "$hello_id" ] || { echo "could not extract hello-world skill id from: $output"; return 1; }

  # 3. Space + space-scoped skill (raw file via spaces.edit; importer
  #    creates the row with `space_id` set).
  run admin_call "spaces" "$(jq -nc --arg s "$space_slug" '{command:"create",slug:$s,description:"Delete-test space"}')"
  echo "$output"

  goodbye_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  goodbye_md="$(render_skill_md "$goodbye_id" "goodbye-world" "farewell skill" "echo bye")"
  run write_skill_to_space "$space_slug" "skills/goodbye-world.md" "$goodbye_md"
  echo "$output"
  [[ "$output" == *"Wrote space:$space_slug/skills/goodbye-world.md"* ]]

  # 4. Both rows must show up via library search before we test delete.
  run wait_for_skill_row "hello-world"
  [ "$status" -eq 0 ] || { echo "hello-world never landed in skills"; return 1; }
  run wait_for_skill_row "goodbye-world"
  [ "$status" -eq 0 ] || { echo "goodbye-world never landed in skills"; return 1; }

  # 5. Delete the project skill via `skill delete`. Forward path
  #    must wipe both `skills` (soft) and `library_documents` (hard)
  #    in the same DbOp — that's the bug the manual test caught.
  run admin_call "skill" "$(jq -nc --arg pid "$project_id" --arg sid "$hello_id" '{
    command: "delete", project_id: $pid, skill_id: $sid
  }')"
  echo "$output"
  [[ "$output" == *"Skill deleted"* ]]

  run wait_for_skill_search_gone "hello-world"
  [ "$status" -eq 0 ] || { echo "hello-world still in library_documents after skill.delete"; return 1; }

  # 6. Delete the space skill via `spaces edit op=delete`. File goes
  #    out via direct GitEngine, importer's `delete_in_op` then wipes
  #    the row + search row.
  run admin_call "spaces" "$(jq -nc --arg s "$space_slug" '{
    command: "edit", slug: $s, edit_op: "delete",
    op_args: { path: "skills/goodbye-world.md" }
  }')"
  echo "$output"
  # spaces edit op=delete on a present file succeeds — assert on the
  # absence of an error string rather than positive output text.
  [[ "$output" != *"PathNotFound"* ]]
  [[ "$output" != *"error"* ]] || true

  run wait_for_skill_row_gone "goodbye-world"
  [ "$status" -eq 0 ] || { echo "goodbye-world skills row still present after spaces delete"; return 1; }
  run wait_for_skill_search_gone "goodbye-world"
  [ "$status" -eq 0 ] || { echo "goodbye-world still in library_documents after spaces delete"; return 1; }

  # 7. Re-deleting via spaces must surface PathNotFound rather than
  #    silently succeeding — guards against the regression that bit
  #    the manual test (silent no-op when path differs from canonical).
  run admin_call "spaces" "$(jq -nc --arg s "$space_slug" '{
    command: "edit", slug: $s, edit_op: "delete",
    op_args: { path: "skills/goodbye-world.md" }
  }')"
  echo "$output"
  [[ "$output" == *"PathNotFound"* ]] || [[ "$output" == *"does not exist"* ]]
}
