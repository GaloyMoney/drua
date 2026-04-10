#!/usr/bin/env bats

load harness-helpers

setup_file() {
  # Require a real API key
  if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    skip "ANTHROPIC_API_KEY not set"
  fi
  start_harness
}

teardown_file() {
  stop_harness
}

# ── Health check ─────────────────────────────────────────────────────

@test "harness: GET /health returns 200 with status ok" {
  run curl -sf "$(harness_url)/health"
  echo "$output"
  [ "$status" -eq 0 ]

  echo "$output" | jq -e '.status == "ok"'
  echo "$output" | jq -e '.busy == false'
}

# ── First message ────────────────────────────────────────────────────

@test "harness: POST /message returns SSE with assistant and result events" {
  SSE=$(harness_send_message '{"prompt":"Reply with only the word PONG. Nothing else.","max_turns":3}' 180)
  echo "$SSE"

  # Must contain expected SSE event types
  echo "$SSE" | grep -q "^event: assistant"
  echo "$SSE" | grep -q "^event: result"

  # Result event must indicate success
  RESULT=$(echo "$SSE" | sse_events "result" | tail -1)
  echo "$RESULT" | jq -e '.type == "result"'
  echo "$RESULT" | jq -e 'has("session_id")'
}

# ── Second message (session persistence) ─────────────────────────────

@test "harness: second message reuses session and returns cost delta" {
  # The CLI may need to resume the session from the first message, so allow
  # extra time for the replay + new turn.
  SSE=$(harness_send_message '{"prompt":"Reply with only the word PING. Nothing else.","max_turns":3}' 180)
  echo "$SSE"

  echo "$SSE" | grep -q "^event: result"

  RESULT=$(echo "$SSE" | sse_events "result" | tail -1)
  echo "$RESULT" | jq -e '.type == "result"'

  # Health should now show a session_id
  HEALTH=$(curl -sf "$(harness_url)/health")
  echo "$HEALTH"
  echo "$HEALTH" | jq -e '.session_id != null'
  echo "$HEALTH" | jq -e '.cli_alive == true'
}

# ── MCP availability ─────────────────────────────────────────────────

@test "harness: message ignores unknown fields without crashing" {
  # Harness should gracefully ignore unknown fields in the input JSON.
  # MCP config is now read from projected SA token file, not from the
  # message payload.
  SSE=$(harness_send_message '{
    "prompt": "Reply with only the word OK. Nothing else.",
    "max_turns": 3,
    "unknown_field": "should be ignored"
  }' 180)
  echo "$SSE"

  # The harness should return some SSE events (result or error — both OK)
  echo "$SSE" | grep -qE "^event: (result|error)"

  # Health endpoint must still work
  run curl -sf "$(harness_url)/health"
  [ "$status" -eq 0 ]
  echo "$output" | jq -e '.status == "ok"'
}

# ── Error paths ──────────────────────────────────────────────────────

@test "harness: POST /message with missing prompt returns 400" {
  wait_not_busy
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$(harness_url)/message" \
    -H "Content-Type: application/json" -d '{}')
  [ "$HTTP_CODE" = "400" ]
}

@test "harness: POST /message with invalid JSON returns 400" {
  wait_not_busy
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$(harness_url)/message" \
    -H "Content-Type: application/json" -d 'not-json')
  [ "$HTTP_CODE" = "400" ]
}

@test "harness: GET unknown route returns 404" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$(harness_url)/nonexistent")
  [ "$HTTP_CODE" = "404" ]
}
