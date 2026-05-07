#!/usr/bin/env bats

# End-to-end glue test for `WorkflowStepDef::ToolStep`.
#
# Drives the full dispatch path with no LLM round-trips and no agent
# steps:
#
#   1. admin `workflow create` lands a manual-trigger workflow with a
#      single tool_step calling `whoami` (composable, declares an
#      `output_schema`, returns deterministic structured content).
#   2. admin `workflow trigger` spawns a run with a payload.
#   3. admin `workflow await_run` blocks until terminal and surfaces
#      the per-step structured outputs.
#
# The executor mints `AuthSubject::WorkflowExecutor(project_id, run_id)`
# at dispatch time; `whoami` introspects the subject and writes the
# resulting identity into its `structured_content`. Asserting that
# field proves the new variant is wired through every layer of the
# tool_step path.

load helpers

setup_file() {
  start_server
  create_test_agent
}

teardown_file() {
  stop_server
}

# Same admin-tier dispatch helper skills.bats uses — wraps
# `drua_admin_<tool>` in the `call_tool` meta-tool envelope.
admin_call() {
  local tool_name="$1"
  local args_json="$2"
  local body
  body="$(jq -nc --arg t "drua_admin_$tool_name" --argjson a "$args_json" '{
    name: "call_tool",
    arguments: { tool_name: $t, arguments: $a }
  }')"
  mcp_call "$AGENT_TOKEN" "tools/call" "$body"
}

# Admin tier renders `id: <uuid>` on a stable line in the create
# response's text body.
extract_id_field() {
  echo "$1" \
    | jq -r '.result.content[0].text' \
    | grep -oE '\bid: [0-9a-f-]+' \
    | head -n1 \
    | awk '{print $2}'
}

@test "workflow: tool_step dispatches whoami and surfaces workflow_executor identity" {
  # Unique-per-run names so re-runs against a persisted PG (developer
  # iteration with SKIP_COMPOSE=1) don't collide.
  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="proj-toolstep-$suffix"

  # 1. Project (auto-creates the lead agent).
  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  echo "$output"
  local project_id
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # 2. Manual-trigger workflow with a single tool_step calling `whoami`.
  #    `manual: true` keeps the trigger surface tiny — `trigger` /
  #    `await_run` fire it directly without a webhook.
  run admin_call "workflow" "$(jq -nc --arg pid "$project_id" '{
    command: "create",
    project_id: $pid,
    name: "tool-step-whoami",
    manual: true,
    steps: [
      { type: "tool_step", name: "identify", tool: "whoami", params: {} }
    ]
  }')"
  echo "$output"
  local def_id
  def_id="$(extract_id_field "$output")"
  [ -n "$def_id" ] || { echo "could not extract definition id"; return 1; }

  # 3. Trigger a run with a payload.
  run admin_call "workflow" "$(jq -nc --arg did "$def_id" '{
    command: "trigger",
    definition_id: $did,
    payload: { build: 1234, pipeline: "galoy-bank" }
  }')"
  echo "$output"
  local run_id
  run_id="$(extract_id_field "$output")"
  [ -n "$run_id" ] || { echo "could not extract run id"; return 1; }

  # 4. Block until terminal and surface step outputs in the response.
  run admin_call "workflow" "$(jq -nc --arg rid "$run_id" '{
    command: "await_run",
    run_id: $rid,
    timeout_seconds: 60
  }')"
  echo "$output"
  [[ "$output" == *"state: succeeded"* ]]

  # 5. Pull the run's structured step output out of the
  #    JSON-in-JSON envelope and assert against the parsed shape.
  #    The synthesised identity must report the workflow_executor
  #    variant, this project's id, and the implicit `ProjectAdmin`
  #    scope minted by `AuthSubject::workflow_executor`.
  local step_output
  step_output="$(echo "$output" \
    | jq -r '.result.content[0].text' \
    | sed -n 's/^[[:space:]]*output: //p' \
    | head -n1)"
  echo "step output JSON: $step_output"
  [ "$(echo "$step_output" | jq -r '.type')" = "workflow_executor" ]
  [ "$(echo "$step_output" | jq -r '.project_id')" = "$project_id" ]
  [[ "$(echo "$step_output" | jq -r '.scopes[]')" == *"project:$project_id:admin"* ]]

  # 6. Read-back path: `run` after the run is already terminal must
  #    surface the same step output shape.
  run admin_call "workflow" "$(jq -nc --arg rid "$run_id" '{
    command: "run", run_id: $rid
  }')"
  echo "$output"
  [[ "$output" == *"state: succeeded"* ]]
  [[ "$output" == *"workflow_executor"* ]]
}
