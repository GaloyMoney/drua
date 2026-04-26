#!/usr/bin/env bash

# Opens (or re-opens) a PR against GaloyMoney/galoy-charts for the bot
# branch that `bump-galoy-charts-tunnel-connector.sh` force-pushes to.
# Closes any existing PR on the branch first — the bot branch is
# continuously force-pushed, and the goal is one always-current open PR
# rather than a list of stale superseded ones.
#
# Auth: mints a short-lived installation token from the shared galoybot
# GitHub App (App 359499 — same App that opens galoy-deployments and
# galoy-charts bump PRs). `GH_APP_PRIVATE_KEY` is the base64-encoded
# PEM stored in Vault and is passed straight through to `gh-token -b`.
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

DIGEST=\$(yq '.tunnelConnector.image.digest' galoy-charts-repo/charts/galoy-deps/values.yaml)

cat > /tmp/pr-body.md <<BODY
Auto-bump of \\\`tunnelConnector.image.digest\\\` in \\\`charts/galoy-deps/values.yaml\\\` to the latest \\\`tunnel-connector:edge\\\` digest published by drua CI:

\\\`\\\`\\\`
\${DIGEST}
\\\`\\\`\\\`

The drua CI bot force-pushes this branch on every new image, so this PR is always current — merge when convenient. Pinning the digest (instead of tracking the mutable \\\`:edge\\\` tag) means a drua CI push does not silently roll a running connector.

Opened by GaloyMoney/drua CI via \\\`ci/tasks/open-galoy-charts-tunnel-connector-pr.sh\\\`.
BODY

export GH_TOKEN="\$(gh-token generate -b "\${GH_APP_PRIVATE_KEY}" -i "\${GH_APP_ID}" | jq -r '.token')"

gh pr close "\${BOT_BRANCH}" --repo "\${TARGET_REPO}" || true
gh pr create \\
  --repo "\${TARGET_REPO}" \\
  --title "chore(deps): bump tunnel-connector image digest" \\
  --body-file /tmp/pr-body.md \\
  --base "\${BASE_BRANCH}" \\
  --head "\${BOT_BRANCH}" \\
  --label galoybot \\
  --label tunnel-connector
SCRIPT
