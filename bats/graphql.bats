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
  run graphql_query "{ me { id } }"
  echo "$output"
  [[ "$output" == *'"me":null'* ]]
}

@test "graphql: me query returns user id when authenticated" {
  create_test_agent

  run graphql_query "{ me { id githubUsername } }" "$AGENT_TOKEN"
  echo "$output"
  # Authenticated via MCP creds — me should return an object with id
  [[ "$output" == *'"id"'* ]]
  # Should not be null
  [[ "$output" != *'"me":null'* ]]
}

@test "graphql: ping mutation returns pong" {
  run graphql_query "mutation { ping }"
  echo "$output"
  [[ "$output" == *'"ping":"pong"'* ]]
}

@test "graphql: workspace CRUD lifecycle" {
  create_test_agent

  # Create a workspace
  run graphql_query 'mutation { workspaceCreate(input: { name: "test-ws", description: "A test workspace" }) { workspace { id name description createdAt } } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"test-ws"'* ]]
  [[ "$output" == *'"description":"A test workspace"'* ]]
  [[ "$output" == *'"createdAt"'* ]]

  # Extract workspace id
  ws_id="$(echo "$output" | jq -r '.data.workspaceCreate.workspace.id')"
  echo "workspace id: $ws_id"

  # Query single workspace with lead agent
  run graphql_query "{ workspace(id: \"$ws_id\") { id name description lead { id name role } } }"
  echo "$output"
  [[ "$output" == *'"name":"test-ws"'* ]]
  [[ "$output" == *'"role":"WORKSPACE_LEAD"'* ]]

  # List workspaces (cursor-based)
  run graphql_query "{ workspaces(first: 10) { edges { node { id name } } pageInfo { hasNextPage } } }"
  echo "$output"
  [[ "$output" == *'"name":"test-ws"'* ]]
  [[ "$output" == *'"hasNextPage"'* ]]

  # Update workspace
  run graphql_query "mutation { workspaceUpdate(input: { id: \"$ws_id\", name: \"renamed-ws\", description: \"updated\" }) { workspace { name description } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"renamed-ws"'* ]]
  [[ "$output" == *'"description":"updated"'* ]]

  # Delete workspace
  run graphql_query "mutation { workspaceDelete(input: { id: \"$ws_id\" }) { workspace { id } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"id"'* ]]

  # After deletion, workspace should not appear in list
  run graphql_query "{ workspaces(first: 100) { edges { node { id } } } }"
  echo "$output"
  [[ "$output" != *"$ws_id"* ]]
}

@test "graphql: workspace query returns null for unknown id" {
  run graphql_query '{ workspace(id: "00000000-0000-0000-0000-000000000000") { id } }'
  echo "$output"
  [[ "$output" == *'"workspace":null'* ]]
}
