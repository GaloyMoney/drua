#!/usr/bin/env bash
# End-to-end reproduction of the cross-replica read-your-write violation
# for space files.
#
# ## Topology
#
# Boots TWO real drua server processes — "replica A" on :4200 and
# "replica B" on :4300 — sharing one Postgres and one library upstream,
# each with its own clone. This is the production deployment's
# topology, minus the load balancer: in production, consecutive MCP
# calls from the same client routinely land on different replicas.
# Here the script plays the load balancer's unlucky hand deliberately:
# every write goes to A, every read to B.
#
# ## Contract under test
#
# A space write tool call is synchronous — it returns success only
# after its commit has been pushed upstream. Once that ack has been
# observed, a read served by ANY replica must return the written
# content. Each round below writes a unique marker through A and, as
# soon as the write call returns, reads the file back through B.
#
# The script exits 0 when every round honors the contract and 1 when
# any round serves a stale read. On current `main` every round fails.
#
# ## Why a real (remote) upstream is required
#
# The staleness window is the time between replica A's push and
# replica B finishing its next fetch. Against a real git host that
# window includes network round-trips, so it is wide — hundreds of
# milliseconds — and the read lands inside it every time. Against a
# local `file://` upstream a fetch completes in single-digit
# milliseconds and the window is too narrow to observe reliably over
# real HTTP calls, which is exactly why the bug escaped local testing.
# So this script requires REPO_URL to point at a remote repo you own
# (SSH form). Everything on its `main` branch is overwritten.
#
# ## Other knobs
#
# - `library.skill_sync_interval_secs=3600` pins each replica's
#   periodic fetch ticker to 1h, so the backstop poll cannot quietly
#   converge B mid-round: convergence can only come from the
#   cross-replica signalling path, exactly as in the immediate-read
#   case in production.
# - Each round issues the write and the read in a SINGLE curl
#   invocation (`--next`), minimizing client overhead between A's ack
#   and B's read — the same ordering guarantee an MCP client has: tool
#   call N completes before tool call N+1 is issued.
# - The marker is unique per round and the file persists between
#   rounds, so a stale read shows up as the PREVIOUS round's content —
#   the read path served an old commit, not "the file doesn't exist
#   yet".
#
# ## Prerequisites
#
# Run inside the dev shell (`direnv allow` or `nix develop`), with
# Postgres up and migrated (`make reset-deps`). Ports 4200/4300 free.
# An empty repo you own, reachable over SSH:
#
#   REPO_URL=git@github.com:you/drua-library-repro.git ./dev/read-your-write-repro.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PG_CON="${PG_CON:-postgres://user:password@localhost:5432/drua}"
ROUNDS="${ROUNDS:-5}"
A=4200
B=4300
# Unique per run: space entities live in Postgres and survive the git
# upstream reset performed below, so a reused slug would fail `create`.
SLUG="ryw-repro-$RANDOM"

if [ -z "${REPO_URL:-}" ]; then
  cat >&2 <<'EOF'
REPO_URL is required — point it at an empty repo you own, SSH form:

  REPO_URL=git@github.com:you/drua-library-repro.git ./dev/read-your-write-repro.sh

Its main branch will be overwritten. A remote upstream matters: the
staleness window this script demonstrates is the peer's push-to-fetch
latency, which a local file:// repo shrinks to unobservable
milliseconds (see the header comments).
EOF
  exit 2
fi

psql "$PG_CON" -qc "SELECT 1" >/dev/null 2>&1 || {
  echo "Postgres unreachable at $PG_CON — run 'make reset-deps' first." >&2
  exit 2
}

for port in $A $B; do
  if curl -s -o /dev/null -m 1 "http://localhost:$port/" 2>/dev/null; then
    echo "Port $port is already in use — stop whatever is listening there first." >&2
    exit 2
  fi
done

# Force-reset the upstream to an empty scaffold and render the minimal
# config both replicas boot from (see dev/local-library-setup.sh).
echo "==> Resetting library upstream ($REPO_URL) + rendering config"
REPO_URL="$REPO_URL" ./dev/local-library-setup.sh >/dev/null

echo "==> Building server binary"
cargo build -q -p drua-cli -p sandbox-tool-server

pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

# Both replicas share the config; per-replica bits are overridden via
# --set. `library.data_dir` MUST differ — each replica owns its clone,
# as each pod does in production. CODE_ASSISTANT_DB_PATH="" disables
# the code-assistant SQLite index (two processes would fight over one
# file, and it's irrelevant here).
start_replica() {
  local name="$1" port="$2" data_dir="$3"
  DRUA_CONFIG="$ROOT/tmp/drua.local.yml" PG_CON="$PG_CON" CODE_ASSISTANT_DB_PATH="" \
    "$ROOT/target/debug/drua" server \
    --set "server.port=$port" \
    --set "library.data_dir=$data_dir" \
    --set "library.skill_sync_interval_secs=3600" \
    > "$ROOT/tmp/ryw-$name.log" 2>&1 &
  pids+=($!)
}

echo "==> Starting replica A (:$A) and replica B (:$B)"
start_replica a $A "$ROOT/tmp/library-data"
start_replica b $B "$ROOT/tmp/library-data-b"

for port in $A $B; do
  for _ in $(seq 1 60); do
    curl -s -o /dev/null -m 1 "http://localhost:$port/" 2>/dev/null && break
    sleep 1
  done
  curl -s -o /dev/null -m 1 "http://localhost:$port/" 2>/dev/null || {
    echo "Replica on :$port did not become ready — see tmp/ryw-*.log" >&2
    exit 2
  }
done

# Admin-scoped MCP bearer token, seeded directly in Postgres. Both
# replicas resolve it against the same table, so one token works for
# both endpoints.
TOKEN="$(./dev/mint-mcp-token.sh)"

# The MCP transport is stateless streamable-HTTP: no initialize
# handshake, every call is a bare JSON-RPC POST. Space files are
# managed by the `drua_admin_spaces` tool, reached through the
# `call_tool` progressive-disclosure envelope.
HDRS=(-H "Content-Type: application/json"
      -H "Accept: application/json, text/event-stream"
      -H "Authorization: Bearer $TOKEN")

spaces_body() {
  jq -nc --argjson a "$1" \
    '{jsonrpc:"2.0",id:1,method:"tools/call",params:{name:"call_tool",arguments:{tool_name:"drua_admin_spaces",arguments:$a}}}'
}

echo "==> Creating space '$SLUG' via replica A"
curl -s "${HDRS[@]}" -d "$(spaces_body "{\"command\":\"create\",\"slug\":\"$SLUG\",\"description\":\"read-your-write repro\"}")" \
  "http://localhost:$A/mcp" | grep -q "Space created" || { echo "space create failed" >&2; exit 2; }

# Warm both replicas' auth/read paths so round 1 isn't measuring
# first-request setup cost, then give B a moment to converge on the
# warmup write so every round starts from an in-sync cluster.
warm_write="$(spaces_body '{"command":"edit","slug":"'"$SLUG"'","edit_op":"write","op_args":{"path":"doc.md","content":"warmup"}}')"
read_body="$(spaces_body '{"command":"view","slug":"'"$SLUG"'","view_op":"read","op_args":{"path":"doc.md"}}')"
curl -s "${HDRS[@]}" -d "$warm_write" "http://localhost:$A/mcp" >/dev/null
curl -s "${HDRS[@]}" -d "$read_body" "http://localhost:$B/mcp" >/dev/null
sleep 3

stale=0
for round in $(seq 1 "$ROUNDS"); do
  marker="round-$round-$RANDOM"
  write_body="$(spaces_body '{"command":"edit","slug":"'"$SLUG"'","edit_op":"write","op_args":{"path":"doc.md","content":"'"$marker"'"}}')"

  # One curl process, two sequential requests: the write to A, then —
  # milliseconds after A's ack — the read from B.
  out="$(curl -s "${HDRS[@]}" -d "$write_body" "http://localhost:$A/mcp" \
         --next -s "${HDRS[@]}" -d "$read_body" "http://localhost:$B/mcp")"

  echo "$out" | grep -q "Wrote space:" || { echo "round $round: write failed: $out" >&2; exit 2; }
  if echo "$out" | grep -q "$marker"; then
    echo "round $round: FRESH  (read on B returned what A just wrote)"
  else
    echo "round $round: STALE  (write acked on A; read on B returned old content)"
    stale=$((stale + 1))
  fi

  # Start the next round from a converged cluster.
  sleep 3
done

echo
if [ "$stale" -gt 0 ]; then
  echo "READ-YOUR-WRITE VIOLATED: $stale/$ROUNDS rounds served stale reads."
  exit 1
fi
echo "read-your-write held in all $ROUNDS rounds."
