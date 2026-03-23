#!/bin/bash

set -euo pipefail

cd repo

COMMITHASH=$(git rev-parse HEAD)
BUILDTIME=$(date -u +"%Y-%m-%d-%H:%M:%S")

cat > .env <<EOF
COMMITHASH=${COMMITHASH}
BUILDTIME=${BUILDTIME}
EOF

echo "Docker build environment prepared:"
cat .env
