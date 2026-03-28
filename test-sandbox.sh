#!/usr/bin/env bash
set -uo pipefail

export GALOY_AGENTS_URL="https://dashboard.agents.galoy.io"
CLI="./target/release/sandbox-cli"

echo "=== Cleanup: delete any existing test-e2e ==="
"$CLI" --token dummy delete test-e2e 2>&1 || true
sleep 2
echo ""

echo "=== Step 1: List (should be empty) ==="
"$CLI" --token dummy list 2>&1
echo ""

echo "=== Step 2: Create sandbox 'test-e2e' ==="
"$CLI" --token dummy create test-e2e --timeout 120 2>&1
echo ""

echo "=== Step 3: List (should show test-e2e Ready) ==="
"$CLI" --token dummy list 2>&1
echo ""

echo "=== Step 4: Exec into sandbox ==="
printf 'echo hello-from-sandbox\nexit\n' | timeout 20 "$CLI" --token dummy exec test-e2e 2>&1 || true
echo ""

echo "=== Step 5: Delete sandbox ==="
"$CLI" --token dummy delete test-e2e 2>&1
echo ""

echo "=== Step 6: List (should be empty again) ==="
"$CLI" --token dummy list 2>&1
echo ""

echo "=== DONE ==="
