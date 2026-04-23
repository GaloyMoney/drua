#!/usr/bin/env bash

# Writes the current `tunnel-connector:edge` digest into
# `charts/galoy-deps/values.yaml` in the galoy-charts clone and commits.
# The pipeline's `put: galoy-charts-repo` (with `rebase: true`) pushes
# the commit straight to galoy-charts main.
#
# Runs inside the nix-flakes Concourse image — `nix shell` pulls in
# `yq-go` and `git` from nixpkgs because the base image has neither.
#
# No-op (no commit) when the digest in values.yaml already matches —
# avoids churning identical commits on every registry-image poll.

set -euo pipefail

exec nix shell nixpkgs#yq-go nixpkgs#git nixpkgs#coreutils --command bash <<'SCRIPT'
set -euo pipefail

DIGEST=$(cat tunnel-connector-edge-image/digest)
VALUES="galoy-charts-repo/charts/galoy-deps/values.yaml"

CURRENT=$(yq '.tunnelConnector.image.digest' "$VALUES")
if [[ "$CURRENT" == "$DIGEST" ]]; then
  echo "galoy-charts tunnelConnector.image.digest already at $DIGEST — nothing to do"
  exit 0
fi

yq -i ".tunnelConnector.image.digest = \"${DIGEST}\"" "$VALUES"

if [[ -z $(git config --global user.email || true) ]]; then
  git config --global user.email "bot@galoy.io"
  git config --global user.name "Galoy CI Bot"
fi

cd galoy-charts-repo
git add -A
git status

if ! git diff --cached --exit-code; then
  git commit -m "chore(deps): bump tunnel-connector image digest to ${DIGEST}"
fi
SCRIPT
