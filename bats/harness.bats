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

@test "harness: first message returns SSE with assistant and result events" {
  SSE=$(harness_send_message '{"prompt":"Remember the secret word BANANA. Reply with only: Remembered BANANA.","max_turns":3}' 180)
  echo "$SSE"

  # Must contain expected SSE event types
  echo "$SSE" | grep -q "^event: assistant"
  echo "$SSE" | grep -q "^event: result"

  # Result event must indicate success
  RESULT=$(echo "$SSE" | sse_events "result" | tail -1)
  echo "$RESULT" | jq -e '.type == "result"'
  echo "$RESULT" | jq -e 'has("session_id")'
}

# ── Second message (same CLI process) ────────────────────────────────

@test "harness: second message to running CLI succeeds" {
  wait_not_busy

  SSE=$(harness_send_message '{"prompt":"Remember the secret word CHERRY. Reply with only: Remembered CHERRY.","max_turns":3}' 180)
  echo "$SSE"

  echo "$SSE" | grep -q "^event: result"

  RESULT=$(echo "$SSE" | sse_events "result" | tail -1)
  echo "$RESULT" | jq -e '.type == "result"'

  # Health should show a session_id and live CLI
  HEALTH=$(curl -sf "$(harness_url)/health")
  echo "$HEALTH"
  echo "$HEALTH" | jq -e '.session_id != null'
  echo "$HEALTH" | jq -e '.cli_alive == true'
}

# ── Session resume after restart ─────────────────────────────────────

@test "harness: restarted CLI resumes session and recalls previous messages" {
  wait_not_busy

  # Kill the CLI subprocess — next message will spawn with --resume
  harness_restart

  # Verify CLI is gone but session is preserved
  HEALTH=$(curl -sf "$(harness_url)/health")
  echo "$HEALTH"
  echo "$HEALTH" | jq -e '.cli_alive == false'
  echo "$HEALTH" | jq -e '.session_id != null'

  # Ask about previously remembered words — proves session resume works.
  # Give extra time: --resume replays the full conversation before processing.
  SSE=$(harness_send_message '{"prompt":"What were the two secret words I asked you to remember earlier? Reply with ONLY those two words, nothing else.","max_turns":3}' 300)
  echo "$SSE"

  echo "$SSE" | grep -q "^event: result"

  # Skip content assertion if the API returned a billing/quota error
  RESULT=$(echo "$SSE" | sse_events "result" | tail -1)
  if echo "$RESULT" | jq -e '.is_error == true' > /dev/null 2>&1; then
    echo "WARN: API returned an error, skipping content check:"
    echo "$RESULT" | jq -r '.result // "unknown"'
    # Still verify the session was resumed (session_id present in result)
    echo "$RESULT" | jq -e 'has("session_id")'
    return 0
  fi

  # The assistant response must mention both words from prior messages
  echo "$SSE" | grep -iq "BANANA"
  echo "$SSE" | grep -iq "CHERRY"
}

# ── MCP availability ─────────────────────────────────────────────────

@test "harness: message with mcp_servers config does not crash harness" {
  wait_not_busy

  # Pass an MCP server config — the harness should respawn the CLI with
  # --mcp-config.  The MCP server URL is a dummy; Claude Code may fail
  # to connect or produce an error, but the harness itself must not crash
  # and the health endpoint must remain responsive afterwards.
  SSE=$(harness_send_message '{
    "prompt": "Reply with only the word OK. Nothing else.",
    "max_turns": 3,
    "mcp_servers": {
      "test-mcp": {
        "type": "http",
        "url": "http://localhost:19999/mcp"
      }
    }
  }' 180)
  echo "$SSE"

  # The harness should return some SSE events (result or error — both OK)
  echo "$SSE" | grep -qE "^event: (result|error)"

  # Health endpoint must still work after MCP config change
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
