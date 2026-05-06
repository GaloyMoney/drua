#!/usr/bin/env bats

load helpers

setup_file() {
  start_server
  create_test_agent
}

teardown_file() {
  stop_server
}

@test "compose: simple arithmetic returns result" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"return 1 + 2;"}}'
  echo "$output"
  [[ "$output" == *'"result"'* ]]
  [[ "$output" == *"3"* ]]
  [[ "$output" != *'"isError"'* ]] || [[ "$output" != *'"isError":true'* ]]
}

@test "compose: returns object value" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"return { greeting: \"hello\", n: 42 };"}}'
  echo "$output"
  [[ "$output" == *"hello"* ]]
  [[ "$output" == *"42"* ]]
}

@test "compose: console.log output captured" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"console.log(\"ping from compose\"); return \"done\";"}}'
  echo "$output"
  [[ "$output" == *"ping from compose"* ]]
  [[ "$output" == *"done"* ]]
}

@test "compose: top-level await works" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"const p = Promise.resolve(99); const v = await p; return v;"}}'
  echo "$output"
  [[ "$output" == *"99"* ]]
}

@test "compose: script error returns error" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"throw new Error(\"boom\");"}}'
  echo "$output"
  [[ "$output" == *"error"* ]] || [[ "$output" == *"boom"* ]]
}

@test "compose: missing script argument returns error" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{}}'
  echo "$output"
  [[ "$output" == *"error"* ]] || [[ "$output" == *"script"* ]]
}

@test "compose: tool call to nonexistent tool returns error in script" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"try { await tools.nonexistent.fake_tool({}); return \"unreachable\"; } catch(e) { return { error: e.message }; }"}}'
  echo "$output"
  # The script should catch the tool-not-found error and return it
  [[ "$output" == *"error"* ]]
  [[ "$output" != *"unreachable"* ]]
}

@test "compose: metadata section present in output" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"return null;"}}'
  echo "$output"
  [[ "$output" == *"tool_calls"* ]]
  [[ "$output" == *"execution_time"* ]]
}

# Wait for `audit_entries` to contain a row whose `action` matches a SQL LIKE
# pattern recorded at-or-after `since_id`. Audit rows are persisted via a
# fire-and-forget tokio::spawn, so we poll briefly.
_wait_for_audit_action() {
  local action_like="$1"
  local since_id="$2"
  for _i in $(seq 1 40); do
    local count
    count="$(psql "$PG_CON" -tAc \
      "SELECT count(*) FROM audit_entries WHERE id > $since_id AND action LIKE '$action_like'")"
    if [ "${count:-0}" -gt 0 ]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Query the outcome column for a single audit row matching `action_like`,
# recorded after `since_id`. Returns "success" or "error".
_audit_outcome() {
  local action_like="$1"
  local since_id="$2"
  psql "$PG_CON" -tAc \
    "SELECT outcome FROM audit_entries
     WHERE id > $since_id AND action LIKE '$action_like'
     ORDER BY id DESC LIMIT 1"
}

# Validates the fix from memory `019dfd6e`: a sub-tool dispatched inside a
# compose script must record its own audit row with its own outcome,
# distinct from the parent compose row.
@test "compose: tools.whoami records sub-tool audit row separate from parent" {
  local since_id
  since_id="$(psql "$PG_CON" -tAc 'SELECT COALESCE(MAX(id), 0) FROM audit_entries')"

  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"return await tools.whoami({});"}}'
  echo "$output"
  [[ "$output" == *"exported_agent"* ]]

  _wait_for_audit_action 'compose > top_level: whoami' "$since_id" \
    || { echo "no whoami audit row appeared"; return 1; }
  _wait_for_audit_action 'compose' "$since_id" \
    || { echo "no parent compose audit row appeared"; return 1; }

  local sub_outcome parent_outcome
  sub_outcome="$(_audit_outcome 'compose > top_level: whoami' "$since_id")"
  parent_outcome="$(_audit_outcome 'compose' "$since_id")"
  echo "sub_outcome=$sub_outcome parent_outcome=$parent_outcome"
  [ "$sub_outcome" = "success" ]
  [ "$parent_outcome" = "success" ]
}

# Regression for the original misattribution bug: when the JS script throws
# AFTER a successful sub-tool call, the sub-tool row must stay `success` —
# only the parent compose row should be `error`.
@test "compose: script throw after whoami leaves whoami audit row at success" {
  local since_id
  since_id="$(psql "$PG_CON" -tAc 'SELECT COALESCE(MAX(id), 0) FROM audit_entries')"

  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"const me = await tools.whoami({}); throw new Error(\"boom from script\");"}}'
  echo "$output"
  [[ "$output" == *"boom from script"* ]] || [[ "$output" == *"error"* ]]

  _wait_for_audit_action 'compose > top_level: whoami' "$since_id" \
    || { echo "no whoami audit row appeared"; return 1; }
  _wait_for_audit_action 'compose' "$since_id" \
    || { echo "no parent compose audit row appeared"; return 1; }

  local sub_outcome parent_outcome parent_err
  sub_outcome="$(_audit_outcome 'compose > top_level: whoami' "$since_id")"
  parent_outcome="$(_audit_outcome 'compose' "$since_id")"
  parent_err="$(psql "$PG_CON" -tAc \
    "SELECT error_message FROM audit_entries
     WHERE id > $since_id AND action = 'compose'
     ORDER BY id DESC LIMIT 1")"
  echo "sub_outcome=$sub_outcome parent_outcome=$parent_outcome parent_err=$parent_err"
  [ "$sub_outcome" = "success" ]
  [ "$parent_outcome" = "error" ]
  [[ "$parent_err" == *"boom from script"* ]]
}
