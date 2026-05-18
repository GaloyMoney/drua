setup_tunnel_ha_file() {
  RUNTIME_A_PORT="$(pick_free_port 4300)"
  RUNTIME_B_PORT="$(pick_free_port "$((RUNTIME_A_PORT + 1))")"

  start_postgres
  seed_library_repo
  write_tunnel_keys

  RUN_PREFIX="ha$(uuidgen | tr '[:upper:]' '[:lower:]' | tr -d '-' | cut -c1-8)"
  {
    echo "RUN_PREFIX=$RUN_PREFIX"
    echo "RUNTIME_A_PORT=$RUNTIME_A_PORT"
    echo "RUNTIME_B_PORT=$RUNTIME_B_PORT"
    echo "AGENT_TOKEN="
  } > "$BATS_FILE_TMPDIR/run.env"

  render_runtime_config "$RUNTIME_A_PORT" "$BATS_FILE_TMPDIR/drua-a.yml"
  render_runtime_config "$RUNTIME_B_PORT" "$BATS_FILE_TMPDIR/drua-b.yml"

  start_runtime "a" "$RUNTIME_A_PORT" "$BATS_FILE_TMPDIR/drua-a.yml"
  start_runtime "b" "$RUNTIME_B_PORT" "$BATS_FILE_TMPDIR/drua-b.yml"

  create_test_agent
  sed -i "s/^AGENT_TOKEN=.*/AGENT_TOKEN=$AGENT_TOKEN/" "$BATS_FILE_TMPDIR/run.env"
}

teardown_tunnel_ha_file() {
  [ -f "$BATS_FILE_TMPDIR/run.env" ] && source "$BATS_FILE_TMPDIR/run.env"

  stop_connectors
  stop_runtime "a"
  stop_runtime "b"

  if [ -f "$BATS_FILE_TMPDIR/started-compose" ]; then
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
  fi
}

pick_free_port() {
  local start="$1"
  local end="$((start + 200))"
  local port

  for port in $(seq "$start" "$end"); do
    if ! (echo > "/dev/tcp/$RUNTIME_HOST/$port") >/dev/null 2>&1; then
      echo "$port"
      return 0
    fi
  done

  echo "could not find a free runtime port from $start to $end" >&2
  return 1
}

start_postgres() {
  if psql "$PG_CON" -c "SELECT 1" > /dev/null 2>&1; then
    return 0
  fi

  if [ "${SKIP_COMPOSE:-0}" != "1" ]; then
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
    $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" up -d postgres
    touch "$BATS_FILE_TMPDIR/started-compose"
  fi

  for _i in $(seq 1 60); do
    if psql "$PG_CON" -c "SELECT 1" > /dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done

  echo "Postgres did not become ready" >&2
  return 1
}

seed_library_repo() {
  UPSTREAM_REPO="$BATS_FILE_TMPDIR/library-upstream.git"
  WORKING_REPO="$BATS_FILE_TMPDIR/library-working"

  git init -q --bare "$UPSTREAM_REPO"
  git init -q -b main "$WORKING_REPO"
  git -C "$WORKING_REPO" config user.email "bats@example.test"
  git -C "$WORKING_REPO" config user.name "Bats"
  printf '# tunnel HA fixture\n' > "$WORKING_REPO/README.md"
  git -C "$WORKING_REPO" add README.md
  git -C "$WORKING_REPO" commit -q -m "init"
  git -C "$WORKING_REPO" remote add origin "$UPSTREAM_REPO"
  git -C "$WORKING_REPO" push -q origin main
}

write_tunnel_keys() {
  cat > "$BATS_FILE_TMPDIR/tunnel-private.pem" <<'EOF'
-----BEGIN PRIVATE KEY-----
MFECAQEwBQYDK2VwBCIEIC9K3MAqcmnhY+fzPm46VE8c2zn1yWcbqr3eJR5eDT6m
gSEAFGiB/MHVLlch3qxTEi07AvrYUbhePsN66hlI1LmMFLI=
-----END PRIVATE KEY-----
EOF

  cat > "$BATS_FILE_TMPDIR/tunnel-public.pem" <<'EOF'
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAFGiB/MHVLlch3qxTEi07AvrYUbhePsN66hlI1LmMFLI=
-----END PUBLIC KEY-----
EOF
}

render_runtime_config() {
  local port="$1"
  local out="$2"

  {
    cat <<EOF
server:
  port: $port
  host: "$RUNTIME_HOST"
  secure_cookies: false
  mcp_endpoint: "http://$RUNTIME_HOST:$port/mcp"
  tunnel:
    heartbeat_secs: 1
    expires_after_secs: 4
    reaper_interval_secs: 1
    deployments:
EOF
    for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
      echo "      $(deployment_id "$i"): |"
      sed 's/^/        /' "$BATS_FILE_TMPDIR/tunnel-public.pem"
    done
    cat <<EOF
oauth:
  login: github
  github_client_id: "test-client-id"
  github_redirect_uri: "http://$RUNTIME_HOST:$port/auth/github/callback"
  github_allowed_teams: []
agents:
  default_chain:
    primary: { name: test-model }
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
providers:
  - name: openai
    base_url: http://127.0.0.1:9
    models:
      - name: test-model
        max_tokens_per_response: 1024
        context_window_tokens: 4096
library:
  data_dir: "$BATS_FILE_TMPDIR/library-$port"
  repo_url: "file://$UPSTREAM_REPO"
  skill_sync_interval_secs: 1
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
toolsets:
  code_assistant:
    db_path: ""
  concourse:
    enabled: false
    url: ""
    team: ""
  zenduty:
    enabled: false
    url: ""
    default_team: ""
  mcp_upstreams: []
EOF
  } > "$out"
}

start_runtime() {
  local name="$1"
  local port="$2"
  local config="$3"
  local log="$BATS_FILE_TMPDIR/runtime-$name.log"
  local pid_file="$BATS_FILE_TMPDIR/runtime-$name.pid"

  (
    export PG_CON
    export DRUA_CONFIG="$config"
    export GITHUB_CLIENT_ID="test-client-id"
    export GITHUB_CLIENT_SECRET="test-client-secret"
    export CODE_ASSISTANT_DB_PATH=""
    export POD_IP="$RUNTIME_HOST"
    export DRUA_TUNNEL_INTERNAL_SECRET="$HA_SECRET"
    nohup bash -c 'exec "$@" server' _ $DRUA_BIN > "$log" 2>&1 &
    echo "$!" > "$pid_file"
  )

  for _i in $(seq 1 80); do
    if curl -fsS "http://$RUNTIME_HOST:$port/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done

  echo "runtime $name did not start; log follows" >&2
  cat "$log" >&2 || true
  return 1
}

stop_runtime() {
  local name="$1"
  local port
  case "$name" in
    a) port="$RUNTIME_A_PORT" ;;
    b) port="$RUNTIME_B_PORT" ;;
    *) return 1 ;;
  esac

  local pid_file="$BATS_FILE_TMPDIR/runtime-$name.pid"
  [ -f "$pid_file" ] || return 0

  local pid
  pid="$(cat "$pid_file")"
  kill "$pid" 2>/dev/null || true

  for _i in $(seq 1 40); do
    if ! curl -fsS "http://$RUNTIME_HOST:$port/" >/dev/null 2>&1; then
      rm -f "$pid_file"
      return 0
    fi
    sleep 0.25
  done

  kill -9 "$pid" 2>/dev/null || true
  rm -f "$pid_file"
}

crash_runtime() {
  local name="$1"
  local pid_file="$BATS_FILE_TMPDIR/runtime-$name.pid"
  [ -f "$pid_file" ] || return 0

  local pid
  pid="$(cat "$pid_file")"
  kill -9 "$pid" 2>/dev/null || true
  rm -f "$pid_file"
}

start_connectors() {
  mkdir -p "$BATS_FILE_TMPDIR/connectors"

  for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
    start_connector_process "$i" "$(default_connector_urls "$i")"
  done

  for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
    wait_connector_ready "$i"
  done
}

stop_connectors() {
  for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
    stop_connector "$i"
  done
}

default_connector_urls() {
  local i="$1"

  if [ "$i" -le "$DEPLOYMENT_SPLIT" ]; then
    echo "ws://$RUNTIME_HOST:$RUNTIME_A_PORT/tunnel/ws,ws://$RUNTIME_HOST:$RUNTIME_B_PORT/tunnel/ws"
  else
    echo "ws://$RUNTIME_HOST:$RUNTIME_B_PORT/tunnel/ws,ws://$RUNTIME_HOST:$RUNTIME_A_PORT/tunnel/ws"
  fi
}

start_connector_with_urls() {
  local i="$1"
  local urls="$2"

  start_connector_process "$i" "$urls"
  wait_connector_ready "$i"
}

start_connector_process() {
  local i="$1"
  local urls="$2"
  local dep

  dep="$(deployment_id "$i")"
  rm -f "$BATS_FILE_TMPDIR/connectors/$dep.ready"
  $TUNNEL_FIXTURE_BIN \
    --server-urls "$urls" \
    --private-key-file "$BATS_FILE_TMPDIR/tunnel-private.pem" \
    --deployment-id "$dep" \
    --ready-file "$BATS_FILE_TMPDIR/connectors/$dep.ready" \
    > "$BATS_FILE_TMPDIR/connectors/$dep.log" 2>&1 &
  echo "$!" > "$BATS_FILE_TMPDIR/connectors/$dep.pid"
}

wait_connector_ready() {
  local i="$1"
  local dep

  dep="$(deployment_id "$i")"
  wait_for_file "$BATS_FILE_TMPDIR/connectors/$dep.ready" "connector $dep ready"
}

stop_connector() {
  local i="$1"
  local dep
  dep="$(deployment_id "$i" 2>/dev/null || true)"
  if [ -n "$dep" ]; then
    stop_pid_file "$BATS_FILE_TMPDIR/connectors/$dep.pid"
  fi
}

stop_pid_file() {
  local pid_file="$1"
  [ -f "$pid_file" ] || return 0

  local pid
  pid="$(cat "$pid_file")"
  kill "$pid" 2>/dev/null || true

  for _i in $(seq 1 40); do
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$pid_file"
      return 0
    fi
    sleep 0.25
  done

  kill -9 "$pid" 2>/dev/null || true
  rm -f "$pid_file"
}

deployment_id() {
  local i="$1"
  printf "%sdep%02d" "$RUN_PREFIX" "$i"
}

mcp_call_port() {
  local port="$1"
  local token="$2"
  local method="$3"
  local params="$4"

  curl -sS --max-time 10 -X POST "http://$RUNTIME_HOST:$port/mcp" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -H "Authorization: Bearer $token" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" || true
}

assert_initial_split_callable() {
  local port="$1"
  local phase="$2"

  for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
    if [ "$i" -le "$DEPLOYMENT_SPLIT" ]; then
      wait_tool_call "$port" "$i" "$phase" "$RUNTIME_A_PORT"
    else
      wait_tool_call "$port" "$i" "$phase" "$RUNTIME_B_PORT"
    fi
  done
}

assert_all_deployments_owned_by() {
  local port="$1"
  local phase="$2"
  local owner_port="$3"

  for i in $(seq 1 "$DEPLOYMENT_COUNT"); do
    wait_tool_call "$port" "$i" "$phase" "$owner_port"
  done
}

wait_tool_call() {
  local port="$1"
  local i="$2"
  local phase="$3"
  local owner_port="${4:-}"
  local dep tool params response

  dep="$(deployment_id "$i")"
  tool="${dep}_echo_echo"
  params="$(jq -nc --arg t "$tool" --arg p "$phase" '{
    name: "call_tool",
    arguments: { tool_name: $t, arguments: { probe: $p } }
  }')"

  for _attempt in $(seq 1 80); do
    response="$(mcp_call_port "$port" "$AGENT_TOKEN" "tools/call" "$params")"
    if [[ "$response" == *"deployment=$dep;probe=$phase"* ]]; then
      if [ -n "$owner_port" ] && [[ "$response" != *"served_by=ws://$RUNTIME_HOST:$owner_port/tunnel/ws"* ]]; then
        sleep 0.25
        continue
      fi
      return 0
    fi
    sleep 0.25
  done

  echo "tool $tool did not respond through runtime port $port" >&2
  echo "$response" >&2
  return 1
}

wait_tool_absent() {
  local port="$1"
  local i="$2"
  local _phase="$3"
  local dep tool params response

  dep="$(deployment_id "$i")"
  tool="${dep}_echo_echo"
  params="$(jq -nc --arg t "$tool" '{
    name: "describe_tool",
    arguments: { tool_name: $t }
  }')"

  for _attempt in $(seq 1 80); do
    response="$(mcp_call_port "$port" "$AGENT_TOKEN" "tools/call" "$params")"
    if [[ "$response" == *"Tool not found: $tool"* ]]; then
      return 0
    fi
    sleep 0.25
  done

  echo "tool $tool was still present after its connector died" >&2
  echo "$response" >&2
  return 1
}

wait_registry_count() {
  local expected="$1"
  local actual

  for _attempt in $(seq 1 80); do
    actual="$(sql_scalar "SELECT count(*) FROM tunnel_registrations WHERE deployment_id LIKE '$RUN_PREFIX%'")"
    if [ "$actual" = "$expected" ]; then
      return 0
    fi
    sleep 0.25
  done

  echo "expected $expected tunnel registrations, got $actual" >&2
  dump_registry >&2
  return 1
}

wait_owner_count() {
  local owner="$1"
  local expected="$2"
  local actual

  for _attempt in $(seq 1 100); do
    actual="$(sql_scalar "SELECT count(*) FROM tunnel_registrations WHERE deployment_id LIKE '$RUN_PREFIX%' AND owner_pod_addr = '$owner'")"
    if [ "$actual" = "$expected" ]; then
      return 0
    fi
    sleep 0.25
  done

  echo "expected $expected tunnel registrations owned by $owner, got $actual" >&2
  dump_registry >&2
  return 1
}

wait_for_file() {
  local path="$1"
  local label="$2"

  for _attempt in $(seq 1 80); do
    [ -f "$path" ] && return 0
    sleep 0.25
  done

  echo "timed out waiting for $label ($path)" >&2
  return 1
}

sql_scalar() {
  psql "$PG_CON" -Atq -c "$1" | tr -d '[:space:]'
}

dump_registry() {
  psql "$PG_CON" -q -c \
    "SELECT deployment_id, owner_pod_addr, expires_at FROM tunnel_registrations WHERE deployment_id LIKE '$RUN_PREFIX%' ORDER BY deployment_id"
}
