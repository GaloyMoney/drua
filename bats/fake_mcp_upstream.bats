#!/usr/bin/env bats

# E2E: drua gateway proxies upstream MCP tools via the `call_tool` meta-tool.
#
# Spins up the in-tree `fake-mcp-upstream` binary as a real HTTP MCP server
# (serving the bundled fixtures dir) and points a freshly rendered drua.yml
# at it. Each test calls a fixture through the gateway and asserts the
# upstream payload round-trips verbatim — only sub-threshold fixtures are
# covered here so the universal pipeline stays in passthrough mode.

load helpers

FAKE_UPSTREAM_BIN="${FAKE_UPSTREAM_BIN:-cargo run -q -p fake-mcp-upstream --}"
FAKE_UPSTREAM_PORT="${FAKE_UPSTREAM_PORT:-18765}"
FAKE_UPSTREAM_FIXTURES="$REPO_ROOT/lib/fake-mcp-upstream/fixtures"
FAKE_UPSTREAM_PID_FILE=""

start_fake_upstream() {
  FAKE_UPSTREAM_PID_FILE="$BATS_FILE_TMPDIR/fake-upstream.pid"
  $FAKE_UPSTREAM_BIN \
    --fixtures-dir "$FAKE_UPSTREAM_FIXTURES" \
    --bind "127.0.0.1:$FAKE_UPSTREAM_PORT" \
    --mount /mcp \
    > "$BATS_FILE_TMPDIR/fake-upstream.log" 2>&1 &
  echo "$!" > "$FAKE_UPSTREAM_PID_FILE"

  for _i in $(seq 1 60); do
    if curl -sf -o /dev/null -X POST "http://127.0.0.1:$FAKE_UPSTREAM_PORT/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'; then
      return 0
    fi
    sleep 0.5
  done

  echo "fake-mcp-upstream did not become ready on port $FAKE_UPSTREAM_PORT" >&2
  cat "$BATS_FILE_TMPDIR/fake-upstream.log" >&2
  return 1
}

stop_fake_upstream() {
  if [ -n "$FAKE_UPSTREAM_PID_FILE" ] && [ -f "$FAKE_UPSTREAM_PID_FILE" ]; then
    kill "$(cat "$FAKE_UPSTREAM_PID_FILE")" 2>/dev/null || true
    rm -f "$FAKE_UPSTREAM_PID_FILE"
  fi
}

render_drua_config_with_fake_upstream() {
  # Library init is unconditional in drua startup; seed an empty bare repo
  # so it has something valid to clone (we never read from it).
  local upstream_repo="$BATS_FILE_TMPDIR/library-upstream.git"
  local working_repo="$BATS_FILE_TMPDIR/library-working"
  mkdir -p "$working_repo"
  git init --bare --initial-branch=main "$upstream_repo" >/dev/null
  git -C "$working_repo" init -q -b main
  git -C "$working_repo" config user.email test@drua
  git -C "$working_repo" config user.name "Drua Test"
  git -C "$working_repo" commit -q --allow-empty -m "init"
  git -C "$working_repo" remote add origin "$upstream_repo"
  git -C "$working_repo" push -q origin main

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
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
library:
  data_dir: "$BATS_FILE_TMPDIR/library"
  repo_url: "file://$upstream_repo"
  skill_sync_interval_secs: 60
toolsets:
  mcp_upstreams:
    - name: fake_upstream
      url: http://127.0.0.1:$FAKE_UPSTREAM_PORT/mcp
      auth_required: false
      category: testing
      category_description: "fake-mcp-upstream fixtures for e2e tests"
EOF
  export DRUA_CONFIG="$BATS_FILE_TMPDIR/drua.yml"
}

setup_file() {
  start_fake_upstream
  render_drua_config_with_fake_upstream
  start_server
  create_test_agent
}

teardown_file() {
  stop_server
  stop_fake_upstream
}

# Dispatch an upstream tool through drua's `call_tool` meta-tool. The
# prefixed name is `<upstream_name>_<original-tool-name>`.
fake_call() {
  local prefixed="$1"
  local args="${2:-{\}}"
  local body
  body="$(jq -nc --arg t "$prefixed" --argjson a "$args" \
    '{name:"call_tool", arguments:{tool_name:$t, arguments:$a}}')"
  mcp_call "$AGENT_TOKEN" "tools/call" "$body"
}

@test "fake-upstream: obj-small round-trips verbatim (passthrough)" {
  run fake_call "fake_upstream_obj-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == '{"ok":true,"count":3}' ]]
  # passthrough → no envelope wrapping (no invocation_id field surfaces)
  [[ "$output" != *'"invocation_id"'* ]]
}

@test "fake-upstream: str-small round-trips verbatim" {
  run fake_call "fake_upstream_str-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == "hello world" ]]
}

@test "fake-upstream: arr-small round-trips verbatim" {
  run fake_call "fake_upstream_arr-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == '[{"id":1},{"id":2}]' ]]
}

@test "fake-upstream: scalar-number round-trips verbatim" {
  run fake_call "fake_upstream_scalar-number"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == "42" ]]
}

@test "fake-upstream: is-error-text propagates is_error flag" {
  run fake_call "fake_upstream_is-error-text"
  echo "$output"
  local text is_error
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  is_error="$(echo "$output" | jq -r '.result.isError')"
  [[ "$text" == "pods 'foo' not found" ]]
  [[ "$is_error" == "true" ]]
}

@test "fake-upstream: mixed-content-parts returns text + image + text" {
  run fake_call "fake_upstream_mixed-content-parts"
  echo "$output"
  local parts types
  parts="$(echo "$output" | jq -r '.result.content | length')"
  types="$(echo "$output" | jq -r '.result.content | map(.type) | join(",")')"
  [[ "$parts" == "3" ]]
  [[ "$types" == "text,image,text" ]]
}
