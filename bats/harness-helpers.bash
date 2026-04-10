REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)"
HARNESS_PID_FILE="$BATS_FILE_TMPDIR/harness.pid"
HARNESS_LOG="$BATS_FILE_TMPDIR/harness.log"
HARNESS_PORT="${HARNESS_PORT:-3123}"
HARNESS_WORK="$BATS_FILE_TMPDIR/workspace"

# AGENT_HARNESS_BIN is set by the nix wrapper (points at the built bundle).
# Fallback for local dev: build with esbuild first.
AGENT_HARNESS_BIN="${AGENT_HARNESS_BIN:-node $REPO_ROOT/images/sandbox-base/agent-harness/dist/index.js}"

start_harness() {
  mkdir -p "$HARNESS_WORK"

  # Initialise workspace as a git repo — Claude Code expects this.
  git -C "$HARNESS_WORK" init -q 2>/dev/null || true

  export HARNESS_PORT
  export HARNESS_CWD="$HARNESS_WORK"

  $AGENT_HARNESS_BIN > "$HARNESS_LOG" 2>&1 &
  echo "$!" > "$HARNESS_PID_FILE"

  # Wait for server to be ready
  for _i in $(seq 1 60); do
    if curl -sf "http://localhost:$HARNESS_PORT/health" > /dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done

  echo "Harness failed to start. Log:"
  cat "$HARNESS_LOG"
  return 1
}

stop_harness() {
  # Always dump the harness log for debugging CI failures
  if [ -f "$HARNESS_LOG" ]; then
    echo "=== harness log ==="
    cat "$HARNESS_LOG"
    echo "=== end harness log ==="
  fi

  if [ -f "$HARNESS_PID_FILE" ]; then
    kill "$(cat "$HARNESS_PID_FILE")" 2>/dev/null || true
    rm -f "$HARNESS_PID_FILE"
  fi

  # Clean up workspace before bats removes BATS_FILE_TMPDIR.
  # Claude Code writes files here during tests; leftover contents cause
  # bats' own temp-dir cleanup to fail with "Directory not empty".
  rm -rf "$HARNESS_WORK"
}

harness_url() {
  echo "http://localhost:$HARNESS_PORT"
}

# Send a message to the harness and capture the full SSE response.
# Usage: harness_send_message '{"prompt":"hello"}'
# Output is written to stdout.
harness_send_message() {
  local body="$1"
  local timeout="${2:-120}"

  curl -sf --max-time "$timeout" \
    -X POST "$(harness_url)/message" \
    -H "Content-Type: application/json" \
    -d "$body"
}

# Wait until the harness is no longer busy (up to 30s).
wait_not_busy() {
  for _i in $(seq 1 60); do
    local health
    health=$(curl -sf "$(harness_url)/health" 2>/dev/null) || true
    if echo "$health" | jq -e '.busy == false' > /dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  echo "Harness still busy after 30s"
  return 1
}

# Extract all SSE data lines for a given event type from an SSE response.
# Usage: echo "$SSE" | sse_events "result"
sse_events() {
  local event_type="$1"
  awk -v type="$event_type" '
    /^event: / { current = substr($0, 8) }
    /^data: / && current == type { print substr($0, 7) }
  '
}
