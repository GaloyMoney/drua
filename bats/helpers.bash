REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)"
DRUA_BIN="${DRUA_BIN:-cargo run --bin drua --}"
SERVER_PID_FILE="$BATS_FILE_TMPDIR/server.pid"
PG_CON="${PG_CON:-postgres://user:password@localhost:5432/drua}"

COMPOSE_CMD="${COMPOSE_CMD:-docker compose}"

start_server() {
  # Skip the compose dance when the caller is bringing its own
  # already-running PG (developer iteration, CI sidecar). Set
  # `SKIP_COMPOSE=1` to opt in.
  if [ "${SKIP_COMPOSE:-0}" != "1" ]; then
    # Clean up any leftover containers
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true

    # Start postgres
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" up -d
  fi

  # Wait for postgres
  for _i in $(seq 1 30); do
    if psql "$PG_CON" -c "SELECT 1" > /dev/null 2>&1; then
      break
    fi
    sleep 0.5
  done

  # Start the server
  export PG_CON
  export GITHUB_CLIENT_ID="test-client-id"
  export GITHUB_CLIENT_SECRET="test-client-secret"
  # Respect a pre-set DRUA_CONFIG so tests can render an isolated
  # config (e.g. a fake on-disk library) before calling start_server.
  export DRUA_CONFIG="${DRUA_CONFIG:-$REPO_ROOT/drua.yml}"
  export CODE_ASSISTANT_DB_PATH=""  # disable code assistant in tests

  $DRUA_BIN server > "$BATS_FILE_TMPDIR/server.log" 2>&1 &
  echo "$!" > "$SERVER_PID_FILE"

  # Wait for server
  for _i in $(seq 1 30); do
    if curl -s -o /dev/null http://localhost:4200/; then
      break
    fi
    sleep 0.5
  done
}

stop_server() {
  if [ -f "$SERVER_PID_FILE" ]; then
    kill "$(cat "$SERVER_PID_FILE")" 2>/dev/null || true
    rm -f "$SERVER_PID_FILE"
  fi
  if [ "${SKIP_COMPOSE:-0}" != "1" ]; then
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
  fi
}

# Create a test user and agent via psql (test fixture setup).
# The server runs migrations on startup, so the schema is ready.
# Sets AGENT_TOKEN for use in subsequent test calls.
create_test_agent() {
  local user_id agent_id raw_token token_hash github_id

  user_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  agent_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  github_id="test-gh-user-$(uuidgen | tr '[:upper:]' '[:lower:]')"

  raw_token="test-token-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  token_hash="$(echo -n "$raw_token" | sha256sum | awk '{print $1}')"

  psql "$PG_CON" -q <<SQL
    INSERT INTO users (id, github_id, created_at) VALUES ('$user_id', '$github_id', NOW());
    INSERT INTO user_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$user_id', 0, 'initialized',
      '{"type":"initialized","id":"$user_id","github_id":"$github_id","email":null,"name":"Test User"}',
      NOW());

    INSERT INTO mcp_creds (id, owner_id, token_hash, created_at) VALUES ('$agent_id', '$user_id', '$token_hash', NOW());
    INSERT INTO mcp_cred_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$agent_id', 0, 'initialized',
      '{"type":"initialized","id":"$agent_id","owner":{"type":"user","user_id":"$user_id"},"name":"test-agent","token_hash":"$token_hash","scopes":["admin"]}',
      NOW());
SQL

  export AGENT_TOKEN="$raw_token"
}

# Generates fresh project + agent UUIDs without touching PG. Call
# this BEFORE `write_git_proxy_config` so the YAML allowlist binds to
# the same project_id we'll later seed.
gen_test_project_agent_ids() {
  export PROJECT_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  export AGENT_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  export PROJECT_NAME="bats-${PROJECT_ID:0:8}"
  export GIT_PROXY_TOKEN="dev-agent:$AGENT_ID"
}

# Persists the project + lead-agent SQL rows. Must run AFTER the
# server is up (which brings up PG + applies migrations) and AFTER
# `gen_test_project_agent_ids`.
seed_test_project_agent() {
  psql "$PG_CON" -q <<SQL
    INSERT INTO projects (id, name, created_at) VALUES ('$PROJECT_ID', '$PROJECT_NAME', NOW());
    INSERT INTO project_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$PROJECT_ID', 0, 'initialized',
      '{"type":"initialized","id":"$PROJECT_ID","lead_agent_id":"$AGENT_ID","name":"$PROJECT_NAME","description":null}',
      NOW());

    INSERT INTO agents (id, project_id, created_at) VALUES ('$AGENT_ID', '$PROJECT_ID', NOW());
    INSERT INTO agent_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$AGENT_ID', 0, 'initialized',
      '{"type":"initialized","id":"$AGENT_ID","project_id":"$PROJECT_ID","agent_role":"project_lead","name":"lead","authz_scopes":["project:$PROJECT_ID:admin"],"project_name":"$PROJECT_NAME"}',
      NOW());
SQL
}

# Renders an isolated drua.yml with:
#   * dev login + dev_mode_agent_tokens enabled
#   * one git_proxy.allowlist entry per arg in the form
#     "<owner>/<repo>:<modes>:<patterns>" — modes csv (pull,push),
#     patterns csv. e.g. "GaloyMoney/drua:pull,push:refs/heads/bot/*,refs/heads/main"
#   * project_id pinned to the current $PROJECT_ID
write_git_proxy_config() {
  local out="$BATS_FILE_TMPDIR/drua.yml"

  cat > "$out" <<EOF
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
  builtin_roles:
    project_lead:
      compaction:
        prune_after_seconds: 600
    agent:
      compaction:
        prune_after_seconds: 600
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
library:
  repo_url: "https://github.com/galoymoney/drua-test-library"
  skill_sync_interval_secs: 3600
git_proxy:
  allowlist:
EOF

  if [ "$#" -gt 0 ]; then
    echo "    entries:" >> "$out"
    for spec in "$@"; do
      local owner_repo modes_patterns modes patterns owner repo_name
      owner_repo="${spec%%:*}"
      modes_patterns="${spec#*:}"
      modes="${modes_patterns%%:*}"
      patterns="${modes_patterns#*:}"
      owner="${owner_repo%%/*}"
      repo_name="${owner_repo#*/}"
      echo "      - project_id: $PROJECT_ID" >> "$out"
      echo "        owner: $owner" >> "$out"
      echo "        repo: $repo_name" >> "$out"
      echo "        modes:" >> "$out"
      IFS=',' read -ra _modes <<< "$modes"
      for m in "${_modes[@]}"; do
        echo "          - $m" >> "$out"
      done
      echo "        allowed_ref_patterns:" >> "$out"
      IFS=',' read -ra _pats <<< "$patterns"
      for p in "${_pats[@]}"; do
        echo "          - \"$p\"" >> "$out"
      done
    done
  else
    echo "    entries: []" >> "$out"
  fi

  export DRUA_CONFIG="$out"
}

mcp_call() {
  local token="$1"
  local method="$2"
  local params="$3"

  curl -s -X POST http://localhost:4200/mcp \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -H "Authorization: Bearer $token" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

mcp_call_no_auth() {
  local method="$1"
  local params="$2"

  curl -s -X POST http://localhost:4200/mcp \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

graphql_query() {
  local query="$1"
  local token="${2:-}"

  local -a headers=(-H "Content-Type: application/json")
  if [ -n "$token" ]; then
    headers+=(-H "Authorization: Bearer $token")
  fi

  # Use jq to properly escape the query string inside the JSON payload.
  local body
  body="$(jq -n --arg q "$query" '{query: $q}')"

  curl -s -X POST http://localhost:4200/graphql \
    "${headers[@]}" \
    -d "$body"
}
