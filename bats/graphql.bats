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

@test "graphql: project CRUD lifecycle" {
  create_test_agent

  # Create a project
  run graphql_query 'mutation { projectCreate(input: { name: "test-proj", description: "A test project" }) { project { id name description createdAt } } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"test-proj"'* ]]
  [[ "$output" == *'"description":"A test project"'* ]]
  [[ "$output" == *'"createdAt"'* ]]

  # Extract project id
  ws_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  echo "project id: $ws_id"

  # Query single project with lead agent
  run graphql_query "{ project(id: \"$ws_id\") { id name description lead { id name role } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"test-proj"'* ]]
  [[ "$output" == *'"role":"PROJECT_LEAD"'* ]]

  # List projects (cursor-based)
  run graphql_query "{ projects(first: 10) { edges { node { id name } } pageInfo { hasNextPage } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"test-proj"'* ]]
  [[ "$output" == *'"hasNextPage"'* ]]

  # Update project (name is immutable, only description can be changed)
  run graphql_query "mutation { projectUpdate(input: { id: \"$ws_id\", description: \"updated\" }) { project { name description } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"name":"test-proj"'* ]]
  [[ "$output" == *'"description":"updated"'* ]]

  # Delete project
  run graphql_query "mutation { projectDelete(input: { id: \"$ws_id\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"id"'* ]]

  # After deletion, project should not appear in list
  run graphql_query "{ projects(first: 100) { edges { node { id } } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" != *"$ws_id"* ]]
}

@test "graphql: project query returns null for unknown id" {
  create_test_agent

  run graphql_query '{ project(id: "00000000-0000-0000-0000-000000000000") { id } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"project":null'* ]]
}
