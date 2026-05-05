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
