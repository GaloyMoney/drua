REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)"
SERVER_PID_FILE="$BATS_FILE_TMPDIR/sandbox-tool-server.pid"
SERVER_LOG="$BATS_FILE_TMPDIR/sandbox-tool-server.log"
SERVER_PORT="${SANDBOX_TOOL_SERVER_PORT:-3199}"
# Use BATS_FILE_TMPDIR so the path is stable across setup_file and all tests.
SANDBOX_WORK="$BATS_FILE_TMPDIR/workspace"

# SANDBOX_TOOL_SERVER_BIN is set by the nix wrapper.
SANDBOX_TOOL_SERVER_BIN="${SANDBOX_TOOL_SERVER_BIN:-cargo run -p sandbox-tool-server --}"

start_sandbox_server() {
  mkdir -p "$SANDBOX_WORK"

  export PORT="$SERVER_PORT"
  # Text editor commands operate on files inside the workspace.
  cd "$SANDBOX_WORK"

  $SANDBOX_TOOL_SERVER_BIN > "$SERVER_LOG" 2>&1 &
  echo "$!" > "$SERVER_PID_FILE"

  cd "$REPO_ROOT"

  # Wait for server to be ready
  for _i in $(seq 1 60); do
    if curl -sf "http://localhost:$SERVER_PORT/health" > /dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done

  echo "Sandbox tool server failed to start. Log:"
  cat "$SERVER_LOG"
  return 1
}

stop_sandbox_server() {
  if [ -f "$SERVER_LOG" ]; then
    echo "=== sandbox-tool-server log ==="
    cat "$SERVER_LOG"
    echo "=== end sandbox-tool-server log ==="
  fi

  if [ -f "$SERVER_PID_FILE" ]; then
    local pid
    pid=$(cat "$SERVER_PID_FILE")
    kill "$pid" 2>/dev/null || true
    sleep 0.5
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$SERVER_PID_FILE"
  fi
}

sandbox_url() {
  echo "http://localhost:$SERVER_PORT"
}

# Execute a tool call against the sandbox server.
# Usage: sandbox_execute '{"tool":"bash","input":{"command":"echo hi"}}'
sandbox_execute() {
  local body="$1"
  curl -sf --max-time 30 \
    -X POST "$(sandbox_url)/execute" \
    -H "Content-Type: application/json" \
    -d "$body"
}

# Dump tail of server log on test failure.
teardown() {
  # shellcheck disable=SC2154
  if [ -n "${BATS_TEST_COMPLETED:-}" ]; then
    return
  fi
  if [ -f "$SERVER_LOG" ]; then
    echo "=== sandbox-tool-server log (last 40 lines) ==="
    tail -40 "$SERVER_LOG"
    echo "=== end sandbox-tool-server log ==="
  fi
}
