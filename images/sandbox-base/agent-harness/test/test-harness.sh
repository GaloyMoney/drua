#!/usr/bin/env bash
set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
HARNESS_PORT="${HARNESS_PORT:-3123}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HARNESS_JS="${HARNESS_JS:-$SCRIPT_DIR/../dist/index.js}"
WORK="$(mktemp -d)"

cleanup() {
  kill "$HARNESS_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# ── Start harness with mock cli.js ───────────────────────────────────
export ANTHROPIC_API_KEY=mock-test-key
export HARNESS_PORT
export HARNESS_CLI_PATH="${HARNESS_CLI_PATH:-$SCRIPT_DIR/mock-cli.mjs}"
export HARNESS_CWD="$WORK"

node "$HARNESS_JS" &
HARNESS_PID=$!

# Wait for server to be ready
for _ in $(seq 1 50); do
  if curl -sf "http://localhost:$HARNESS_PORT/health" > /dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

BASE="http://localhost:$HARNESS_PORT"
PASS=0
FAIL=0

assert() {
  local name="$1"; shift
  if eval "$@" > /dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo "  PASS  $name"
  else
    FAIL=$((FAIL + 1))
    echo "  FAIL  $name"
  fi
}

# ── Test 1: GET /health ──────────────────────────────────────────────
echo "--- GET /health ---"
HEALTH=$(curl -sf "$BASE/health")
assert "returns 200"        "test -n '$HEALTH'"
assert "has status=ok"      "echo '$HEALTH' | jq -e '.status == \"ok\"'"
assert "has busy=false"     "echo '$HEALTH' | jq -e '.busy == false'"
# cli_alive is false before the first message (CLI spawns lazily)
assert "has cli_alive field" "echo '$HEALTH' | jq -e 'has(\"cli_alive\")'"

# ── Test 2: POST /message with valid prompt ──────────────────────────
echo "--- POST /message (valid) ---"
SSE=$(curl -sf -X POST "$BASE/message" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"hello world"}')

# Write SSE to a temp file so grep works reliably
echo "$SSE" > "$WORK/sse1.txt"
assert "returns SSE data"        "grep -q '^event:' '$WORK/sse1.txt'"
assert "has system event"        "grep -q '^event: system' '$WORK/sse1.txt'"
assert "has assistant event"     "grep -q '^event: assistant' '$WORK/sse1.txt'"
assert "has result event"        "grep -q '^event: result' '$WORK/sse1.txt'"
assert "result has session_id"   "grep '^event: result' -A1 '$WORK/sse1.txt' | grep -q 'mock-session-001'"
assert "assistant has mock text" "grep '^event: assistant' -A1 '$WORK/sse1.txt' | grep -q 'Mock response to: hello world'"

# ── Test 2b: GET /health after message (cli should be alive now) ─────
echo "--- GET /health (post-message) ---"
HEALTH2=$(curl -sf "$BASE/health")
assert "cli_alive=true after message" "echo '$HEALTH2' | jq -e '.cli_alive == true'"
assert "session_id is set"            "echo '$HEALTH2' | jq -e '.session_id != null'"

# ── Test 3: POST /message with missing prompt ────────────────────────
echo "--- POST /message (missing prompt) ---"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST \
  "$BASE/message" \
  -H "Content-Type: application/json" -d '{}')
assert "returns 400" "test '$HTTP_CODE' = '400'"

# ── Test 4: POST /message with invalid JSON ──────────────────────────
echo "--- POST /message (invalid JSON) ---"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST \
  "$BASE/message" \
  -H "Content-Type: application/json" -d 'not-json')
assert "returns 400" "test '$HTTP_CODE' = '400'"

# ── Test 5: Unknown route ────────────────────────────────────────────
echo "--- GET /nonexistent ---"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/nonexistent")
assert "returns 404" "test '$HTTP_CODE' = '404'"

# ── Test 6: Second message reuses session (cost delta) ───────────────
echo "--- POST /message (second message — cost delta) ---"
SSE2=$(curl -sf -X POST "$BASE/message" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"second message"}')
echo "$SSE2" > "$WORK/sse2.txt"
assert "second message returns result" "grep -q '^event: result' '$WORK/sse2.txt'"
# Cost delta: total_cost_usd for msg2 is 0.002 cumulative, msg1 was 0.001, so delta = 0.001
assert "result has cost delta"         "grep '^event: result' -A1 '$WORK/sse2.txt' | grep -q '0.001'"

# ── Summary ──────────────────────────────────────────────────────────
echo ""
echo "Results: $PASS passed, $FAIL failed"
test "$FAIL" -eq 0
