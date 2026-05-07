#!/usr/bin/env bats
#
# Drua git-proxy smart-HTTP contract (per memo 019dfebc §6).
#
# Layered tests — each `@test` exercises one decision in the
# accept/reject pipeline. The git-client e2e tests (clone / push)
# light up as the M1.5 backend lands.

load helpers

setup_file() {
  gen_test_project_agent_ids
  write_git_proxy_config \
    "GaloyMoney/drua:pull,push:refs/heads/bot/*,refs/heads/main" \
    "GaloyMoney/drua-readonly:pull:refs/heads/main"
  start_server
  seed_test_project_agent
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

@test "git-proxy: missing Authorization rejected with 401 + audit row" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "401" ]
  grep -q 'unauthorized' "$BATS_TEST_TMPDIR/body"

  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM sandbox_git_proxy_attempts WHERE owner='GaloyMoney' AND repo='drua' AND decision='rejected' AND reject_reason='unauthorized'")"
  [ "$count" -ge 1 ]
}

@test "git-proxy: bogus dev-agent uuid rejected with 401" {
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer dev-agent:00000000-0000-0000-0000-000000000000" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "401" ]
}

@test "git-proxy: malformed dev-agent token rejected with 401" {
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer dev-agent:not-a-uuid" \
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

@test "git-proxy: pull on allowed (project, owner, repo) passes authz" {
  # M1: 501 returned post-authz (backend lands in M1.5). The point is
  # the response is NOT a 401/403/400 — the policy accepted.
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "501" ]
  grep -q 'policy accepted' "$BATS_TEST_TMPDIR/body"

  # Audit row should be `accepted`.
  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM sandbox_git_proxy_attempts WHERE project_id='$PROJECT_ID' AND owner='GaloyMoney' AND repo='drua' AND decision='accepted'")"
  [ "$count" -ge 1 ]
}

@test "git-proxy: pull on repo absent from project's allow-list rejected 403" {
  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer $GIT_PROXY_TOKEN" \
    "$GP_URL/attacker/exfil/info/refs$SVC_PULL")"
  [ "$status" = "403" ]
  grep -q 'repo_not_allowed' "$BATS_TEST_TMPDIR/body"

  count="$(psql "$PG_CON" -tAc "SELECT count(*) FROM sandbox_git_proxy_attempts WHERE owner='attacker' AND repo='exfil' AND decision='rejected' AND reject_reason='repo_not_allowed'")"
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

# ─── Cross-project isolation ───────────────────────────────────────────

@test "git-proxy: a different project cannot reach this project's repos" {
  # Spin up another project + agent. Its dev-agent token must NOT
  # be able to pull GaloyMoney/drua (which is allow-listed to PROJECT_ID).
  local other_proj other_agent_id
  other_proj="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  other_agent_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  local proj_short="${other_proj:0:8}"
  psql "$PG_CON" -q <<SQL
    INSERT INTO projects (id, name, created_at) VALUES ('$other_proj', 'bats-other-$proj_short', NOW());
    INSERT INTO project_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$other_proj', 0, 'initialized',
      '{"type":"initialized","id":"$other_proj","lead_agent_id":"$other_agent_id","name":"bats-other-$proj_short","description":null}',
      NOW());
    INSERT INTO agents (id, project_id, created_at) VALUES ('$other_agent_id', '$other_proj', NOW());
    INSERT INTO agent_events (id, sequence, event_type, event, recorded_at)
    VALUES ('$other_agent_id', 0, 'initialized',
      '{"type":"initialized","id":"$other_agent_id","project_id":"$other_proj","agent_role":"project_lead","name":"lead","authz_scopes":["project:$other_proj:admin"],"project_name":"bats-other-$proj_short"}',
      NOW());
SQL

  status="$(curl -s -o "$BATS_TEST_TMPDIR/body" -w '%{http_code}' \
    -H "Authorization: Bearer dev-agent:$other_agent_id" \
    "$GP_URL/GaloyMoney/drua/info/refs$SVC_PULL")"
  [ "$status" = "403" ]
  grep -q 'repo_not_allowed' "$BATS_TEST_TMPDIR/body"
}

# ─── M1.5 — git-client e2e (skip until backend lands) ──────────────────

@test "git-proxy: git ls-remote against allowed repo succeeds (M1.5)" {
  skip "M1.5: requires git http-backend spawn + per-project mirror"
}

@test "git-proxy: git clone through proxy yields a valid working repo (M1.5)" {
  skip "M1.5: requires git http-backend spawn"
}

@test "git-proxy: git push to bot/* succeeds (M1.5)" {
  skip "M1.5: requires receive-pack handler + pkt-line peek + upstream forward"
}

@test "git-proxy: git push to refs/heads/main rejected by ref-pattern (M1.5)" {
  skip "M1.5: requires receive-pack handler + pkt-line peek"
}
