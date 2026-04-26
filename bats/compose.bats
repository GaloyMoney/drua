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
