#!/usr/bin/env bats

# E2E: drua gateway proxies upstream MCP tools via the `call_tool` meta-tool.
#
# Spins up the in-tree `fake-mcp-upstream` binary as a real HTTP MCP server
# (serving the bundled fixtures dir) and points a freshly rendered drua.yml
# at it. Sub-threshold fixtures assert the upstream payload round-trips
# verbatim (passthrough); over-threshold fixtures assert the rendered
# `<summary>` + `<recovery>` envelope matches a snapshot under
# `bats/summarized-tool-responses/`. Re-run with `UPDATE_FIXTURES=1` to
# regenerate snapshots after intentional envelope changes.

load helpers

FAKE_UPSTREAM_BIN="${FAKE_UPSTREAM_BIN:-cargo run -q -p fake-mcp-upstream --}"
FAKE_UPSTREAM_PORT="${FAKE_UPSTREAM_PORT:-18765}"
FAKE_UPSTREAM_FIXTURES="$REPO_ROOT/lib/fake-mcp-upstream/fixtures"
FAKE_UPSTREAM_PID_FILE=""

start_fake_upstream() {
  FAKE_UPSTREAM_PID_FILE="$BATS_FILE_TMPDIR/fake-upstream.pid"
  $FAKE_UPSTREAM_BIN \
    --fixtures-dir "$FAKE_UPSTREAM_FIXTURES" \
    --bind "127.0.0.1:$FAKE_UPSTREAM_PORT" \
    --mount /mcp \
    > "$BATS_FILE_TMPDIR/fake-upstream.log" 2>&1 &
  echo "$!" > "$FAKE_UPSTREAM_PID_FILE"

  for _i in $(seq 1 60); do
    if curl -sf -o /dev/null -X POST "http://127.0.0.1:$FAKE_UPSTREAM_PORT/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'; then
      return 0
    fi
    sleep 0.5
  done

  echo "fake-mcp-upstream did not become ready on port $FAKE_UPSTREAM_PORT" >&2
  cat "$BATS_FILE_TMPDIR/fake-upstream.log" >&2
  return 1
}

stop_fake_upstream() {
  if [ -n "$FAKE_UPSTREAM_PID_FILE" ] && [ -f "$FAKE_UPSTREAM_PID_FILE" ]; then
    kill "$(cat "$FAKE_UPSTREAM_PID_FILE")" 2>/dev/null || true
    rm -f "$FAKE_UPSTREAM_PID_FILE"
  fi
}

render_drua_config_with_fake_upstream() {
  # Library init is unconditional in drua startup; seed an empty bare repo
  # so it has something valid to clone (we never read from it).
  local upstream_repo="$BATS_FILE_TMPDIR/library-upstream.git"
  local working_repo="$BATS_FILE_TMPDIR/library-working"
  mkdir -p "$working_repo"
  git init --bare --initial-branch=main "$upstream_repo" >/dev/null
  git -C "$working_repo" init -q -b main
  git -C "$working_repo" config user.email test@drua
  git -C "$working_repo" config user.name "Drua Test"
  git -C "$working_repo" commit -q --allow-empty -m "init"
  git -C "$working_repo" remote add origin "$upstream_repo"
  git -C "$working_repo" push -q origin main

  cat > "$BATS_FILE_TMPDIR/drua.yml" <<EOF
server:
  port: 4200
  host: "0.0.0.0"
  secure_cookies: false
  mcp_endpoint: "http://localhost:4200/mcp"
oauth:
  login: github
  github_client_id: "test-client-id"
  github_redirect_uri: "http://localhost:4200/auth/github/callback"
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
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
library:
  data_dir: "$BATS_FILE_TMPDIR/library"
  repo_url: "file://$upstream_repo"
  skill_sync_interval_secs: 60
toolsets:
  mcp_upstreams:
    - name: fake_upstream
      url: http://127.0.0.1:$FAKE_UPSTREAM_PORT/mcp
      auth_required: false
      category: testing
      category_description: "fake-mcp-upstream fixtures for e2e tests"
EOF
  export DRUA_CONFIG="$BATS_FILE_TMPDIR/drua.yml"
}

setup_file() {
  start_fake_upstream
  render_drua_config_with_fake_upstream
  start_server
  create_test_agent
}

teardown_file() {
  stop_server
  stop_fake_upstream
}

# Dispatch an upstream tool through drua's `call_tool` meta-tool. The
# prefixed name is `<upstream_name>_<original-tool-name>`.
fake_call() {
  local prefixed="$1"
  local args="${2:-{\}}"
  local body
  body="$(jq -nc --arg t "$prefixed" --argjson a "$args" \
    '{name:"call_tool", arguments:{tool_name:$t, arguments:$a}}')"
  mcp_call "$AGENT_TOKEN" "tools/call" "$body"
}

# Diff the gateway's rendered envelope against a snapshot under
# bats/summarized-tool-responses/<fixture>.txt. `UPDATE_FIXTURES=1`
# rewrites the snapshot with the live response. UUIDs in
# `invocation_id="…"` get normalized to `<uuid>` so per-run randomness
# doesn't break the diff.
assert_summarized_text_matches() {
  local prefixed="$1"
  local fixture="$2"
  local args="${3:-{\}}"
  local snapshot="$REPO_ROOT/bats/summarized-tool-responses/${fixture}.txt"

  run fake_call "$prefixed" "$args"
  [ "$status" -eq 0 ]
  local text normalized
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  normalized="$(printf '%s\n' "$text" | sed -E \
    's/invocation_id="[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"/invocation_id="<uuid>"/g')"

  if [ "${UPDATE_FIXTURES:-}" = "1" ]; then
    mkdir -p "$(dirname "$snapshot")"
    printf '%s' "$normalized" > "$snapshot"
    return 0
  fi

  [ -f "$snapshot" ] || {
    echo "snapshot missing: $snapshot (run with UPDATE_FIXTURES=1 to seed)" >&2
    return 1
  }
  diff -u "$snapshot" <(printf '%s' "$normalized")
}

@test "fake-upstream: obj-small round-trips verbatim (passthrough)" {
  run fake_call "fake_upstream_obj-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == '{"ok":true,"count":3}' ]]
  # passthrough → no envelope wrapping (no invocation_id field surfaces)
  [[ "$output" != *'"invocation_id"'* ]]
}

@test "fake-upstream: str-small round-trips verbatim" {
  run fake_call "fake_upstream_str-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == "hello world" ]]
}

@test "fake-upstream: arr-small round-trips verbatim" {
  run fake_call "fake_upstream_arr-small"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == '[{"id":1},{"id":2}]' ]]
}

@test "fake-upstream: scalar-number round-trips verbatim" {
  run fake_call "fake_upstream_scalar-number"
  echo "$output"
  local text
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  [[ "$text" == "42" ]]
}

@test "fake-upstream: is-error-text propagates is_error flag" {
  run fake_call "fake_upstream_is-error-text"
  echo "$output"
  local text is_error
  text="$(echo "$output" | jq -r '.result.content[0].text')"
  is_error="$(echo "$output" | jq -r '.result.isError')"
  [[ "$text" == "pods 'foo' not found" ]]
  [[ "$is_error" == "true" ]]
}

@test "fake-upstream: str-large-table → tool_output_fetch round-trips elided lines" {
  # Summarise once: extract the advertised recovery args from <recovery>.
  run fake_call "fake_upstream_str-large-table"
  [ "$status" -eq 0 ]
  local envelope call_line inv_id mode offset len
  envelope="$(echo "$output" | jq -r '.result.content[0].text')"
  call_line="$(echo "$envelope" | grep 'tool_output_fetch(' | head -1)"
  inv_id="$(echo "$call_line" | grep -oE 'invocation_id="[0-9a-f-]{36}"' \
    | sed -E 's/invocation_id="([^"]+)"/\1/')"
  mode="$(echo "$call_line" | grep -oE '"mode":"[^"]+"' \
    | sed -E 's/"mode":"([^"]+)"/\1/')"
  offset="$(echo "$call_line" | grep -oE '"offset":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  len="$(echo "$call_line" | grep -oE '"len":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  [ -n "$inv_id" ] && [ -n "$mode" ] && [ -n "$offset" ] && [ -n "$len" ]

  # Replay the recovery call the envelope advertised.
  local fetch_args fetch_body
  fetch_args="$(jq -nc \
    --arg id "$inv_id" \
    --arg m "$mode" \
    --argjson o "$offset" \
    --argjson l "$len" \
    '{invocation_id:$id, path:"$", query:{mode:$m, offset:$o, len:$l}}')"
  fetch_body="$(jq -nc --argjson a "$fetch_args" \
    '{name:"tool_output_fetch", arguments:$a}')"
  run mcp_call "$AGENT_TOKEN" "tools/call" "$fetch_body"
  [ "$status" -eq 0 ]
  local recovered
  recovered="$(echo "$output" | jq -r '.result.content[0].text')"

  # Diff against the snapshot of the elided-middle slice. UPDATE_FIXTURES=1
  # regenerates from the live response.
  local snapshot="$REPO_ROOT/bats/summarized-tool-responses/str-large-table.recovered.txt"
  if [ "${UPDATE_FIXTURES:-}" = "1" ]; then
    mkdir -p "$(dirname "$snapshot")"
    printf '%s\n' "$recovered" > "$snapshot"
  else
    [ -f "$snapshot" ] || {
      echo "snapshot missing: $snapshot (run with UPDATE_FIXTURES=1 to seed)" >&2
      return 1
    }
    diff -u "$snapshot" <(printf '%s\n' "$recovered")
  fi
}

@test "fake-upstream: str-large-table summarized into <summary>+<recovery> envelope" {
  # ~13KB kubectl-table-style output (real k8s_list_pods shape); exceeds the
  # 8KiB threshold so tool-caching elides head/tail and renders a
  # range-mode tool_output_fetch recovery template. Snapshot lives at
  # bats/summarized-tool-responses/str-large-table.txt — open the file to
  # see exactly what the agent receives. Re-run with UPDATE_FIXTURES=1 to
  # regenerate after intentional envelope changes.
  assert_summarized_text_matches \
    "fake_upstream_str-large-table" \
    "str-large-table"
}

@test "fake-upstream: arr-large-passthrough-items summarized into array sentinel envelope" {
  # 14KB top-level JSON array of 500 small {id, tag} objects. Items are
  # sub-threshold so they passthrough verbatim; the root array is over
  # threshold so the walker emits an {_elided, kind: "array", head, tail}
  # sentinel and a json_array_slice recovery template.
  assert_summarized_text_matches \
    "fake_upstream_arr-large-passthrough-items" \
    "arr-large-passthrough-items"
}

@test "fake-upstream: arr-large-passthrough-items → tool_output_fetch round-trips elided items" {
  run fake_call "fake_upstream_arr-large-passthrough-items"
  [ "$status" -eq 0 ]
  local envelope call_line inv_id mode offset len
  envelope="$(echo "$output" | jq -r '.result.content[0].text')"
  call_line="$(echo "$envelope" | grep 'tool_output_fetch(' | head -1)"
  inv_id="$(echo "$call_line" | grep -oE 'invocation_id="[0-9a-f-]{36}"' \
    | sed -E 's/invocation_id="([^"]+)"/\1/')"
  mode="$(echo "$call_line" | grep -oE '"mode":"[^"]+"' \
    | sed -E 's/"mode":"([^"]+)"/\1/')"
  offset="$(echo "$call_line" | grep -oE '"offset":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  len="$(echo "$call_line" | grep -oE '"len":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  [ -n "$inv_id" ] && [ -n "$mode" ] && [ -n "$offset" ] && [ -n "$len" ]

  local fetch_args fetch_body
  fetch_args="$(jq -nc \
    --arg id "$inv_id" \
    --arg m "$mode" \
    --argjson o "$offset" \
    --argjson l "$len" \
    '{invocation_id:$id, path:"$", query:{mode:$m, offset:$o, len:$l}}')"
  fetch_body="$(jq -nc --argjson a "$fetch_args" \
    '{name:"tool_output_fetch", arguments:$a}')"
  run mcp_call "$AGENT_TOKEN" "tools/call" "$fetch_body"
  [ "$status" -eq 0 ]
  local recovered
  recovered="$(echo "$output" | jq -r '.result.content[0].text')"

  local snapshot="$REPO_ROOT/bats/summarized-tool-responses/arr-large-passthrough-items.recovered.json"
  if [ "${UPDATE_FIXTURES:-}" = "1" ]; then
    mkdir -p "$(dirname "$snapshot")"
    printf '%s\n' "$recovered" > "$snapshot"
  else
    [ -f "$snapshot" ] || {
      echo "snapshot missing: $snapshot (run with UPDATE_FIXTURES=1 to seed)" >&2
      return 1
    }
    diff -u "$snapshot" <(printf '%s\n' "$recovered")
  fi
}

@test "fake-upstream: arr-large-fat-items summarized → multi-element body line-elide + array sentinel" {
  # 5 GitHub PR rows, each with a 11.7 KB markdown body (~65 KB total).
  # Walker recurses with per-item budget, line-elides each body, then
  # if the walked array still exceeds budget it sentinels — exercising
  # both kinds of truncation at once.
  assert_summarized_text_matches \
    "fake_upstream_arr-large-fat-items" \
    "arr-large-fat-items"
}

@test "fake-upstream: arr-large-fat-items → tool_output_fetch round-trips elided slice" {
  run fake_call "fake_upstream_arr-large-fat-items"
  [ "$status" -eq 0 ]
  local envelope call_line inv_id rec_path mode offset len
  envelope="$(echo "$output" | jq -r '.result.content[0].text')"
  # Pull the literal call form from <recovery>.
  call_line="$(echo "$envelope" | grep 'tool_output_fetch(' | head -1)"
  inv_id="$(echo "$call_line" | grep -oE 'invocation_id="[0-9a-f-]{36}"' \
    | sed -E 's/invocation_id="([^"]+)"/\1/')"
  rec_path="$(echo "$call_line" | grep -oE 'path="[^"]*"' \
    | sed -E 's/path="([^"]*)"/\1/')"
  mode="$(echo "$call_line" | grep -oE '"mode":"[^"]+"' \
    | sed -E 's/"mode":"([^"]+)"/\1/')"
  offset="$(echo "$call_line" | grep -oE '"offset":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  len="$(echo "$call_line" | grep -oE '"len":[0-9]+' | head -1 | grep -oE '[0-9]+')"
  [ -n "$inv_id" ] && [ -n "$rec_path" ] && [ -n "$mode" ] && [ -n "$offset" ] && [ -n "$len" ]

  local fetch_args fetch_body
  fetch_args="$(jq -nc \
    --arg id "$inv_id" \
    --arg p "$rec_path" \
    --arg m "$mode" \
    --argjson o "$offset" \
    --argjson l "$len" \
    '{invocation_id:$id, path:$p, query:{mode:$m, offset:$o, len:$l}}')"
  fetch_body="$(jq -nc --argjson a "$fetch_args" \
    '{name:"tool_output_fetch", arguments:$a}')"
  run mcp_call "$AGENT_TOKEN" "tools/call" "$fetch_body"
  [ "$status" -eq 0 ]
  local recovered
  recovered="$(echo "$output" | jq -r '.result.content[0].text')"

  local snapshot="$REPO_ROOT/bats/summarized-tool-responses/arr-large-fat-items.recovered.json"
  if [ "${UPDATE_FIXTURES:-}" = "1" ]; then
    mkdir -p "$(dirname "$snapshot")"
    printf '%s\n' "$recovered" > "$snapshot"
  else
    [ -f "$snapshot" ] || {
      echo "snapshot missing: $snapshot (run with UPDATE_FIXTURES=1 to seed)" >&2
      return 1
    }
    diff -u "$snapshot" <(printf '%s\n' "$recovered")
  fi
}

# Snapshot the compose structuredContent against a fixture. Normalises
# uuids (invocation_ids — varied per run) and execution_time_ms (timing-
# dependent) so the diff is byte-stable.
assert_compose_snapshot() {
  local raw="$1"
  local fixture="$2"
  local snapshot="$REPO_ROOT/bats/summarized-tool-responses/${fixture}.json"
  local normalized
  normalized="$(echo "$raw" | jq -S '
    walk(
      if type == "object" then
        if has("invocation_id") then .invocation_id = "<uuid>" else . end |
        if has("result_invocation_id") and .result_invocation_id != null then .result_invocation_id = "<uuid>" else . end |
        if has("execution_time_ms") then .execution_time_ms = "<ms>" else . end
      else . end
    )' | sed -E \
      -e 's/invocation_id="[0-9a-f-]{36}"/invocation_id="<uuid>"/g' \
      -e 's/invocation_id=\\"[0-9a-f-]{36}\\"/invocation_id=\\"<uuid>\\"/g')"
  if [ "${UPDATE_FIXTURES:-}" = "1" ]; then
    mkdir -p "$(dirname "$snapshot")"
    printf '%s\n' "$normalized" > "$snapshot"
    return 0
  fi
  [ -f "$snapshot" ] || {
    echo "snapshot missing: $snapshot (run with UPDATE_FIXTURES=1 to seed)" >&2
    return 1
  }
  diff -u "$snapshot" <(printf '%s\n' "$normalized")
}

@test "compose: sub_invocations exposed + recovery round-trips through tool_output_fetch" {
  # Round 1 — compose calls 3 fake-upstream tools (small, large-string,
  # large-array). Returns metadata including sub_invocations[] with
  # invocation_ids for the persisted (large) ones.
  local r1_script
  r1_script='return {
    small: await tools["fake_upstream_obj-small"]({}),
    str: await tools["fake_upstream_str-large-table"]({}),
    arr: await tools["fake_upstream_arr-large-passthrough-items"]({}),
  };'
  local r1_body
  r1_body="$(jq -nc --arg s "$r1_script" \
    '{name:"compose", arguments:{script:$s}}')"
  run mcp_call "$AGENT_TOKEN" "tools/call" "$r1_body"
  [ "$status" -eq 0 ]
  local r1_struct
  r1_struct="$(echo "$output" | jq -r '.result.structuredContent')"

  # Compose owns its envelope shape (outputSchema is ComposeOutput,
  # a flat schema — no DruaToolResult wrapper). ComposeOutput fields
  # are at the structuredContent root.
  #
  # Structural: 2 sub_invocations expected (small obj is passthrough →
  # no recovery; large str + large arr are persisted).
  local n_subs
  n_subs="$(echo "$r1_struct" | jq -r '.sub_invocations | length')"
  [ "$n_subs" = "2" ]

  # Each persisted sub_invocation carries a uuid + a kind discriminator.
  local subs_summary
  subs_summary="$(echo "$r1_struct" | jq -r \
    '.sub_invocations | map({tool_name, kind})')"
  echo "$subs_summary"
  [[ "$subs_summary" == *"str-large-table"* ]]
  [[ "$subs_summary" == *"arr-large-passthrough-items"* ]]

  # Snapshot the round-1 structured response (uuids + timings normalized).
  assert_compose_snapshot "$r1_struct" "compose-roundtrip-1"

  # Round 2 — feed each captured invocation_id back through
  # tool_output_fetch inside a fresh compose script. Asserts end-to-end
  # recoverability of sub-call data persisted by persist_for_compose.
  local str_id arr_id
  str_id="$(echo "$r1_struct" | jq -r \
    '.sub_invocations[] | select(.tool_name | endswith("str-large-table")) | .invocation_id')"
  arr_id="$(echo "$r1_struct" | jq -r \
    '.sub_invocations[] | select(.tool_name | endswith("arr-large-passthrough-items")) | .invocation_id')"
  [[ "$str_id" =~ ^[0-9a-f-]{36}$ ]]
  [[ "$arr_id" =~ ^[0-9a-f-]{36}$ ]]

  # Construct a recovery script via jq so quoting is correct.
  local r2_script r2_body
  r2_script="$(jq -nc \
    --arg str_id "$str_id" \
    --arg arr_id "$arr_id" \
    '"return { str_slice: await tools.tool_output_fetch({invocation_id: \"" + $str_id + "\", path: \"$\", query: {mode: \"lines\", offset: 12, len: 16}}), arr_slice: await tools.tool_output_fetch({invocation_id: \"" + $arr_id + "\", path: \"$\", query: {mode: \"json_array_slice\", offset: 195, len: 109}}) };"')"
  r2_body="$(jq -nc --argjson s "$r2_script" \
    '{name:"compose", arguments:{script:$s}}')"
  run mcp_call "$AGENT_TOKEN" "tools/call" "$r2_body"
  [ "$status" -eq 0 ]

  # The recovered slices must match the elided middle of each fixture.
  # ComposeOutput.result is the JS return —
  # `{ str_slice: <fetched_string>, arr_slice: <fetched_array> }`.
  local r2_struct r2_inner
  r2_struct="$(echo "$output" | jq -r '.result.structuredContent')"
  r2_inner="$(echo "$r2_struct" | jq -r '.result')"

  # str_slice is a JSON string; arr_slice is a JSON array.
  local str_slice arr_slice_len
  str_slice="$(echo "$r2_inner" | jq -r '.str_slice' | head -c 60)"
  arr_slice_len="$(echo "$r2_inner" | jq -r '.arr_slice | length')"

  # Sanity: line-mode slice begins with a known kubectl row prefix.
  [[ "$str_slice" == kube-system* ]] || {
    echo "expected str_slice to start with 'kube-system'; got: $str_slice" >&2
    return 1
  }
  [ "$arr_slice_len" = "109" ]

  # Snapshot the round-2 structured response. uuids normalized, but
  # the actual recovered slices (str_slice text + arr_slice array) are
  # byte-stable across runs since the fixtures are deterministic.
  assert_compose_snapshot "$r2_struct" "compose-roundtrip-2"
}

@test "fake-upstream: nix-copy-output → chain compacts copy/build/cache runs" {
  # Tool output is well under threshold (1.8KB) so the budget-aware
  # eliders don't fire. But the nix pattern passes still detect copy/
  # building/cache-activity runs and compact them — exercising the
  # generic StringSummarizerChain on a small payload.
  #
  # Snapshot shows the rendered nix-* markers in place of the original
  # runs. UPDATE_FIXTURES=1 regens.
  assert_summarized_text_matches \
    "fake_upstream_nix-copy-output" \
    "nix-copy-output"
}

@test "fake-upstream: concourse-build-log → preprocessor strips ANSI/timestamps + line-elide" {
  # ~32KB ANSI-coloured timestamped concourse build log. Tool name
  # `fake_upstream_concourse-build-log` ends with `concourse-build-log`
  # which is in preprocessors::concourse::TOOL_NAMES, so the
  # preprocessor engages and strips ANSI escapes + `[HH:MM:SS] `
  # timestamps before the budget-aware line-eliding runs. Snapshot
  # shows the cleaned head/tail.
  assert_summarized_text_matches \
    "fake_upstream_concourse-build-log" \
    "concourse-build-log"
}
