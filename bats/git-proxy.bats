#!/usr/bin/env bats
#
# Drua git-proxy smart-HTTP contract (per memo 019dfebc §6).
#
# This file captures the URL + auth + policy + audit contract the
# proxy must satisfy. Tests fall into two buckets:
#
#   * HTTP-layer tests (this commit, M1): hit the routes with curl,
#     assert status codes + audit-table rows. No real `git` client
#     and no upstream GitHub round-trip needed.
#
#   * git-client e2e tests (M1.5, follow-up): drive `git ls-remote`,
#     `git clone`, `git push` through the proxy. Requires the
#     `git http-backend` spawn + per-project mirror + pre-receive
#     hook + dev-mode SA-token validator (memo §6.2). All tagged
#     `skip` here so this file documents the contract today and
#     turns green incrementally as M1.5 lands.

load helpers

setup_file() {
  start_server
  create_test_agent
}

teardown_file() {
  stop_server
}

# ─── M1 — HTTP-layer contract (active) ─────────────────────────────────

@test "git-proxy: missing Authorization rejected with 401 + audit row" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    "http://localhost:4200/git/GaloyMoney/drua/info/refs?service=git-upload-pack")"
  [ "$status" = "401" ]
  grep -q 'unauthorized' "$BATS_TEST_TMPDIR/body"

  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM sandbox_git_proxy_attempts WHERE owner='GaloyMoney' AND repo='drua' AND decision='rejected' AND reject_reason='unauthorized'")"
  [ "$count" -ge 1 ]
}

@test "git-proxy: malformed repo coord rejected with 400 (no audit row)" {
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    "http://localhost:4200/git/..%2F..%2Fetc/passwd/info/refs?service=git-upload-pack")"
  # axum URL-decoding hands the handler the literal path; either way
  # we want a 4xx — never a 2xx.
  [ "$status" -ge 400 ]
  [ "$status" -lt 500 ]
}

@test "git-proxy: missing service= query rejected with 400" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    "http://localhost:4200/git/GaloyMoney/drua/info/refs")"
  [ "$status" = "400" ]
  grep -q 'missing_service_query' "$BATS_TEST_TMPDIR/body"
}

@test "git-proxy: invalid service= value rejected with 400" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    "http://localhost:4200/git/GaloyMoney/drua/info/refs?service=git-archive")"
  [ "$status" = "400" ]
  grep -q 'invalid_service' "$BATS_TEST_TMPDIR/body"
}

# AGENT_TOKEN from create_test_agent maps to AuthSubject::ExportedAgent
# (MCP creds path), not Agent. The proxy authenticates Agent only.
# This tests the fail-closed contract for non-Agent subjects.
@test "git-proxy: non-Agent subject rejected (ExportedAgent treated as unauthorized)" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    "http://localhost:4200/git/GaloyMoney/drua/info/refs?service=git-upload-pack")"
  [ "$status" = "401" ]
  grep -q 'unauthorized' "$BATS_TEST_TMPDIR/body"
}

# ─── M1.5 — git-client e2e contract (skip until backend lands) ─────────

@test "git-proxy: git ls-remote against allowed repo + Agent subject succeeds" {
  skip "M1.5: requires git http-backend spawn + dev-mode SA validator"
  cd "$(mktemp -d)"
  GIT_TERMINAL_PROMPT=0 \
    git -c http.extraHeader="Authorization: Bearer $TEST_SA_TOKEN" \
        ls-remote http://localhost:4200/git/GaloyMoney/drua-test-fixture.git
  [[ "$output" =~ "refs/heads/main" ]]
}

@test "git-proxy: git clone through proxy yields a valid working repo" {
  skip "M1.5: requires git http-backend spawn"
  cd "$(mktemp -d)"
  git -c http.extraHeader="Authorization: Bearer $TEST_SA_TOKEN" \
      clone http://localhost:4200/git/GaloyMoney/drua-test-fixture.git ./clone
  [ -d ./clone/.git ]
}

@test "git-proxy: git push to bot/* succeeds (allowed pattern)" {
  skip "M1.5: requires receive-pack + pre-receive hook + upstream forward"
  ( cd ./clone && git checkout -b bot/e2e-test && date > test.txt && \
    git add test.txt && git commit -m "e2e" && \
    git -c http.extraHeader="Authorization: Bearer $TEST_SA_TOKEN" \
        push origin bot/e2e-test )
}

@test "git-proxy: git push to refs/heads/main rejected by ref-pattern policy" {
  skip "M1.5: requires receive-pack + pre-receive hook"
  ( cd ./clone && git checkout main && \
    git commit --allow-empty -m "should fail" && \
    run git -c http.extraHeader="Authorization: Bearer $TEST_SA_TOKEN" \
            push origin main )
  [ "$status" -ne 0 ]
  [[ "$output" =~ "ref_pattern_denied" ]]
}

@test "git-proxy: expired SA token rejected with 401" {
  skip "M1.5: requires dev-mode SA validator with exp control"
}

@test "git-proxy: audit table records both accepts and rejects" {
  skip "M1.5: completion path needs the backend to actually accept something"
  rows="$(psql -t -c "SELECT decision FROM sandbox_git_proxy_attempts ORDER BY created_at DESC LIMIT 5" "$PG_CON")"
  [[ "$rows" =~ "accepted" ]]
  [[ "$rows" =~ "rejected" ]]
}
