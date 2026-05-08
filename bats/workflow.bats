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
load library-helpers

setup_file() {
  reset_library_tables \
    workflow_run_events workflow_runs \
    workflow_definition_events workflow_definitions
  setup_isolated_library runtime/workflows
  create_test_agent
}

teardown_file() {
  stop_server
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

  # 2. Manual-trigger workflow with two tool_steps. Step 1 calls
  #    `whoami` to surface the synthesised auth subject; step 2
  #    interpolates the trigger payload AND step 1's output into a
  #    `notes store` call. The second step exists specifically to
  #    exercise `${{ trigger.* }}` and `${{ steps.<n>.outputs.* }}`
  #    substitution at runtime — the resulting note's `title` and
  #    `tags` are surfaced in the step's structured output, which
  #    `format_run` echoes back so the assertions below can grep
  #    for the substituted values.
  run admin_call "workflow" "$(jq -nc --arg pid "$project_id" '{
    command: "create",
    project_id: $pid,
    name: "tool-step-whoami",
    manual: true,
    steps: [
      { type: "tool_step", name: "identify", tool: "whoami", params: {} },
      {
        type: "tool_step",
        name: "store-note",
        tool: "notes",
        params: {
          command: "store",
          title: "run-${{ steps.identify.outputs.workflow_run_id }}",
          content: "Build ${{ trigger.payload.build }} on ${{ trigger.payload.pipeline }} acked by ${{ steps.identify.outputs.type }}",
          tags: [
            "${{ trigger.payload.pipeline }}",
            "build-${{ trigger.payload.build }}"
          ]
        }
      }
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

  # 5. Pull each step's structured output out of `format_run`'s
  #    inline-JSON envelope. `format_run` emits one
  #    `      output: { … }` line per step; sed peels them off in
  #    declaration order.
  local outputs_text
  outputs_text="$(echo "$output" | jq -r '.result.content[0].text')"
  local step1_output step2_output
  step1_output="$(echo "$outputs_text" | sed -n 's/^[[:space:]]*output: //p' | sed -n '1p')"
  step2_output="$(echo "$outputs_text" | sed -n 's/^[[:space:]]*output: //p' | sed -n '2p')"
  echo "step1 output: $step1_output"
  echo "step2 output: $step2_output"

  # 6. Step 1 (whoami) — synthesised identity must report the
  #    workflow_executor variant, this project's id, and the implicit
  #    `ProjectAdmin` scope minted by `AuthSubject::workflow_executor`.
  [ "$(echo "$step1_output" | jq -r '.type')" = "workflow_executor" ]
  [ "$(echo "$step1_output" | jq -r '.project_id')" = "$project_id" ]
  [[ "$(echo "$step1_output" | jq -r '.scopes[]')" == *"project:$project_id:admin"* ]]
  local run_id_from_step1
  run_id_from_step1="$(echo "$step1_output" | jq -r '.workflow_run_id')"
  [ "$run_id_from_step1" = "$run_id" ]

  # 7. Step 2 (notes store) — proves that `${{ trigger.* }}` and
  #    `${{ steps.<n>.outputs.* }}` references substituted at run
  #    time. The stored note's title and tags surface in the step's
  #    structured output; the title carries the cross-step run_id
  #    reference, the tags carry the trigger payload (one as a
  #    whole-string splice, one as embedded interpolation).
  [ "$(echo "$step2_output" | jq -r '.title')" = "run-$run_id" ]
  local tags
  tags="$(echo "$step2_output" | jq -r '.tags | join(",")')"
  [[ "$tags" == *"galoy-bank"* ]]
  [[ "$tags" == *"build-1234"* ]]

  # 8. Read-back path: `run` after the run is already terminal must
  #    surface the same shape.
  run admin_call "workflow" "$(jq -nc --arg rid "$run_id" '{
    command: "run", run_id: $rid
  }')"
  echo "$output"
  [[ "$output" == *"state: succeeded"* ]]
  [[ "$output" == *"workflow_executor"* ]]
}

@test "workflow: null trigger payload doesn't leak Bool(false) into substituted params" {
  # Regression test for a bug surfaced by a manual run with no
  # payload: CEL's `null.field` evaluates to `Bool(false)` rather
  # than raising `NoSuchKey`, so `${{ trigger.payload.X }}` got
  # spliced as `false` and the consuming tool's deserializer
  # exploded with `expected a string, got boolean false`.
  #
  # Fix coerces non-object trigger payloads to an empty object
  # before binding so the resolver's normal missing-key →
  # null path runs unchanged. The downstream step still errors
  # (notes' tags are `Vec<String>` and reject null), but the
  # error now names the right thing — `null` instead of the
  # mysterious `false`. This test asserts both: the run errors
  # cleanly, and the error mentions `null` rather than `boolean`.

  local suffix
  suffix="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  local proj_name="proj-toolstep-null-$suffix"

  run graphql_query "mutation { projectCreate(input: { name: \"$proj_name\" }) { project { id } } }" "$AGENT_TOKEN"
  local project_id
  project_id="$(echo "$output" | jq -r '.data.projectCreate.project.id')"
  [ -n "$project_id" ] && [ "$project_id" != "null" ]

  # Same workflow shape as the happy-path test — depends on
  # `${{ trigger.payload.* }}` for content and tags.
  run admin_call "workflow" "$(jq -nc --arg pid "$project_id" '{
    command: "create",
    project_id: $pid,
    name: "tool-step-null",
    manual: true,
    steps: [
      { type: "tool_step", name: "identify", tool: "whoami", params: {} },
      {
        type: "tool_step",
        name: "store-note",
        tool: "notes",
        params: {
          command: "store",
          title: "run-${{ steps.identify.outputs.workflow_run_id }}",
          content: "Build ${{ trigger.payload.build }} on ${{ trigger.payload.pipeline }}",
          tags: [
            "${{ trigger.payload.pipeline }}",
            "build-${{ trigger.payload.build }}"
          ]
        }
      }
    ]
  }')"
  echo "$output"
  local def_id
  def_id="$(extract_id_field "$output")"
  [ -n "$def_id" ] || { echo "could not extract definition id"; return 1; }

  # Trigger with NO payload — admin accepts a missing `payload`
  # field; the run lands with `trigger_context: null`.
  run admin_call "workflow" "$(jq -nc --arg did "$def_id" '{
    command: "trigger",
    definition_id: $did
  }')"
  echo "$output"
  local run_id
  run_id="$(extract_id_field "$output")"
  [ -n "$run_id" ] || { echo "could not extract run id"; return 1; }

  run admin_call "workflow" "$(jq -nc --arg rid "$run_id" '{
    command: "await_run",
    run_id: $rid,
    timeout_seconds: 60
  }')"
  echo "$output"
  # Step 1 (whoami, no payload deps) should succeed. Step 2
  # (notes, depends on missing payload fields) errors — but
  # cleanly, with `null`, not `false`. The run rolls up to
  # `errored`.
  [[ "$output" == *"state: errored"* ]]
  [[ "$output" == *"workflow_executor"* ]]
  [[ "$output" == *"expected a string"* ]]
  [[ "$output" == *"null"* ]]
  # Regression assertion: the original symptom was `boolean false`
  # leaking through. That phrase must NOT appear.
  [[ "$output" != *"boolean \`false\`"* ]]
  [[ "$output" != *"boolean false"* ]]
}
