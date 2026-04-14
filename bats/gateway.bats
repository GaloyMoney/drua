#!/usr/bin/env bats

load helpers

setup_file() {
  start_server
}

teardown_file() {
  stop_server
}

@test "mcp: unauthenticated tool call returns auth error" {
  run mcp_call_no_auth "tools/call" '{"name":"ping","arguments":{}}'
  echo "$output"
  [[ "$output" == *"Authentication required"* ]]
}

@test "mcp: authenticated tool call succeeds" {
  create_test_agent

  run mcp_call "$AGENT_TOKEN" "tools/call" '{"name":"ping","arguments":{}}'
  echo "$output"
  [[ "$output" == *"pong"* ]]
}

@test "mcp: invalid token returns auth error" {
  run mcp_call "invalid-token-value" "tools/call" '{"name":"ping","arguments":{}}'
  echo "$output"
  [[ "$output" == *"Authentication required"* ]]
}
