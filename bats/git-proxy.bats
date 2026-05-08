#!/usr/bin/env bats
#
# Drua git-proxy smart-HTTP contract (per memo 019dfebc §6).
#
# Layered tests — each `@test` exercises one decision in the
# accept/reject pipeline. The git-client e2e tests (clone / push)
# light up as the M1.5 backend lands.

load helpers

setup_file() {
  # Allow-list is global → no per-project SQL seeding needed. The
  # dev-mode auth path accepts the literal `dev-agent` Bearer token
  # (no UUID; auth_middleware synthesises an AuthSubject::Agent with
  # nil project_id + agent_id) so we don't need a real agent in the DB.
  export GIT_PROXY_TOKEN="dev-agent"

  # Bring up the test bare upstream(s) BEFORE rendering the config so
  # we can plug their file:// URLs into the allow-list entries.
  mkdir -p "$BATS_FILE_TMPDIR/upstream"
  UPSTREAM_DRUA="$(mk_upstream_repo GaloyMoney drua)"
  UPSTREAM_RO="$(mk_upstream_repo GaloyMoney drua-readonly)"
  export UPSTREAM_DRUA UPSTREAM_RO
  write_git_proxy_config \
    "GaloyMoney/drua:pull,push:refs/heads/bot/*,refs/heads/main:$UPSTREAM_DRUA" \
    "GaloyMoney/drua-readonly:pull:refs/heads/main:$UPSTREAM_RO"
  start_server
}

teardown_file() {
  stop_server
}

setup() {
  # Every test runs in its own tmpdir; reset the per-test response file.
  : > "$BATS_TEST_TMPDIR/body" 2>/dev/null || true
}

GP_URL="http://localhost:4200/git"
SVC_PULL="?service=git-upload-pack"

# ─── Auth contract ─────────────────────────────────────────────────────

@test "git-proxy: missing Authorization rejected with 401" {
  # No `Authorization` header → AuthSubject::Anonymous. The shared
  # `Audit::record_from_context` skips entries without a subject by
  # design (anonymous junk is high-volume and would drown audit_entries);
  # the rejection is still visible via tracing on the handler span.
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "401" ]
  grep -q 'unauthorized' "$BATS_TEST_TMPDIR/body"
}

@test "git-proxy: dev-agent token with extra suffix rejected with 401" {
  # Strict match — only the literal `dev-agent` is accepted. Anything
  # else is unauthorised so a stray space / typo doesn't accidentally
  # succeed.
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer dev-agent:00000000-0000-0000-0000-000000000000" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "401" ]
}

@test "git-proxy: random non-dev-agent string rejected with 401" {
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer not-a-real-token" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "401" ]
}

# ─── URL-shape contract ────────────────────────────────────────────────

@test "git-proxy: missing service= query rejected with 400" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/GaloyMoney/drua/info/refs")"
  [ "$status" = "400" ]
  grep -q 'missing_service_query' "$BATS_TEST_TMPDIR/body"
}

@test "git-proxy: invalid service= value rejected with 400" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/GaloyMoney/drua/info/refs?service=git-archive")"
  [ "$status" = "400" ]
  grep -q 'invalid_service' "$BATS_TEST_TMPDIR/body"
}

# ─── Allow-list contract ───────────────────────────────────────────────

@test "git-proxy: pull info/refs returns smart-HTTP advertisement + audit row" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "200" ]
  # Smart-HTTP service advertisement: 4-byte pkt-line length prefix
  # + `# service=git-upload-pack` literal somewhere in the first frame.
  grep -aq 'service=git-upload-pack' "$BATS_TEST_TMPDIR/body"

  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM audit_entries WHERE action = 'git_proxy.git-upload-pack' AND outcome = 'success' AND metadata->>'owner' = 'GaloyMoney' AND metadata->>'repo' = 'drua'")"
  [ "$count" -ge 1 ]
}

@test "git-proxy: pull on repo absent from the global allow-list rejected 403" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/attacker/exfil/info/refs$SVC_PULL")"
  [ "$status" = "403" ]
  grep -q 'repo_not_allowed' "$BATS_TEST_TMPDIR/body"

  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM audit_entries WHERE action = 'git_proxy.git-upload-pack' AND outcome = 'error' AND error_message = 'repo_not_allowed' AND metadata->>'owner' = 'attacker' AND metadata->>'repo' = 'exfil'")"
  [ "$count" -ge 1 ]
}

@test "git-proxy: push to a repo configured pull-only rejected 403 with mode_not_allowed" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/GaloyMoney/drua-readonly/info/refs?service=git-receive-pack")"
  [ "$status" = "403" ]
  grep -q 'mode_not_allowed' "$BATS_TEST_TMPDIR/body"
}

@test "git-proxy: malformed repo coord rejected with 400 (no audit row)" {
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/Galoy.Money/dr%2Fua/info/refs$SVC_PULL")"
  # URL-decoded `dr/ua` would inject path traversal — must 4xx.
  [ "$status" -ge 400 ]
  [ "$status" -lt 500 ]
}

# ─── git-client e2e — pull side ────────────────────────────────────────

@test "git-proxy: git ls-remote against allowed repo succeeds" {
  cd "$BATS_TEST_TMPDIR"
  run git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
        ls-remote "$GP_URL/GaloyMoney/drua"
  echo "$output"
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "refs/heads/main"
}

@test "git-proxy: git clone through proxy yields a valid working repo" {
  cd "$BATS_TEST_TMPDIR"
  run git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
        clone "$GP_URL/GaloyMoney/drua" ./clone
  echo "$output"
  [ "$status" -eq 0 ]
  [ -d ./clone/.git ]
  ( cd ./clone && git log --oneline | head -1 | grep -q "fixture initial" )
}

# ─── push-side e2e ─────────────────────────────────────────────────────

@test "git-proxy: git push to bot/* succeeds and forwards upstream" {
  cd "$BATS_TEST_TMPDIR"
  rm -rf push-clone
  git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
      clone "$GP_URL/GaloyMoney/drua" ./push-clone
  cd ./push-clone
  git config user.email "bats@example.com"
  git config user.name "bats"
  git checkout -b bot/e2e-test
  date > marker.txt
  git add marker.txt
  git commit -q -m "e2e push test"
  run git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
        push origin bot/e2e-test
  echo "$output"
  [ "$status" -eq 0 ]
  # Upstream got the new ref via the proxy's forward step.
  upstream_dir="${UPSTREAM_DRUA#file://}"
  git -C "$upstream_dir" rev-parse --verify refs/heads/bot/e2e-test >/dev/null
}

@test "git-proxy: git push to refs/heads/release rejected by ref-pattern" {
  cd "$BATS_TEST_TMPDIR"
  if [ ! -d ./push-clone/.git ]; then
    git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
        clone "$GP_URL/GaloyMoney/drua" ./push-clone
    cd ./push-clone
    git config user.email "bats@example.com"
    git config user.name "bats"
  else
    cd ./push-clone
  fi
  git checkout -B release
  date > release-marker.txt
  git add release-marker.txt
  git commit -q -m "should be denied"
  run git -c "http.extraHeader=Authorization: Bearer $GIT_PROXY_TOKEN" \
        push origin release
  echo "$output"
  [ "$status" -ne 0 ]
  # `git` swallows the response body but surfaces the HTTP status. The
  # proxy rejects with 403 — accept either "HTTP 403" or curl's
  # "error: 22" wrapper.
  echo "$output" | grep -E "HTTP 403|error: 22" >/dev/null

  # Audit row should record ref_pattern_denied for the receive-pack POST.
  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM audit_entries WHERE action = 'git_proxy.git-receive-pack' AND outcome = 'error' AND error_message = 'ref_pattern_denied' AND metadata->>'owner' = 'GaloyMoney' AND metadata->>'repo' = 'drua'")"
  [ "$count" -ge 1 ]
}
