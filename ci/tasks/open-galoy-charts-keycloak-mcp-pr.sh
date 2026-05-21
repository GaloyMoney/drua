#!/usr/bin/env bash

# Opens (or re-opens) a PR against GaloyMoney/galoy-charts for the bot
# branch that `bump-galoy-charts-keycloak-mcp.sh` force-pushes to.
# Closes any existing PR on the branch first because the bot branch is
# continuously force-pushed, and the goal is one always-current open PR.
#
# Auth: mints a short-lived installation token from the shared galoybot
# GitHub App. `GH_APP_PRIVATE_KEY` is the base64-encoded PEM stored in
# Vault and is passed straight through to `gh-token -b`.
#
# `nix shell` pulls `gh-token`, `gh`, `jq`, `yq-go` and `coreutils`
# since the base nix-flakes image has none of them.

set -euo pipefail

: "${GH_APP_ID:?GH_APP_ID must be set}"
: "${GH_APP_PRIVATE_KEY:?GH_APP_PRIVATE_KEY must be set (base64-encoded PEM)}"
: "${BOT_BRANCH:?BOT_BRANCH must be set}"
: "${BASE_BRANCH:?BASE_BRANCH must be set}"
: "${TARGET_REPO:?TARGET_REPO must be set (owner/repo)}"

exec nix shell \
  nixpkgs#gh-token \
  nixpkgs#gh \
  nixpkgs#jq \
  nixpkgs#yq-go \
  nixpkgs#coreutils \
  --command bash <<SCRIPT
set -euo pipefail

DIGEST=\$(yq '.keycloakMcp.image.digest' galoy-charts-repo/charts/galoy-deps/values.yaml)

cat > /tmp/pr-body.md <<BODY
Auto-bump of \\\`keycloakMcp.image.digest\\\` in \\\`charts/galoy-deps/values.yaml\\\` to the latest \\\`keycloak-mcp:edge\\\` digest published by drua CI:

\\\`\\\`\\\`
\${DIGEST}
\\\`\\\`\\\`

The drua CI bot force-pushes this branch on every new image, so this PR is always current. Pinning the digest instead of tracking the mutable \\\`:edge\\\` tag means a drua CI push does not silently roll the Keycloak MCP deployment.

Opened by GaloyMoney/drua CI via \\\`ci/tasks/open-galoy-charts-keycloak-mcp-pr.sh\\\`.
BODY

export GH_TOKEN="\$(gh-token generate -b "\${GH_APP_PRIVATE_KEY}" -i "\${GH_APP_ID}" | jq -r '.token')"

gh pr close "\${BOT_BRANCH}" --repo "\${TARGET_REPO}" || true
gh pr create \\
  --repo "\${TARGET_REPO}" \\
  --title "chore(deps): bump keycloak-mcp image digest" \\
  --body-file /tmp/pr-body.md \\
  --base "\${BASE_BRANCH}" \\
  --head "\${BOT_BRANCH}" \\
  --label galoybot
SCRIPT
