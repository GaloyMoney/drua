#!/usr/bin/env bats

load helpers

setup_file() {
  start_server
}

teardown_file() {
  stop_server
}

@test "graphql: ping query returns pong" {
  run graphql_query "{ ping }"
  echo "$output"
  [[ "$output" == *'"ping":"pong"'* ]]
}

@test "graphql: me query returns null when unauthenticated" {
  run graphql_query "{ me }"
  echo "$output"
  [[ "$output" == *'"me":null'* ]]
}

@test "graphql: me query returns user id when authenticated" {
  create_test_agent

  run graphql_query "{ me }" "$AGENT_TOKEN"
  echo "$output"
  # Authenticated via MCP creds — me should return a UUID
  [[ "$output" == *'"me":"'* ]]
  # Should not be null
  [[ "$output" != *'"me":null'* ]]
}

@test "graphql: ping mutation returns pong" {
  run graphql_query "mutation { ping }"
  echo "$output"
  [[ "$output" == *'"ping":"pong"'* ]]
}
