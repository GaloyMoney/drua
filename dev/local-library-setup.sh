#!/usr/bin/env bash
# Seeds a library upstream and renders a minimal server config pointing
# at it, so the server can boot without access to any hosted library
# repo. Idempotent; wipes prior local state and force-resets the
# upstream to an empty scaffold.
#
# By default the upstream is a local bare repo under tmp/. Set
# DRUA_LIBRARY_REPO_URL to use a remote you own instead (SSH form, e.g.
# DRUA_LIBRARY_REPO_URL=git@github.com:you/some-empty-repo.git) — the server picks
# up ssh-agent or ~/.ssh/id_* keys automatically. Everything on that
# repo's main branch is overwritten.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UPSTREAM="${DRUA_LIBRARY_REPO_URL:-$ROOT/tmp/library-upstream.git}"
WORKING="$ROOT/tmp/library-upstream-working"
DATA_DIR="$ROOT/tmp/library-data"
CONFIG="$ROOT/tmp/drua.local.yml"

rm -rf "$WORKING" "$DATA_DIR"
if [ -z "${DRUA_LIBRARY_REPO_URL:-}" ]; then
  rm -rf "$UPSTREAM"
  git init --bare --initial-branch=main "$UPSTREAM" >/dev/null
fi

mkdir -p "$WORKING"
git -C "$WORKING" init -q -b main
git -C "$WORKING" config user.email dev@drua
git -C "$WORKING" config user.name "Drua Dev"
mkdir -p "$WORKING/spaces" "$WORKING/runtime/skills" "$WORKING/runtime/projects"
touch "$WORKING/spaces/.gitkeep" "$WORKING/runtime/skills/.gitkeep" "$WORKING/runtime/projects/.gitkeep"
git -C "$WORKING" add -A
git -C "$WORKING" commit -q -m "init: library scaffold"
git -C "$WORKING" remote add origin "$UPSTREAM"
git -C "$WORKING" push -q --force origin main

cat > "$CONFIG" <<EOF
server:
  port: 4200
  host: "0.0.0.0"
  secure_cookies: false
  mcp_endpoint: "http://localhost:4200/mcp"
oauth:
  login: github
  github_client_id: "local-client-id"
  github_redirect_uri: "http://localhost:4200/auth/github/callback"
  github_allowed_teams: []
agents:
  default_chain:
    primary: { name: local-model }
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
      - name: local-model
        max_tokens_per_response: 1024
        context_window_tokens: 4096
library:
  data_dir: "$ROOT/tmp/library-data"
  repo_url: "${DRUA_LIBRARY_REPO_URL:-file://$UPSTREAM}"
  skill_sync_interval_secs: 1
sandbox:
  backend:
    provider: local
    sandbox_spawn_cmd: "true"
    local_repo_root: "."
EOF

echo "Local library upstream: $UPSTREAM"
echo "Config rendered at:     $CONFIG"
echo
echo "Start the server with:"
echo "  DRUA_CONFIG=$CONFIG make run-server"
