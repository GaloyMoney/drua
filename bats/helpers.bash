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

# Renders an isolated bare upstream repo at $BATS_FILE_TMPDIR/<owner>-<repo>.git
# with one commit on `main`. Returns its `file://` URL via stdout so
# the caller can plug it into write_git_proxy_config's upstream override.
mk_upstream_repo() {
  local owner="$1" repo="$2"
  local work="$BATS_FILE_TMPDIR/upstream/${owner}-${repo}"
  local bare="$BATS_FILE_TMPDIR/upstream/${owner}-${repo}.git"
  rm -rf "$work" "$bare"
  mkdir -p "$work"
  git -C "$work" init -q -b main
  git -C "$work" config user.email "bats@example.com"
  git -C "$work" config user.name "bats"
  echo "# fixture" > "$work/README.md"
  git -C "$work" add README.md
  git -C "$work" commit -q -m "fixture initial"
  git clone -q --bare "$work" "$bare" 2>/dev/null
  # Pre-receive hook bare repos default to denyCurrentBranch=refuse
  # which breaks pushes to the checked-out branch on the upstream.
  # Bare clones don't have a checked-out branch so this is a no-op,
  # but set it explicitly for clarity.
  git -C "$bare" config receive.denyCurrentBranch ignore
  echo "file://$bare"
}

# Renders an isolated drua.yml with:
#   * dev login + dev_mode_agent_tokens enabled (accepts the literal
#     `dev-agent` Bearer token; no agent row needs to exist in PG)
#   * one git_proxy.allowlist entry per arg in the form
#     "<owner>/<repo>:<modes>:<patterns>[:<upstream_url>]"
#     — modes csv (pull,push), patterns csv. e.g.
#       "GaloyMoney/drua:pull,push:refs/heads/bot/*,refs/heads/main"
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
    workflow_step_agent:
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
  mirror_root: "${BATS_FILE_TMPDIR}/git-proxy-mirrors"
  allowlist:
EOF

  if [ "$#" -gt 0 ]; then
    echo "    entries:" >> "$out"
    for spec in "$@"; do
      # spec format: <owner>/<repo>:<modes>:<patterns>[:<upstream_url>]
      # split on : but keep only the first 3 splits — the 4th may itself contain ':' (file://).
      local owner_repo modes patterns upstream_url remainder
      owner_repo="${spec%%:*}"
      remainder="${spec#*:}"
      modes="${remainder%%:*}"
      remainder="${remainder#*:}"
      # `patterns` is everything until next `:` OR end if no upstream_url
      if [[ "$remainder" == *":"* ]]; then
        patterns="${remainder%%:*}"
        upstream_url="${remainder#*:}"
      else
        patterns="$remainder"
        upstream_url=""
      fi
      local owner="${owner_repo%%/*}"
      local repo_name="${owner_repo#*/}"
      echo "      - owner: $owner" >> "$out"
      echo "        repo: $repo_name" >> "$out"
      if [ -n "$upstream_url" ]; then
        echo "        upstream_url: \"$upstream_url\"" >> "$out"
      fi
      echo "        modes:" >> "$out"
      IFS=',' read -ra _modes <<< "$modes"
      for m in "${_modes[@]}"; do
        echo "          - $m" >> "$out"
      done
      echo "        allowed_push_refs:" >> "$out"
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

# Poll `audit_entries` until a row matching the given WHERE clause appears.
# Echoes the final count on stdout. Audit persistence is fire-and-forget
# (`tokio::spawn`) so the row may arrive a few ms after the HTTP response —
# wait up to ~3s to absorb CI scheduler jitter.
wait_for_audit_count() {
  local where="$1"
  local count=0
  for _i in $(seq 1 30); do
    count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM audit_entries WHERE $where")"
    if [ "$count" -ge 1 ]; then
      break
    fi
    sleep 0.1
  done
  echo "$count"
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
