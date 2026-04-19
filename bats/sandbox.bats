#!/usr/bin/env bats

load sandbox-helpers

setup_file() {
  start_sandbox_server
}

teardown_file() {
  stop_sandbox_server
}

# ── Health check ─────────────────────────────────────────────────────

@test "sandbox: GET /health returns 200" {
  run curl -sf "$(sandbox_url)/health"
  echo "$output"
  [ "$status" -eq 0 ]
  [ "$output" = "ok" ]
}

# ── Bash tool ────────────────────────────────────────────────────────

@test "sandbox: bash executes a simple command" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo hello world"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "hello world"
}

@test "sandbox: bash returns error on non-zero exit" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"exit 42"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "Exit code 42"
}

@test "sandbox: bash restart returns success" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"restart":true}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "restarted"
}

@test "sandbox: bash missing command returns error" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "command"
}

@test "sandbox: bash captures stderr" {
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo out; echo err >&2"}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "out"
  echo "$RESP" | jq -r '.output' | grep -q "err"
}

@test "sandbox: bash background process survives between calls" {
  # Start a background process that writes a marker file after a delay
  MARKER="$SANDBOX_WORK/bg-alive-marker"
  RESP=$(sandbox_execute "{\"tool\":\"bash\",\"input\":{\"command\":\"(sleep 1 && echo bg-alive > $MARKER) &\"}}")
  echo "$RESP"
  echo "$RESP" | jq -e '.is_error == false'

  # Run another command — the background process should still be running
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo foreground-ok"}}')
  echo "$RESP"
  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "foreground-ok"

  # Wait for background process to complete and verify marker file
  sleep 2
  [ -f "$MARKER" ]
  grep -q "bg-alive" "$MARKER"
}

@test "sandbox: bash session preserves state between calls" {
  # Set a variable in one call
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"export PERSIST_VAR=hello_session"}}')
  echo "$RESP"
  echo "$RESP" | jq -e '.is_error == false'

  # Read it back in a separate call — only works with persistent session
  RESP=$(sandbox_execute '{"tool":"bash","input":{"command":"echo $PERSIST_VAR"}}')
  echo "$RESP"
  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "hello_session"
}

# ── Text editor: create ──────────────────────────────────────────────

@test "sandbox: text editor creates a file" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"create\",\"path\":\"$SANDBOX_WORK/created.txt\",\"file_text\":\"line one\nline two\nline three\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "File created successfully"

  # Verify file exists with correct content
  [ -f "$SANDBOX_WORK/created.txt" ]
  grep -q "line one" "$SANDBOX_WORK/created.txt"
  grep -q "line three" "$SANDBOX_WORK/created.txt"
}

@test "sandbox: text editor creates nested directories" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"create\",\"path\":\"$SANDBOX_WORK/deep/nested/dir/file.txt\",\"file_text\":\"nested content\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  [ -f "$SANDBOX_WORK/deep/nested/dir/file.txt" ]
}

# ── Text editor: view (file) ────────────────────────────────────────

@test "sandbox: text editor views a file with line numbers" {
  # Create file first
  echo -e "alpha\nbeta\ngamma" > "$SANDBOX_WORK/viewme.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/viewme.txt\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "1: alpha"
  echo "$RESP" | jq -r '.output' | grep -q "2: beta"
  echo "$RESP" | jq -r '.output' | grep -q "3: gamma"
}

@test "sandbox: text editor views a file with view_range" {
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

@test "sandbox: text editor view nonexistent file returns error" {
  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"view\",\"path\":\"$SANDBOX_WORK/no_such_file.txt\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
}

# ── Text editor: view (directory) ────────────────────────────────────

@test "sandbox: text editor views a directory" {
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

@test "sandbox: text editor str_replace succeeds on unique match" {
  echo "hello world" > "$SANDBOX_WORK/replace.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace.txt\",\"old_str\":\"hello world\",\"new_str\":\"hello rust\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "Successfully replaced"
  grep -q "hello rust" "$SANDBOX_WORK/replace.txt"
}

@test "sandbox: text editor str_replace fails on no match" {
  echo "hello world" > "$SANDBOX_WORK/replace_nomatch.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace_nomatch.txt\",\"old_str\":\"nonexistent\",\"new_str\":\"whatever\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "No match found"
}

@test "sandbox: text editor str_replace fails on multiple matches" {
  echo -e "foo bar foo" > "$SANDBOX_WORK/replace_multi.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"str_replace\",\"path\":\"$SANDBOX_WORK/replace_multi.txt\",\"old_str\":\"foo\",\"new_str\":\"baz\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "2 matches"
}

# ── Text editor: insert ──────────────────────────────────────────────

@test "sandbox: text editor insert at beginning of file" {
  echo -e "line one\nline two" > "$SANDBOX_WORK/insert.txt"

  RESP=$(sandbox_execute "{\"tool\":\"str_replace_based_edit_tool\",\"input\":{\"command\":\"insert\",\"path\":\"$SANDBOX_WORK/insert.txt\",\"insert_line\":0,\"new_str\":\"header\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "Successfully inserted"

  # Header should be the first line
  HEAD=$(head -1 "$SANDBOX_WORK/insert.txt")
  [ "$HEAD" = "header" ]
}

@test "sandbox: text editor insert in middle of file" {
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

# ── Grep tool ────────────────────────────────────────────────────────

@test "sandbox: Grep finds pattern in files" {
  mkdir -p "$SANDBOX_WORK/grep-test"
  echo -e "hello world\ngoodbye world\nhello rust" > "$SANDBOX_WORK/grep-test/sample.txt"
  echo -e "no match here" > "$SANDBOX_WORK/grep-test/other.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Grep\",\"input\":{\"pattern\":\"hello\",\"path\":\"$SANDBOX_WORK/grep-test\",\"output_mode\":\"content\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "hello world"
  echo "$RESP" | jq -r '.output' | grep -q "hello rust"
}

@test "sandbox: Grep files_with_matches mode returns filenames" {
  mkdir -p "$SANDBOX_WORK/grep-fwm"
  echo "match me" > "$SANDBOX_WORK/grep-fwm/a.txt"
  echo "no hit" > "$SANDBOX_WORK/grep-fwm/b.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Grep\",\"input\":{\"pattern\":\"match\",\"path\":\"$SANDBOX_WORK/grep-fwm\",\"output_mode\":\"files_with_matches\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "a.txt"
  ! echo "$RESP" | jq -r '.output' | grep -q "b.txt"
}

@test "sandbox: Grep no matches returns empty" {
  mkdir -p "$SANDBOX_WORK/grep-empty"
  echo "nothing relevant" > "$SANDBOX_WORK/grep-empty/x.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Grep\",\"input\":{\"pattern\":\"zzz_nonexistent\",\"path\":\"$SANDBOX_WORK/grep-empty\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
}

@test "sandbox: Grep with head_limit caps output" {
  mkdir -p "$SANDBOX_WORK/grep-head"
  printf '%s\n' a a a a a a a a a a > "$SANDBOX_WORK/grep-head/many.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Grep\",\"input\":{\"pattern\":\"a\",\"path\":\"$SANDBOX_WORK/grep-head\",\"output_mode\":\"content\",\"head_limit\":3}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  LINE_COUNT=$(echo "$RESP" | jq -r '.output' | wc -l | tr -d ' ')
  [ "$LINE_COUNT" -le 4 ]  # 3 lines + possible trailing newline
}

@test "sandbox: Grep missing pattern returns error" {
  RESP=$(sandbox_execute '{"tool":"Grep","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "pattern"
}

@test "sandbox: Grep case insensitive flag works" {
  mkdir -p "$SANDBOX_WORK/grep-ci"
  echo "Hello World" > "$SANDBOX_WORK/grep-ci/mixed.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Grep\",\"input\":{\"pattern\":\"hello\",\"path\":\"$SANDBOX_WORK/grep-ci\",\"output_mode\":\"content\",\"-i\":true}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "Hello World"
}

# ── Glob tool ────────────────────────────────────────────────────────

@test "sandbox: Glob finds matching files" {
  mkdir -p "$SANDBOX_WORK/glob-test"
  echo "fn main() {}" > "$SANDBOX_WORK/glob-test/foo.rs"
  echo "text" > "$SANDBOX_WORK/glob-test/bar.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Glob\",\"input\":{\"pattern\":\"*.rs\",\"path\":\"$SANDBOX_WORK/glob-test\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  echo "$RESP" | jq -r '.output' | grep -q "foo.rs"
  ! echo "$RESP" | jq -r '.output' | grep -q "bar.txt"
}

@test "sandbox: Glob no matches returns empty" {
  mkdir -p "$SANDBOX_WORK/glob-empty"
  echo "text" > "$SANDBOX_WORK/glob-empty/hello.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Glob\",\"input\":{\"pattern\":\"*.xyz\",\"path\":\"$SANDBOX_WORK/glob-empty\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
}

@test "sandbox: Glob missing pattern returns error" {
  RESP=$(sandbox_execute '{"tool":"Glob","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "pattern"
}

@test "sandbox: Glob recursive pattern works" {
  mkdir -p "$SANDBOX_WORK/glob-deep/sub/nested"
  echo "a" > "$SANDBOX_WORK/glob-deep/top.rs"
  echo "b" > "$SANDBOX_WORK/glob-deep/sub/mid.rs"
  echo "c" > "$SANDBOX_WORK/glob-deep/sub/nested/deep.rs"
  echo "d" > "$SANDBOX_WORK/glob-deep/sub/nested/other.txt"

  RESP=$(sandbox_execute "{\"tool\":\"Glob\",\"input\":{\"pattern\":\"**/*.rs\",\"path\":\"$SANDBOX_WORK/glob-deep\"}}")
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == false'
  OUTPUT=$(echo "$RESP" | jq -r '.output')
  echo "$OUTPUT" | grep -q "top.rs"
  echo "$OUTPUT" | grep -q "mid.rs"
  echo "$OUTPUT" | grep -q "deep.rs"
  ! echo "$OUTPUT" | grep -q "other.txt"
}

# ── Unknown tool ─────────────────────────────────────────────────────

@test "sandbox: unknown tool returns error" {
  RESP=$(sandbox_execute '{"tool":"unknown_tool","input":{}}')
  echo "$RESP"

  echo "$RESP" | jq -e '.is_error == true'
  echo "$RESP" | jq -r '.output' | grep -q "Unknown tool"
}

# ── Initialize: scratch ──────────────────────────────────────────────

@test "sandbox: POST /initialize scratch returns workspace cwd" {
  RESP=$(sandbox_initialize '{"mode":"scratch"}')
  echo "$RESP"

  CWD=$(echo "$RESP" | jq -r '.cwd')
  [ "$CWD" = "$SANDBOX_WORK" ]
  echo "$RESP" | jq -e '.exported_system_prompt == null'
  echo "$RESP" | jq -e '.exported_skills == []'
  echo "$RESP" | jq -e '.error == null'
}

# ── Initialize: repo ────────────────────────────────────────────────

@test "sandbox: POST /initialize repo clones and returns cwd" {
  # Create a local bare repo to clone
  BARE_DIR="$BATS_FILE_TMPDIR/init-test-bare.git"
  WORK_DIR="$BATS_FILE_TMPDIR/init-test-work"
  rm -rf "$BARE_DIR" "$WORK_DIR"

  git init --bare "$BARE_DIR"
  git clone "$BARE_DIR" "$WORK_DIR"
  echo "hello" > "$WORK_DIR/README.md"
  git -C "$WORK_DIR" config user.email "test@test.com"
  git -C "$WORK_DIR" config user.name "Test"
  git -C "$WORK_DIR" add -A
  git -C "$WORK_DIR" commit -m "init"
  git -C "$WORK_DIR" push

  RESP=$(sandbox_initialize "{\"mode\":\"repo\",\"repo_url\":\"$BARE_DIR\"}")
  echo "$RESP"

  echo "$RESP" | jq -e '.error == null'
  CWD=$(echo "$RESP" | jq -r '.cwd')
  echo "cwd=$CWD"
  [[ "$CWD" == *"/repos/init-test-bare" ]]
  # Verify the cloned directory actually exists
  [ -d "$CWD" ]
}

@test "sandbox: POST /initialize repo returns CLAUDE.md when present" {
  BARE_DIR="$BATS_FILE_TMPDIR/init-claude-bare.git"
  WORK_DIR="$BATS_FILE_TMPDIR/init-claude-work"
  rm -rf "$BARE_DIR" "$WORK_DIR"

  git init --bare "$BARE_DIR"
  git clone "$BARE_DIR" "$WORK_DIR"
  echo "# My Instructions" > "$WORK_DIR/CLAUDE.md"
  git -C "$WORK_DIR" config user.email "test@test.com"
  git -C "$WORK_DIR" config user.name "Test"
  git -C "$WORK_DIR" add -A
  git -C "$WORK_DIR" commit -m "init"
  git -C "$WORK_DIR" push

  RESP=$(sandbox_initialize "{\"mode\":\"repo\",\"repo_url\":\"$BARE_DIR\"}")
  echo "$RESP"

  echo "$RESP" | jq -e '.error == null'
  echo "$RESP" | jq -e '.exported_system_prompt != null'
  echo "$RESP" | jq -e '.exported_system_prompt.file_name == "CLAUDE.md"'
  echo "$RESP" | jq -r '.exported_system_prompt.content' | grep -q "My Instructions"
}

@test "sandbox: POST /initialize repo returns skills from .claude/commands/" {
  BARE_DIR="$BATS_FILE_TMPDIR/init-skills-bare.git"
  WORK_DIR="$BATS_FILE_TMPDIR/init-skills-work"
  rm -rf "$BARE_DIR" "$WORK_DIR"

  git init --bare "$BARE_DIR"
  git clone "$BARE_DIR" "$WORK_DIR"
  mkdir -p "$WORK_DIR/.claude/commands"
  echo "Review the code carefully" > "$WORK_DIR/.claude/commands/review.md"
  echo "Deploy to production" > "$WORK_DIR/.claude/commands/deploy.md"
  git -C "$WORK_DIR" config user.email "test@test.com"
  git -C "$WORK_DIR" config user.name "Test"
  git -C "$WORK_DIR" add -f .
  git -C "$WORK_DIR" commit -m "init"
  git -C "$WORK_DIR" push

  RESP=$(sandbox_initialize "{\"mode\":\"repo\",\"repo_url\":\"$BARE_DIR\"}")
  echo "$RESP"

  echo "$RESP" | jq -e '.error == null'
  SKILLS_COUNT=$(echo "$RESP" | jq '.exported_skills | length')
  [ "$SKILLS_COUNT" -eq 2 ]
  echo "$RESP" | jq -e '.exported_skills[] | select(.name == "review") | .content' | grep -q "Review the code"
  echo "$RESP" | jq -e '.exported_skills[] | select(.name == "deploy") | .content' | grep -q "Deploy to production"
}

@test "sandbox: POST /initialize repo with bad URL returns error" {
  RESP=$(sandbox_initialize '{"mode":"repo","repo_url":"https://invalid.example.com/no/repo.git"}')
  echo "$RESP"

  echo "$RESP" | jq -e '.error != null'
  echo "$RESP" | jq -r '.error' | grep -qi "clone"
}

@test "sandbox: POST /initialize missing mode returns 422" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$(sandbox_url)/initialize" \
    -H "Content-Type: application/json" -d '{}')
  [ "$HTTP_CODE" = "422" ]
}

@test "sandbox: POST /initialize unknown mode returns error" {
  RESP=$(sandbox_initialize '{"mode":"unknown"}')
  echo "$RESP"

  echo "$RESP" | jq -e '.error != null'
  echo "$RESP" | jq -r '.error' | grep -q "Unknown mode"
}

# ── Error paths ──────────────────────────────────────────────────────

@test "sandbox: POST /execute with invalid JSON returns 400" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$(sandbox_url)/execute" \
    -H "Content-Type: application/json" -d 'not-json')
  [ "$HTTP_CODE" = "400" ]
}

@test "sandbox: GET unknown route returns 404" {
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$(sandbox_url)/nonexistent")
  [ "$HTTP_CODE" = "404" ]
}
