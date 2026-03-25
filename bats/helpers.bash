REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)"
GALOY_AGENTS_BIN="${GALOY_AGENTS_BIN:-cargo run --bin galoy-agents --}"
SERVER_PID_FILE="$BATS_FILE_TMPDIR/server.pid"
PG_CON="${PG_CON:-postgres://user:password@localhost:5432/galoy_agents}"

COMPOSE_CMD="${COMPOSE_CMD:-docker compose}"

start_server() {
  # Clean up any leftover containers
  $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true

  # Start postgres
  $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" up -d

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
  export GALOY_AGENTS_CONFIG="$REPO_ROOT/galoy-agents.yml"
  export STYLE_AGENT_DB_PATH=""  # disable style-agent in tests

  $GALOY_AGENTS_BIN > "$BATS_FILE_TMPDIR/server.log" 2>&1 &
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
  $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
}

# Create a test user and agent via psql (test fixture setup).
# The server runs migrations on startup, so the schema is ready.
# Sets AGENT_TOKEN for use in subsequent test calls.
create_test_agent() {
  local user_id agent_id raw_token token_hash

  user_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  agent_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"

  raw_token="test-token-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  token_hash="$(echo -n "$raw_token" | sha256sum | awk '{print $1}')"

  psql "$PG_CON" -q <<SQL
    INSERT INTO users (id, github_id, created_at) VALUES ('$user_id', 'test-gh-user', NOW());
    INSERT INTO user_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$user_id', 0, 'initialized',
      '{"type":"initialized","id":"$user_id","github_id":"test-gh-user","email":null,"name":"Test User"}',
      NOW());

    INSERT INTO agents (id, user_id, token_hash, created_at) VALUES ('$agent_id', '$user_id', '$token_hash', NOW());
    INSERT INTO agent_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$agent_id', 0, 'initialized',
      '{"type":"initialized","id":"$agent_id","user_id":"$user_id","name":"test-agent","token_hash":"$token_hash","scopes":[]}',
      NOW());
SQL

  export AGENT_TOKEN="$raw_token"
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
