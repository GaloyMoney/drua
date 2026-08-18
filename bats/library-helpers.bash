# Shared bats helpers for e2e tests that need an isolated library
# upstream. Tests that load this on top of `helpers.bash` get:
#
# - `setup_isolated_library`: seeds a bare git repo + working clone
#   in `$BATS_FILE_TMPDIR`, renders a per-file `drua.yml` pointing
#   the library service at the bare repo, and starts the drua
#   server. Each test file gets its own isolated upstream — no
#   network dependency, no cross-run state. Tests that need an
#   admin token call `create_test_agent` themselves (per-test or
#   in `setup_file` after this call).
# - `reset_library_tables`: wipes the rows that linger between
#   test runs when `SKIP_DEPS=1` is set (developer iteration
#   against a persisted PG). Always wipes `jobs` + `library_documents`;
#   callers pass entity-specific table names as extra args.
# - `admin_call`: dispatches a `drua_admin_<tool>` searchable tool
#   through the top-level `call_tool` meta-tool envelope.
#
# Tests are expected to call `reset_library_tables …` BEFORE
# `setup_isolated_library` (otherwise the cleanup runs against an
# already-rebooted server's state).

# Seeds a bare git repo + working clone, renders the standard
# test-config `drua.yml`, starts the server, and mints a test agent.
#
# Positional args are extra directories to seed under the working
# repo root before the initial commit. Each one gets a `.gitkeep`
# so the bare repo's HEAD has a tree the importers can diff against.
# Without args the working repo has just the default git scaffolding;
# pass e.g. `spaces` for tests that exercise the spaces tool, or
# `runtime/workflows` for tests that exercise the workflow importer.
setup_isolated_library() {
  UPSTREAM_REPO="$BATS_FILE_TMPDIR/upstream.git"
  WORKING_REPO="$BATS_FILE_TMPDIR/upstream-working"
  mkdir -p "$WORKING_REPO"
  git init --bare --initial-branch=main "$UPSTREAM_REPO" >/dev/null
  git -C "$WORKING_REPO" init -q -b main
  git -C "$WORKING_REPO" config user.email test@drua
  git -C "$WORKING_REPO" config user.name "Drua Test"
  for seed_dir in "$@"; do
    mkdir -p "$WORKING_REPO/$seed_dir"
    : > "$WORKING_REPO/$seed_dir/.gitkeep"
  done
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
  default_chain:
    primary: { name: test-model }
  builtin_roles:
    project_lead:
      compaction:
        prune_after_seconds: 600
    agent:
      compaction:
        prune_after_seconds: 600
    workflow_step_agent:
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

# Reset the rows that linger between bats runs when `SKIP_DEPS=1`
# is set (developer iteration). Wipes `jobs` + `library_documents`
# unconditionally — every library-using test needs them clean — plus
# every additional table name passed as an extra arg.
#
# Each extra is `DELETE FROM <table>` so call sites can list both
# the entity table and its events table (e.g.
# `reset_library_tables skill_events skills`).
reset_library_tables() {
  if [ "${SKIP_DEPS:-0}" != "1" ]; then return 0; fi
  local pg="${PG_CON:-postgres://user:password@localhost:5432/drua}"
  local -a cmd=(
    -q
    -c "TRUNCATE job_events, job_executions CASCADE;"
    -c "DELETE FROM jobs;"
    -c "DELETE FROM library_documents;"
  )
  local table
  for table in "$@"; do
    cmd+=(-c "DELETE FROM $table;")
  done
  psql "$pg" "${cmd[@]}" > /dev/null 2>&1 || true
}

# Dispatch a `drua_admin_<tool>` searchable tool through the
# top-level `call_tool` meta-tool envelope. Same shape every
# admin-tier test uses.
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
