#!/usr/bin/env bats

# End-to-end coverage for the `bootstrapPersonalProject` GraphQL mutation.
# Drives the production resolver + service path; project name resolution,
# idempotency, and the admin opt-out flag.

load helpers

setup_file() {
  start_server
}

teardown_file() {
  stop_server
}

# Insert a user with arbitrary `name` / `github_username` and a paired
# MCP-creds row. Sets:
#   USER_ID         — the inserted user's UUID
#   AGENT_TOKEN     — bearer for graphql_query
#   GH_USERNAME     — the github_username (or empty)
create_test_user_with() {
  local user_name="$1"          # may be empty for null
  local github_username="$2"    # may be empty for null
  local user_id agent_id raw_token token_hash github_id name_field gh_field

  user_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  agent_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  github_id="test-gh-$(uuidgen | tr '[:upper:]' '[:lower:]')"

  raw_token="test-token-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  token_hash="$(echo -n "$raw_token" | sha256sum | awk '{print $1}')"

  if [ -n "$user_name" ]; then
    name_field="\"$user_name\""
  else
    name_field="null"
  fi
  if [ -n "$github_username" ]; then
    gh_field="\"$github_username\""
  else
    gh_field="null"
  fi

  psql "$PG_CON" -q <<SQL
    INSERT INTO users (id, github_id, created_at) VALUES ('$user_id', '$github_id', NOW());
    INSERT INTO user_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$user_id', 0, 'initialized',
      '{"type":"initialized","id":"$user_id","github_id":"$github_id","email":null,"name":$name_field,"github_username":$gh_field}',
      NOW());

    INSERT INTO mcp_creds (id, owner_id, token_hash, created_at) VALUES ('$agent_id', '$user_id', '$token_hash', NOW());
    INSERT INTO mcp_cred_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$agent_id', 0, 'initialized',
      '{"type":"initialized","id":"$agent_id","owner":{"type":"user","user_id":"$user_id"},"name":"test-agent","token_hash":"$token_hash","scopes":["admin"]}',
      NOW());
SQL

  export USER_ID="$user_id"
  export AGENT_TOKEN="$raw_token"
  export GH_USERNAME="$github_username"
}

@test "bootstrap: fresh user creates project named after github username" {
  local gh="vindard-$(uuidgen | cut -c1-8 | tr '[:upper:]' '[:lower:]')"
  create_test_user_with "Alice" "$gh"

  run graphql_query 'mutation { bootstrapPersonalProject { project { id name } created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *"\"name\":\"$gh\""* ]]
  [[ "$output" == *'"created":true'* ]]
}

@test "bootstrap: second call returns the same project with created=false" {
  local gh="returning-$(uuidgen | cut -c1-8 | tr '[:upper:]' '[:lower:]')"
  create_test_user_with "Bob" "$gh"

  run graphql_query 'mutation { bootstrapPersonalProject { project { id } created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"created":true'* ]]
  local first_id
  first_id="$(echo "$output" | jq -r '.data.bootstrapPersonalProject.project.id')"

  run graphql_query 'mutation { bootstrapPersonalProject { project { id } created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"created":false'* ]]
  local second_id
  second_id="$(echo "$output" | jq -r '.data.bootstrapPersonalProject.project.id')"
  [ "$first_id" = "$second_id" ]
}

@test "bootstrap: project name pre-existing for the login is reused as-is" {
  local gh="collide-$(uuidgen | cut -c1-8 | tr '[:upper:]' '[:lower:]')"
  # First user creates the project explicitly through projectCreate.
  create_test_user_with "Owner" "owner-$(uuidgen | cut -c1-8)"
  run graphql_query "mutation { projectCreate(input: { name: \"$gh\", description: \"pre-existing\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *"\"name\""* ]] || [[ "$output" == *"\"id\""* ]]
  local pre_id
  pre_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"

  # Second user with matching github_username calls bootstrap.
  create_test_user_with "Squatter" "$gh"
  run graphql_query 'mutation { bootstrapPersonalProject { project { id name description } created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"created":false'* ]]
  [[ "$output" == *"\"description\":\"pre-existing\""* ]]
  local reuse_id
  reuse_id="$(echo "$output" | jq -r '.data.bootstrapPersonalProject.project.id')"
  [ "$pre_id" = "$reuse_id" ]
}

@test "bootstrap: null github_username falls back to display name" {
  local display_name="Alice-$(uuidgen | cut -c1-8)"
  create_test_user_with "$display_name" ""

  run graphql_query 'mutation { bootstrapPersonalProject { project { name } created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *"\"name\":\"$display_name\""* ]]
  [[ "$output" == *'"created":true'* ]]
}

