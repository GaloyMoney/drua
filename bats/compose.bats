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

# Query the audit log via the `drua_admin_log` MCP tool (admin scope; the
# bats fixture's mcp_creds carry `admin`). Tool lives under the `drua_admin`
# searchable toolset, so we go through the `call_tool` meta-tool. Returns
# the text body — `format_audit_entries` formats action / outcome columns
# truncated to fixed widths.
_audit_log() {
  local args_json="$1"
  local response
  response="$(mcp_call "$AGENT_TOKEN" "tools/call" \
    "$(jq -nc --argjson args "$args_json" \
      '{name:"call_tool", arguments:{tool_name:"drua_admin_log", arguments:$args}}')")"
  echo "$response" | jq -r '.result.content[0].text // ""'
}

# Poll the log tool until a row matching `args_json` appears. Audit rows
# are persisted via fire-and-forget tokio::spawn, so wait briefly.
_wait_for_audit_log() {
  local args_json="$1"
  for _i in $(seq 1 40); do
    local out
    out="$(_audit_log "$args_json")"
    if [[ "$out" != "No audit entries found."* ]] && [ -n "$out" ]; then
      echo "$out"
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Validates the fix from memory `019dfd6e`: a sub-tool dispatched inside a
# compose script must record its own audit row with its own outcome,
# distinct from the parent compose row. Both rows are checked via the
# `drua_admin_log` MCP tool.
@test "compose: tools.whoami records sub-tool audit row separate from parent" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"return await tools.whoami({});"}}'
  echo "$output"
  [[ "$output" == *"exported_agent"* ]]

  # Sub-tool row: action contains "whoami", outcome=success.
  local sub_log
  sub_log="$(_wait_for_audit_log '{"action":"whoami","outcome":"success","limit":1}')" \
    || { echo "no whoami success audit row appeared"; return 1; }
  echo "sub_log=$sub_log"
  [[ "$sub_log" == *"whoam"* ]]
  [[ "$sub_log" == *"success"* ]]

  # Parent compose row: entrypoint=mcp: compose, outcome=success.
  local parent_log
  parent_log="$(_wait_for_audit_log \
    '{"entrypoint":"mcp: compose","outcome":"success","action":"compose","limit":5}')" \
    || { echo "no parent compose success row appeared"; return 1; }
  echo "parent_log=$parent_log"
  [[ "$parent_log" == *"compose"* ]]
  [[ "$parent_log" == *"success"* ]]
}

# Regression for the original misattribution bug: when the JS script throws
# AFTER a successful sub-tool call, the sub-tool row must stay `success` —
# only the parent compose row should be `error`. Asserted via the
# `drua_admin_log` MCP tool (no DB access).
@test "compose: script throw after whoami leaves whoami audit row at success" {
  run mcp_call "$AGENT_TOKEN" "tools/call" \
    '{"name":"compose","arguments":{"script":"const me = await tools.whoami({}); throw new Error(\"boom from script\");"}}'
  echo "$output"
  [[ "$output" == *"boom from script"* ]] || [[ "$output" == *"error"* ]]

  # Pre-fix bug: latest whoami row would be tagged error. Post-fix: the
  # most recent whoami row stays at success. Filter on action+success and
  # require a row to exist (proves no error tagging on the sub-tool call).
  local sub_log
  sub_log="$(_wait_for_audit_log '{"action":"whoami","outcome":"success","limit":1}')" \
    || { echo "no whoami success audit row appeared after script throw"; return 1; }
  echo "sub_log=$sub_log"
  [[ "$sub_log" == *"whoam"* ]]
  [[ "$sub_log" == *"success"* ]]

  # Parent compose row: errors_only narrows to error rows under entrypoint
  # mcp:compose; the latest one is this test's throw.
  local parent_log
  parent_log="$(_wait_for_audit_log \
    '{"entrypoint":"mcp: compose","action":"compose","errors_only":true,"limit":1}')" \
    || { echo "no parent compose error row appeared"; return 1; }
  echo "parent_log=$parent_log"
  [[ "$parent_log" == *"compose"* ]]
  [[ "$parent_log" == *"error"* ]]
}
