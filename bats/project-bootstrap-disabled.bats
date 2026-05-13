#!/usr/bin/env bats

# Standalone bats file so the server boots with
# `auto_bootstrap_personal_project: false` from the start.
#
# Scenarios 1–4 of the bootstrap suite live in `project-bootstrap.bats`
# and share a single server instance via that file's `setup_file`.
# Scenario 5 needs a different config — an in-test
# `stop_server` / `start_server` restart was tried first and proved
# racy in CI: `cargo run`'s child drua process can survive its parent
# kill, keep port 4200 bound, and short-circuit the replacement's
# readiness curl. Splitting the scenario into its own file means
# bats brings up a fresh server in `setup_file` with the desired
# `DRUA_CONFIG` already set, and tears it down once when done.

load helpers

setup_file() {
  DISABLED_CONFIG="$BATS_FILE_TMPDIR/drua-disabled.yml"
  cat > "$DISABLED_CONFIG" <<EOF
server:
  port: 4200
  host: "0.0.0.0"
  secure_cookies: false
  mcp_endpoint: "http://localhost:4200/mcp"
oauth:
  login: dev
  dev_mode_agent_tokens: true
  github_redirect_uri: "http://localhost:4200/auth/github/callback"
  github_client_id: "bats"
  github_allowed_teams: []
agents:
  models:
    bats-test-model:
      model: bats-test-model
      max_tokens_per_response: 1024
      context_window_tokens: 4096
  default_chain:
    primary: { name: "bats-test-model", max_tokens: 1024 }
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
library:
  repo_url: "https://github.com/galoymoney/drua-test-library"
  skill_sync_interval_secs: 3600
auto_bootstrap_personal_project: false
EOF
  export DRUA_CONFIG="$DISABLED_CONFIG"
  start_server
}

teardown_file() {
  stop_server
}

# Inserts a user + paired MCP-creds row. Same shape as the helper in
# `project-bootstrap.bats`. Duplicated rather than promoted to
# `helpers.bash` because it's only useful for the bootstrap tests.
create_test_user_with() {
  local user_name="$1"
  local github_username="$2"
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

  export AGENT_TOKEN="$raw_token"
}

@test "bootstrap: disabled by config raises an error" {
  create_test_user_with "Disabled" "disabled-$(uuidgen | cut -c1-8)"
  run graphql_query 'mutation { bootstrapPersonalProject { created } }' "$AGENT_TOKEN"
  echo "$output"
  [[ "$output" == *'"errors"'* ]]
  [[ "$output" == *'disabled'* ]]
}
