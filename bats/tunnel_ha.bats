#!/usr/bin/env bats

load helpers
load tunnel_ha_helpers

RUNTIME_A_PORT=4300
RUNTIME_B_PORT=4301
RUNTIME_HOST="127.0.0.1"
HA_SECRET="bats-ha-tunnel-secret"
DEPLOYMENT_COUNT=6
DEPLOYMENT_SPLIT=3
if [ -z "${TUNNEL_FIXTURE_BIN:-}" ]; then
  TUNNEL_FIXTURE_BIN="cargo run --manifest-path $REPO_ROOT/Cargo.toml -p tunnel-connector --bin tunnel-fixture --"
fi
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-drua-tunnel-ha-bats}"

setup_file() {
  setup_tunnel_ha_file
}

teardown_file() {
  teardown_tunnel_ha_file
}

@test "tunnel HA: deployments route across two runtimes, survive one runtime death, and clean up dead connectors" {
  source "$BATS_FILE_TMPDIR/run.env"

  start_connectors
  wait_registry_count "$DEPLOYMENT_COUNT"
  wait_owner_count "$RUNTIME_HOST:$RUNTIME_A_PORT" "$DEPLOYMENT_SPLIT"
  wait_owner_count "$RUNTIME_HOST:$RUNTIME_B_PORT" "$DEPLOYMENT_SPLIT"

  assert_initial_split_callable "$RUNTIME_A_PORT" "two-runtimes-a"
  assert_initial_split_callable "$RUNTIME_B_PORT" "two-runtimes-b"

  stop_connector 1
  wait_registry_count "$((DEPLOYMENT_COUNT - 1))"
  start_connector_with_urls 1 "ws://$RUNTIME_HOST:$RUNTIME_B_PORT/tunnel/ws,ws://$RUNTIME_HOST:$RUNTIME_A_PORT/tunnel/ws"
  wait_owner_count "$RUNTIME_HOST:$RUNTIME_A_PORT" "$((DEPLOYMENT_SPLIT - 1))"
  wait_owner_count "$RUNTIME_HOST:$RUNTIME_B_PORT" "$((DEPLOYMENT_SPLIT + 1))"
  wait_tool_call "$RUNTIME_A_PORT" 1 "same-deployment-new-owner" "$RUNTIME_B_PORT"
  wait_tool_call "$RUNTIME_B_PORT" 1 "same-deployment-new-owner" "$RUNTIME_B_PORT"

  crash_runtime "a"
  assert_all_deployments_owned_by "$RUNTIME_B_PORT" "during-runtime-a-death" "$RUNTIME_B_PORT"
  wait_owner_count "$RUNTIME_HOST:$RUNTIME_B_PORT" "$DEPLOYMENT_COUNT"
  assert_all_deployments_owned_by "$RUNTIME_B_PORT" "after-runtime-a-death" "$RUNTIME_B_PORT"

  stop_connector 1
  stop_connector 2
  wait_registry_count "$((DEPLOYMENT_COUNT - 2))"

  for i in 1 2; do
    wait_tool_absent "$RUNTIME_B_PORT" "$i" "after-connector-death"
  done
  for i in $(seq 3 "$DEPLOYMENT_COUNT"); do
    wait_tool_call "$RUNTIME_B_PORT" "$i" "after-connector-death"
  done
}
