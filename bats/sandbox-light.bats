#!/usr/bin/env bats

load sandbox-light-helpers

setup_file() {
  start_sandbox_server
}

teardown_file() {
  stop_sandbox_server
}

# ── Health check ─────────────────────────────────────────────────────

@test "sandbox-light: GET /health returns 200" {
  run curl -sf "$(sandbox_url)/health"
  echo "$output"
  [ "$status" -eq 0 ]
  [ "$output" = "ok" ]
}

# ── Bash tool ────────────────────────────────────────────────────────

@test "sandbox-light: bash executes a simple command" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo hello world"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "hello world"
}

@test "sandbox-light: bash returns error on non-zero exit" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"exit 42"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "Exit code 42"
}

@test "sandbox-light: bash restart returns success" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"restart":true}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "restarted"
}

@test "sandbox-light: bash missing command returns error" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "command"
}

@test "sandbox-light: bash captures stderr" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo out; echo err >&2"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "out"
  echo "$RESP" | jq -r '.output' | grep -q "err"
}

# ── Text editor: create ──────────────────────────────────────────────

@test "sandbox-light: text editor creates a file" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"create\",\"path\":\"$SANDBOX_WORK/created.txt\",\"file_text\":\"line one\nline two\nline three\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "File created successfully"

  # Verify file exists with correct content
  [ -f "$SANDBOX_WORK/created.txt" ]
  grep -q "line one" "$SANDBOX_WORK/created.txt"
  grep -q "line three" "$SANDBOX_WORK/created.txt"
}

@test "sandbox-light: text editor creates nested directories" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"create\",\"path\":\"$SANDBOX_WORK/deep/nested/dir/file.txt\",\"file_text\":\"nested content\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  [ -f "$SANDBOX_WORK/deep/nested/dir/file.txt" ]
}

# ── Text editor: view (file) ────────────────────────────────────────

@test "sandbox-light: text editor views a file with line numbers" {
  # Create file first
  echo -e "alpha\nbeta\ngamma" > "$SANDBOX_WORK/viewme.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/viewme.txt\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "1: alpha"
  echo "$RESP" | jq -r '.output' | grep -q "2: beta"
  echo "$RESP" | jq -r '.output' | grep -q "3: gamma"
}

@test "sandbox-light: text editor views a file with view_range" {
  echo -e "a\nb\nc\nd\ne" > "$SANDBOX_WORK/range.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/range.txt\",\"view_range\":[2,4]}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  OUTPUT=$(echo "$RESP" | jq -r '.output')
  echo "$OUTPUT" | grep -q "2: b"
  echo "$OUTPUT" | grep -q "3: c"
  echo "$OUTPUT" | grep -q "4: d"
  # Should NOT contain lines outside the range
  ! echo "$OUTPUT" | grep -q "1: a"
  ! echo "$OUTPUT" | grep -q "5: e"
}

@test "sandbox-light: text editor view nonexistent file returns error" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/no_such_file.txt\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
}

# ── Text editor: view (directory) ────────────────────────────────────

@test "sandbox-light: text editor views a directory" {
  mkdir -p "$SANDBOX_WORK/listdir/subdir"
  echo "x" > "$SANDBOX_WORK/listdir/file_a.txt"
  echo "y" > "$SANDBOX_WORK/listdir/file_b.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/listdir\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "file_a.txt"
  echo "$RESP" | jq -r '.output' | grep -q "file_b.txt"
  echo "$RESP" | jq -r '.output' | grep -q "subdir/"
}

# ── Text editor: str_replace ─────────────────────────────────────────

@test "sandbox-light: text editor str_replace succeeds on unique match" {
  echo "hello world" > "$SANDBOX_WORK/replace.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace.txt\",\"old_str\":\"hello world\",\"new_str\":\"hello rust\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "Successfully replaced"
  grep -q "hello rust" "$SANDBOX_WORK/replace.txt"
}

@test "sandbox-light: text editor str_replace fails on no match" {
  echo "hello world" > "$SANDBOX_WORK/replace_nomatch.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace_nomatch.txt\",\"old_str\":\"nonexistent\",\"new_str\":\"whatever\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "No match found"
}

@test "sandbox-light: text editor str_replace fails on multiple matches" {
  echo -e "foo bar foo" > "$SANDBOX_WORK/replace_multi.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace_multi.txt\",\"old_str\":\"foo\",\"new_str\":\"baz\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "2 matches"
}

# ── Text editor: insert ──────────────────────────────────────────────

@test "sandbox-light: text editor insert at beginning of file" {
  echo -e "line one\nline two" > "$SANDBOX_WORK/insert.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"insert\",\"path\":\"$SANDBOX_WORK/insert.txt\",\"insert_line\":0,\"new_str\":\"header\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "Successfully inserted"

  # Header should be the first line
  HEAD=$(head -1 "$SANDBOX_WORK/insert.txt")
  [ "$HEAD" = "header" ]
}

@test "sandbox-light: text editor insert in middle of file" {
  echo -e "first\nthird" > "$SANDBOX_WORK/insert_mid.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"insert\",\"path\":\"$SANDBOX_WORK/insert_mid.txt\",\"insert_line\":1,\"new_str\":\"second\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'

  # Verify ordering
  CONTENT=$(cat "$SANDBOX_WORK/insert_mid.txt")
  echo "$CONTENT"
  echo "$CONTENT" | head -1 | grep -q "first"
  echo "$CONTENT" | head -2 | tail -1 | grep -q "second"
  echo "$CONTENT" | tail -1 | grep -q "third"
}

# ── Unknown tool ─────────────────────────────────────────────────────

@test "sandbox-light: unknown tool returns error" {
  RESP=$(sandbox_execute '{"tool":"unknown_tool","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "Unknown tool"
}

# ── Error paths ──────────────────────────────────────────────────────

@test "sandbox-light: POST /execute with invalid JSON returns 400" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$(sandbox_url)/execute" \
    -H "Content-Type: application/json" -d 'not-json')
  [ "$HTTP_CODE" = "400" ]
}

@test "sandbox-light: GET unknown route returns 404" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$(sandbox_url)/nonexistent")
  [ "$HTTP_CODE" = "404" ]
}
