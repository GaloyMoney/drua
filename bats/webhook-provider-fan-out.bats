#!/usr/bin/env bats

# End-to-end test for `POST /webhooks/providers/{provider}` fan-out.
#
# Creates two workflows with `provider: concourse` and different
# trigger conditions, POSTs a single event to the provider endpoint,
# and asserts that only the matching workflow spawns a run.

load helpers
load library-helpers

setup_file() {
  reset_library_tables \
    workflow_run_events workflow_runs \
    workflow_definition_events workflow_definitions

  export WEBHOOK_SECRET_CONCOURSE="test-concourse-secret"

  setup_isolated_library runtime/workflows
  create_test_agent
}

teardown_file() {
  stop_server
}

extract_id_field() {
  echo "$1" \
    | jq -r '.result.content[0].text' \
    | grep -oE '\bid: [0-9a-f-]+' \
    | head -n1 \
    | awk '{print $2}'
}

@test "webhook provider fan-out: dispatches to matching workflows only" {
  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="proj-fanout-$suffix"

  # 1. Create project.
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  local project_id
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Create workflow A — matches pipeline == "galoy-agents".
  run admin_call "workflow" "$(jq -nc --arg pid "$project_id" '{
    command: "create",
    project_id: $pid,
    name: "concourse-agents-flow",
    provider: "concourse",
    trigger_condition: "trigger.payload.pipeline == \"galoy-agents\"",
    steps: [
      { type: "tool_step", name: "identify", tool: "whoami", params: {} }
    ]
  }')"
  echo "workflow A: $output"
  local def_a
  def_a="$(extract_id_field "$output")"
  [ -n "$def_a" ] || { echo "could not extract def A id"; return 1; }

  # 3. Create workflow B — matches pipeline == "lana-bank".
  run admin_call "workflow" "$(jq -nc --arg pid "$project_id" '{
    command: "create",
    project_id: $pid,
    name: "concourse-lana-flow",
    provider: "concourse",
    trigger_condition: "trigger.payload.pipeline == \"lana-bank\"",
    steps: [
      { type: "tool_step", name: "identify", tool: "whoami", params: {} }
    ]
  }')"
  echo "workflow B: $output"
  local def_b
  def_b="$(extract_id_field "$output")"
  [ -n "$def_b" ] || { echo "could not extract def B id"; return 1; }

  # 4. POST to provider endpoint with pipeline=galoy-agents.
  #    Only workflow A should fire.
  run curl -s -w "\n%{http_code}" \
    -X POST http://localhost:4200/webhooks/providers/concourse \
    -H "Authorization: Bearer test-concourse-secret" \
    -H "Content-Type: application/json" \
    -d '{"source":"concourse","pipeline":"galoy-agents","job":"check-code","build":"42"}'
  echo "fan-out response: $output"
  local http_code body
  http_code="$(echo "$output" | tail -1)"
  body="$(echo "$output" | sed '$d')"
  [ "$http_code" = "200" ]
  [ "$(echo "$body" | jq -r '.triggered')" = "1" ]
  [ "$(echo "$body" | jq -r '.filtered')" = "1" ]
  [ "$(echo "$body" | jq -r '.errored')" = "0" ]

  # 5. Verify workflow A has a run.
  run admin_call "workflow" "$(jq -nc --arg did "$def_a" '{
    command: "runs", definition_id: $did
  }')"
  echo "workflow A runs: $output"
  [[ "$output" == *"$def_a"* ]]

  # 6. Verify workflow B has NO run.
  run admin_call "workflow" "$(jq -nc --arg did "$def_b" '{
    command: "runs", definition_id: $did
  }')"
  echo "workflow B runs: $output"
  [[ "$output" == *"No runs"* ]] || [[ "$output" == *"0 run"* ]] || {
    # If there are runs listed, they shouldn't include def_b's uuid in
    # a "run_id" position — only the "definition_id" mention is fine.
    local run_count
    run_count="$(echo "$output" | jq -r '.result.content[0].text' | grep -c 'state:' || true)"
    [ "$run_count" = "0" ]
  }
}

@test "webhook provider fan-out: rejects wrong bearer token" {
  run curl -s -o /dev/null -w "%{http_code}" \
    -X POST http://localhost:4200/webhooks/providers/concourse \
    -H "Authorization: Bearer wrong-secret" \
    -H "Content-Type: application/json" \
    -d '{"pipeline":"test"}'
  [ "$output" = "401" ]
}

@test "webhook provider fan-out: returns 501 for unconfigured provider" {
  run curl -s -o /dev/null -w "%{http_code}" \
    -X POST http://localhost:4200/webhooks/providers/unknown_provider \
    -H "Authorization: Bearer anything" \
    -H "Content-Type: application/json" \
    -d '{"pipeline":"test"}'
  [ "$output" = "501" ]
}
