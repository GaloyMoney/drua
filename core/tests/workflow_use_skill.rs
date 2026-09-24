//! Acceptance tests for the workflow-skill-reload-context fix: a step
//! agent that calls `use_skill(action: "invoke", name: "<assigned skill>")`
//! must get back its step's trusted, rendered context — never the raw
//! skill body with unresolved `${{ … }}` expressions.
//!
//! These are deterministic integration tests: fixtures are built
//! directly against real repos/services and `UseSkillTool::call` is
//! invoked directly, with no model in the loop (per the handoff's
//! "no paid models needed" acceptance criteria).

use std::collections::HashMap;
use std::sync::Arc;

use drua_core::agent::session::{Sessions, TargetThread};
use drua_core::agent::{AgentRole, Agents, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::primitives::{
    AgentId, AuthSubject, ContextGeneration, ProjectId, UserId, UserMessageSource,
    WorkflowDefinitionId, WorkflowRunId,
};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::skill::Skills;
use drua_core::toolset::{ToolSets, ToolSetsConfig, TopLevelTool, UseSkillTool};
use drua_core::workflow::repo::WorkflowDefinitionRepo;
use drua_core::workflow::run::NewWorkflowRun;
use drua_core::workflow::{
    default_output_schema, NewWorkflowDefinition, WorkflowRun, WorkflowRunRepo, WorkflowStepDef,
    WorkflowTrigger,
};
use rmcp::model::JsonObject;
use tokio::sync::mpsc;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn insert_project(pool: &sqlx::PgPool) -> ProjectId {
    let id = ProjectId::new();
    let name = format!("test-project-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&name)
        .execute(pool)
        .await
        .expect("insert project");
    id
}

fn agents_config() -> AgentsConfig {
    let model_name = "claude-haiku-4-5-20251001".to_string();
    let mut builtin_roles = HashMap::new();
    for role in [
        AgentRole::ProjectLead,
        AgentRole::Agent,
        AgentRole::WorkflowStepAgent,
    ] {
        builtin_roles.insert(
            role,
            RoleConfig {
                chain: Some(llm::ModelChain::new(model_name.clone())),
                compaction: Default::default(),
                breaker: Default::default(),
            },
        );
    }
    let mut models = HashMap::new();
    models.insert(
        model_name.clone(),
        ModelDefaults {
            model: model_name,
            max_tokens_per_response: 1024,
            context_window_tokens: 200_000,
            effort: None,
        },
    );
    AgentsConfig {
        builtin_roles,
        models,
        ..Default::default()
    }
}

/// Full stack: `Agents` + `Skills` + `WorkflowRunRepo` +
/// `WorkflowDefinitionRepo` + a `UseSkillTool` wired exactly like
/// `App::init` wires it in `core/src/lib.rs`, plus a standalone
/// `Sessions` handle (same pool, same config) used to seed the
/// step-start `user_input_added` event the way
/// `Executor::run_agent_until_submit_output` would.
struct Stack {
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    runs: WorkflowRunRepo,
    definitions: WorkflowDefinitionRepo,
    sessions: Sessions,
    tool: UseSkillTool,
    project_id: ProjectId,
    sub: AuthSubject,
}

async fn build_stack(pool: &sqlx::PgPool) -> Stack {
    let (prompt_tx, _prompt_rx) = mpsc::channel::<llm::PromptRequest>(64);
    let config = agents_config();

    let toolsets = Arc::new(
        ToolSets::init(ToolSetsConfig::default(), None, None, None)
            .await
            .expect("init toolsets"),
    );
    let sandboxes = Arc::new(
        Sandboxes::init(
            pool,
            SandboxConfig::default(),
            Arc::new(drua_git_proxy::Allowlist::default()),
        )
        .await
        .expect("init sandboxes"),
    );
    let skills = Arc::new(Skills::new_without_library(pool, Arc::clone(&sandboxes)));
    let agents = Arc::new(Agents::new(
        pool,
        config.clone(),
        Arc::clone(&toolsets),
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    ));
    let sessions = Sessions::new(pool, config);
    let runs = WorkflowRunRepo::new(pool);
    let definitions = WorkflowDefinitionRepo::new_without_library(pool);

    let project_id = insert_project(pool).await;
    let sub = AuthSubject::User(UserId::new());
    agents
        .create_project_lead(&sub, project_id, "lead", "test-project")
        .await
        .expect("create lead");

    let tool = UseSkillTool::new(Arc::clone(&skills), Arc::clone(&agents), runs.clone());

    Stack {
        agents,
        skills,
        runs,
        definitions,
        sessions,
        tool,
        project_id,
        sub,
    }
}

/// Creates a project-scoped skill with a unique name (see
/// `core/tests/workflow_executor.rs::seed_one_step_run`'s comment on
/// why uniqueness matters for `find_by_name`'s 10-candidate window).
async fn create_skill(stack: &Stack, body: &str) -> String {
    let name = format!("step-skill-{}", uuid::Uuid::new_v4());
    stack
        .skills
        .create(
            &stack.sub,
            stack.project_id,
            "test-project",
            name.clone(),
            "test skill".to_string(),
            body.to_string(),
        )
        .await
        .expect("create skill");
    name
}

/// Seeds a one-`AgentStep` workflow definition + run with the given
/// `trigger_context`. Returns the ids plus the created `WorkflowRun` (its
/// event-sourced `trigger_context`/`steps_snapshot` are what
/// `render_in_workflow` reloads later).
async fn seed_run(
    stack: &Stack,
    skill_name: &str,
    step_name: &str,
    trigger_context: serde_json::Value,
) -> (WorkflowDefinitionId, WorkflowRunId, WorkflowRun) {
    let steps = vec![WorkflowStepDef::AgentStep {
        name: step_name.to_string(),
        skill: skill_name.to_string(),
        sandbox: None,
        sandbox_mode: None,
        timeout_seconds: None,
        model_chain: None,
        output_schema: Box::new(default_output_schema()),
        condition: None,
    }];

    let new_definition = NewWorkflowDefinition::builder()
        .project_id(stack.project_id)
        .name(format!("test-wf-{}", uuid::Uuid::new_v4()))
        .trigger(WorkflowTrigger::Manual { condition: None })
        .steps(steps.clone())
        .build()
        .expect("build definition");
    let mut op = stack.definitions.begin_op().await.expect("begin op");
    let definition = stack
        .definitions
        .create_in_op(&mut op, new_definition)
        .await
        .expect("create definition");
    op.commit().await.expect("commit");

    let new_run = NewWorkflowRun::builder()
        .definition_id(definition.id)
        .project_id(stack.project_id)
        .trigger_context(trigger_context)
        .steps_snapshot(steps)
        .build()
        .expect("build run");
    let run = stack.runs.create(new_run).await.expect("create run");
    (definition.id, run.id, run)
}

/// Creates the step agent exactly like `Executor::execute_step` does,
/// then seeds its session's initial prompt with `initial_prompt` —
/// standing in for `run_agent_until_submit_output`'s first
/// `send_message_with_choice` call, without driving a fake model turn.
async fn create_step_agent_with_initial_prompt(
    stack: &Stack,
    workflow_id: WorkflowDefinitionId,
    run_id: WorkflowRunId,
    step_name: &str,
    assigned_skill: &str,
    initial_prompt: &str,
) -> AgentId {
    let mut op = stack.agents.begin_op().await.expect("begin op");
    let agent = stack
        .agents
        .create_for_workflow_run_in_op(
            &mut op,
            stack.project_id,
            workflow_id,
            run_id,
            format!("workflow-{}-{step_name}", run_id.short()),
            None,
            None,
            default_output_schema(),
            step_name,
            assigned_skill,
            "test-revision",
        )
        .await
        .expect("create workflow step agent");
    op.commit().await.expect("commit");

    stack
        .sessions
        .add_user_input(
            agent.id,
            TargetThread::Main,
            UserMessageSource::Agent { agent_id: agent.id },
            initial_prompt.to_string(),
            Vec::new(),
        )
        .await
        .expect("seed initial prompt");

    agent.id
}

/// Mirrors `Executor::execute_step`'s two-pass prompt assembly closely
/// enough to serve as this test suite's oracle for "what the initial
/// prompt would have been" — used only to seed fixtures, never as
/// production code.
fn render_initial_prompt(
    raw_body: &str,
    trigger_context: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
    run: &WorkflowRun,
) -> String {
    let run_context = run.base_run_context();
    let ctx = drua_core::workflow::template::TemplateContext {
        trigger: trigger_context,
        steps: step_outputs,
        run: &run_context,
    };
    let templated_body = ctx.substitute_in_string(raw_body).expect("substitute");
    let pretty = serde_json::to_string_pretty(trigger_context).unwrap();
    let run_pretty = serde_json::to_string_pretty(&run_context).unwrap();
    format!(
        "{templated_body}\n\nTRIGGER_CONTEXT:\n```json\n{pretty}\n```\n\nRUN_CONTEXT:\n```json\n{run_pretty}\n```"
    )
}

fn invoke_args(name: &str, arguments: Option<&str>) -> Option<JsonObject> {
    let mut map = serde_json::Map::new();
    map.insert("action".into(), serde_json::json!("invoke"));
    map.insert("name".into(), serde_json::json!(name));
    if let Some(a) = arguments {
        map.insert("arguments".into(), serde_json::json!(a));
    }
    Some(map)
}

fn tool_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("tool result has text content")
}

// ---------------------------------------------------------------------
// Test 1 — distinctive pack carried through both the initial prompt and
// a direct use_skill reload; no unresolved workflow expressions.
// ---------------------------------------------------------------------
#[tokio::test]
async fn reload_of_assigned_skill_carries_the_same_evidence_pack_as_the_initial_prompt() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let canary = format!("CANARY-{}", uuid::Uuid::new_v4());
    let skill_name = create_skill(
        &stack,
        "Evidence pack:\n${{ steps.prepare.outputs.pack }}\n\nRun id: ${{ run.id }}",
    )
    .await;

    let (workflow_id, run_id, mut run) =
        seed_run(&stack, &skill_name, "vet", serde_json::json!({})).await;
    run.step_completed("prepare".to_string(), serde_json::json!({ "pack": canary }))
        .did_execute();
    stack.runs.update(&mut run).await.expect("persist step");

    let raw_body: String = stack
        .skills
        .find_by_name(&skill_name, Some(stack.project_id), None)
        .await
        .expect("find skill")
        .expect("skill exists")
        .into();
    let mut step_outputs = HashMap::new();
    step_outputs.insert("prepare".to_string(), serde_json::json!({ "pack": canary }));
    let initial_prompt =
        render_initial_prompt(&raw_body, &run.trigger_context, &step_outputs, &run);

    let agent_id = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id,
        run_id,
        "vet",
        &skill_name,
        &initial_prompt,
    )
    .await;

    // Sanity: the initial prompt itself carries the pack and no
    // unresolved workflow expressions.
    assert!(initial_prompt.contains(&canary));
    assert!(!initial_prompt.contains("${{"));

    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());
    let result = stack
        .tool
        .call(&subject, invoke_args(&skill_name, None))
        .await
        .expect("use_skill invoke");
    let text = tool_text(&result);

    assert_eq!(
        text, initial_prompt,
        "reload must replay the exact initial prompt, byte for byte"
    );
    assert!(text.contains(&canary), "reload must carry the same pack");
    assert!(
        !text.contains("${{"),
        "reload must not contain unresolved workflow expressions: {text}"
    );
    assert!(
        text.contains(&run_id.to_string()),
        "reload must carry the correct run id"
    );
}

// ---------------------------------------------------------------------
// Test 2 — stable after "restart" (fresh service instances against the
// same DB) and after the library skill is edited mid-run: reload still
// returns the frozen initial snapshot.
// ---------------------------------------------------------------------
#[tokio::test]
async fn reload_survives_restart_and_mid_run_library_edit() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let original_pack = "ORIGINAL-EVIDENCE";
    let skill_name = create_skill(&stack, "Evidence: ${{ steps.prepare.outputs.pack }}").await;

    let (workflow_id, run_id, mut run) =
        seed_run(&stack, &skill_name, "vet", serde_json::json!({})).await;
    run.step_completed(
        "prepare".to_string(),
        serde_json::json!({ "pack": original_pack }),
    )
    .did_execute();
    stack.runs.update(&mut run).await.expect("persist step");

    let raw_body: String = stack
        .skills
        .find_by_name(&skill_name, Some(stack.project_id), None)
        .await
        .expect("find skill")
        .expect("skill exists")
        .into();
    let mut step_outputs = HashMap::new();
    step_outputs.insert(
        "prepare".to_string(),
        serde_json::json!({ "pack": original_pack }),
    );
    let initial_prompt =
        render_initial_prompt(&raw_body, &run.trigger_context, &step_outputs, &run);

    let agent_id = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id,
        run_id,
        "vet",
        &skill_name,
        &initial_prompt,
    )
    .await;

    // Mid-run library edit: the skill body changes to reference a
    // DIFFERENT (never-populated) step, so a naive re-render would blow
    // up or silently drop the evidence.
    let skill = stack
        .skills
        .list_for_project(&stack.sub, stack.project_id)
        .await
        .expect("list")
        .into_iter()
        .find(|s| s.name == skill_name)
        .expect("skill present");
    stack
        .skills
        .update(
            &stack.sub,
            skill.id,
            stack.project_id,
            None,
            None,
            Some("EDITED — evidence removed: ${{ steps.nonexistent.outputs.x }}".to_string()),
        )
        .await
        .expect("edit skill mid-run");

    // "Restart": a brand-new `Agents`/`UseSkillTool` stack against the
    // same pool, sharing no in-memory state with the one above.
    let restarted = build_stack(&pool).await;
    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());

    let result = restarted
        .tool
        .call(&subject, invoke_args(&skill_name, None))
        .await
        .expect("use_skill invoke after restart");
    let text = tool_text(&result);

    assert_eq!(
        text, initial_prompt,
        "reload after restart + mid-run edit must still return the initial snapshot"
    );
    assert!(text.contains(original_pack));
    assert!(!text.contains("EDITED"));
    assert!(!text.contains("nonexistent"));
}

// ---------------------------------------------------------------------
// Test 3 — cross-run isolation: two concurrent runs of the same skill
// with different canary packs. Each agent can only reload its own run's
// context, including via AgentOnBehalfOfUser, and a forged `arguments`
// string cannot switch runs.
// ---------------------------------------------------------------------
#[tokio::test]
async fn concurrent_runs_of_the_same_skill_stay_isolated() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let skill_name = create_skill(&stack, "Evidence: ${{ steps.prepare.outputs.pack }}").await;

    let canary_a = "CANARY-RUN-A";
    let canary_b = "CANARY-RUN-B";

    let (workflow_id_a, run_id_a, mut run_a) =
        seed_run(&stack, &skill_name, "vet", serde_json::json!({})).await;
    run_a
        .step_completed(
            "prepare".to_string(),
            serde_json::json!({ "pack": canary_a }),
        )
        .did_execute();
    stack.runs.update(&mut run_a).await.expect("persist a");

    let (workflow_id_b, run_id_b, mut run_b) =
        seed_run(&stack, &skill_name, "vet", serde_json::json!({})).await;
    run_b
        .step_completed(
            "prepare".to_string(),
            serde_json::json!({ "pack": canary_b }),
        )
        .did_execute();
    stack.runs.update(&mut run_b).await.expect("persist b");

    let raw_body: String = stack
        .skills
        .find_by_name(&skill_name, Some(stack.project_id), None)
        .await
        .expect("find skill")
        .expect("skill exists")
        .into();

    let mut steps_a = HashMap::new();
    steps_a.insert(
        "prepare".to_string(),
        serde_json::json!({ "pack": canary_a }),
    );
    let prompt_a = render_initial_prompt(&raw_body, &run_a.trigger_context, &steps_a, &run_a);

    let mut steps_b = HashMap::new();
    steps_b.insert(
        "prepare".to_string(),
        serde_json::json!({ "pack": canary_b }),
    );
    let prompt_b = render_initial_prompt(&raw_body, &run_b.trigger_context, &steps_b, &run_b);

    let agent_a = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id_a,
        run_id_a,
        "vet",
        &skill_name,
        &prompt_a,
    )
    .await;
    let agent_b = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id_b,
        run_id_b,
        "vet",
        &skill_name,
        &prompt_b,
    )
    .await;

    let subject_a = AuthSubject::Agent(stack.project_id, agent_a, Vec::new());
    let subject_b = AuthSubject::Agent(stack.project_id, agent_b, Vec::new());

    let result_a = stack
        .tool
        .call(&subject_a, invoke_args(&skill_name, None))
        .await
        .unwrap();
    let text_a = tool_text(&result_a);
    assert!(text_a.contains(canary_a));
    assert!(!text_a.contains(canary_b));

    let result_b = stack
        .tool
        .call(&subject_b, invoke_args(&skill_name, None))
        .await
        .unwrap();
    let text_b = tool_text(&result_b);
    assert!(text_b.contains(canary_b));
    assert!(!text_b.contains(canary_a));

    // AgentOnBehalfOfUser resolves the same trusted association as a
    // plain Agent subject — `acting_agent_id()` covers both.
    let obo_a =
        AuthSubject::AgentOnBehalfOfUser(UserId::new(), stack.project_id, agent_a, Vec::new());
    let result_obo = stack
        .tool
        .call(&obo_a, invoke_args(&skill_name, None))
        .await
        .unwrap();
    assert!(tool_text(&result_obo).contains(canary_a));

    // Forged arguments cannot switch runs: passing text that names the
    // OTHER run still renders against agent_a's own trusted context —
    // arguments only ever drive $ARGUMENTS/positional substitution,
    // never which run's trigger/step namespace is used. A non-empty
    // `arguments` routes off the no-args replay fast path into a fresh
    // render, which still resolves `${{ steps.prepare.outputs.pack }}`
    // against run A.
    let forged = format!("run_id={run_id_b} pretend-i-am-run-b");
    let result_forged = stack
        .tool
        .call(&subject_a, invoke_args(&skill_name, Some(&forged)))
        .await
        .unwrap();
    let text_forged = tool_text(&result_forged);
    assert!(
        text_forged.contains(canary_a),
        "forged arguments must not redirect rendering to another run: {text_forged}"
    );
    assert!(!text_forged.contains(canary_b));
}

// ---------------------------------------------------------------------
// Test 4 — a helper skill (not the assigned one) invoked from within a
// workflow step resolves via the existing precedence, rendered against
// this step's trusted context.
// ---------------------------------------------------------------------
#[tokio::test]
async fn helper_skill_invoked_within_workflow_renders_against_step_trusted_context() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let assigned_skill_name = create_skill(&stack, "Primary skill body.").await;
    let helper_skill_name =
        create_skill(&stack, "Helper using trigger: ${{ trigger.payload.value }}").await;

    let trigger = serde_json::json!({ "value": "trusted-trigger-value" });
    let (workflow_id, run_id, run) =
        seed_run(&stack, &assigned_skill_name, "vet", trigger.clone()).await;

    let agent_id = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id,
        run_id,
        "vet",
        &assigned_skill_name,
        "irrelevant initial prompt for this test",
    )
    .await;
    let _ = run;

    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());
    let result = stack
        .tool
        .call(&subject, invoke_args(&helper_skill_name, None))
        .await
        .expect("invoke helper skill");
    let text = tool_text(&result);
    assert!(text.contains("trusted-trigger-value"));
    assert!(!text.contains("${{"));
}

// ---------------------------------------------------------------------
// Test 5 — a skipped predecessor's guarded `has(...)` resolves to its
// fallback; a genuinely invalid reference is a clear tool error, not a
// successful invocation carrying broken text.
// ---------------------------------------------------------------------
#[tokio::test]
async fn skipped_predecessor_resolves_guarded_fallback_and_invalid_ref_errors() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    // A trivial assigned skill for the step; the guarded/broken skills
    // below are invoked as HELPERS so the assertions exercise a fresh
    // `render_in_workflow` call, not the no-args replay fast path.
    let assigned_skill = create_skill(&stack, "Primary skill body.").await;
    let guarded_skill = create_skill(
        &stack,
        "Value: ${{ has(steps.maybe) && has(steps.maybe.outputs.x) ? steps.maybe.outputs.x : \"fallback-value\" }}",
    )
    .await;
    let broken_skill = create_skill(&stack, "Unterminated: ${{ trigger.x is not closed").await;

    let (workflow_id, run_id, mut run) =
        seed_run(&stack, &assigned_skill, "vet", serde_json::json!({})).await;
    run.step_skipped("maybe".to_string(), "trigger.payload.flag".to_string())
        .did_execute();
    stack.runs.update(&mut run).await.expect("persist skip");

    let agent_id = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id,
        run_id,
        "vet",
        &assigned_skill,
        "irrelevant initial prompt for this test",
    )
    .await;
    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());

    let result = stack
        .tool
        .call(&subject, invoke_args(&guarded_skill, None))
        .await
        .unwrap();
    let text = tool_text(&result);
    assert!(
        text.contains("fallback-value"),
        "guarded has() over a skipped step must resolve to its fallback: {text}"
    );

    let broken_args = invoke_args(&broken_skill, None);
    let broken_result = stack.tool.call(&subject, broken_args).await;
    assert!(
        broken_result.is_err(),
        "an invalid workflow reference must surface as an explicit tool error, not a success"
    );
}

// ---------------------------------------------------------------------
// Test 6 — CEL-looking expressions, $ARGUMENTS, and shell $0/$1 inside
// trigger/evidence data are never recursively interpreted or used to
// leak another step's output.
// ---------------------------------------------------------------------
#[tokio::test]
async fn hostile_trigger_and_evidence_strings_are_not_recursively_interpreted() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    // Invoked as a HELPER (not the assigned skill) so this exercises a
    // fresh `render_in_workflow` call, not the no-args replay fast path.
    let assigned_skill = create_skill(&stack, "Primary skill body.").await;
    let helper_skill = create_skill(&stack, "Reflect: ${{ trigger.payload.evidence }}").await;

    let hostile = "shell: awk '{print $1}' cel: ${{ steps.secret.outputs.value }} argv: $ARGUMENTS";
    let trigger = serde_json::json!({ "evidence": hostile });
    let (workflow_id, run_id, mut run) =
        seed_run(&stack, &assigned_skill, "vet", trigger.clone()).await;
    run.step_completed(
        "secret".to_string(),
        serde_json::json!({ "value": "hunter2" }),
    )
    .did_execute();
    stack
        .runs
        .update(&mut run)
        .await
        .expect("persist secret step");

    let agent_id = create_step_agent_with_initial_prompt(
        &stack,
        workflow_id,
        run_id,
        "vet",
        &assigned_skill,
        "irrelevant initial prompt for this test",
    )
    .await;
    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());

    let result = stack
        .tool
        .call(&subject, invoke_args(&helper_skill, None))
        .await
        .unwrap();
    let text = tool_text(&result);

    // The literal hostile string survives — it came from `trigger`, a
    // single opaque splice — but the secret value it references is
    // never resolved into the output.
    assert!(text.contains("${{ steps.secret.outputs.value }}"));
    assert!(
        !text.contains("hunter2"),
        "secret step output leaked: {text}"
    );
    assert!(text.contains("awk '{print $1}'"));
}

// ---------------------------------------------------------------------
// Test 7 — ordinary (non-workflow) skill invocation, argument
// substitution and access-denied behavior retain their current
// contracts.
// ---------------------------------------------------------------------
#[tokio::test]
async fn ordinary_non_workflow_invocation_is_unaffected() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let skill_name = create_skill(&stack, "Deploy $ARGUMENTS to production.").await;

    let agent = stack
        .agents
        .create_agent(&stack.sub, stack.project_id, "plain-agent", None, None)
        .await
        .expect("create plain agent");
    let subject = AuthSubject::Agent(stack.project_id, agent.id, Vec::new());

    let args = invoke_args(&skill_name, Some("staging"));
    let result = stack.tool.call(&subject, args).await.unwrap();
    assert_eq!(tool_text(&result), "Deploy staging to production.");

    // Unknown skill still reports as a tool error result, not a hard Err.
    let missing_args = invoke_args("does-not-exist", None);
    let missing = stack.tool.call(&subject, missing_args).await.unwrap();
    assert!(missing.is_error.unwrap_or(false) || tool_text(&missing).contains("Unknown skill"));
}

// ---------------------------------------------------------------------
// Test 8 — a legacy workflow step agent (created before
// workflow_step/assigned_skill were tracked) fails clearly when the
// requested skill needs workflow templates, rather than returning
// unresolved expressions as a successful invocation.
// ---------------------------------------------------------------------
#[tokio::test]
async fn legacy_workflow_agent_without_recoverable_context_errors_on_templated_skill() {
    let pool = pool().await;
    let stack = build_stack(&pool).await;

    let (workflow_id, run_id, _run) =
        seed_run(&stack, "unused", "legacy-step", serde_json::json!({})).await;

    // Build a REAL `AgentEvent::Initialized` (so serialization exactly
    // matches production), then strip the three step-invocation keys to
    // simulate a row written before this migration —
    // `#[serde(default)]` is what makes this a legacy agent rather than
    // a deserialization failure.
    let agent_id = AgentId::new();
    let event = drua_core::agent::AgentEvent::Initialized {
        id: agent_id,
        project_id: stack.project_id,
        agent_role: AgentRole::WorkflowStepAgent,
        name: format!("workflow-{}-legacy-step", run_id.short()),
        authz_scopes: Vec::new(),
        project_name: "test-project".to_string(),
        workflow_id: Some(workflow_id),
        workflow_run_id: Some(run_id),
        output_schema: None,
        workflow_step: None,
        assigned_skill: None,
        assigned_skill_revision: None,
    };
    let mut event_json = serde_json::to_value(&event).expect("serialize event");
    let obj = event_json.as_object_mut().expect("object");
    obj.remove("workflow_step");
    obj.remove("assigned_skill");
    obj.remove("assigned_skill_revision");

    sqlx::query(
        "INSERT INTO agents (id, project_id, created_at, deleted, workflow_id, workflow_run_id) \
         VALUES ($1, $2, NOW(), FALSE, $3, $4)",
    )
    .bind(agent_id)
    .bind(stack.project_id)
    .bind(workflow_id)
    .bind(run_id)
    .execute(&pool)
    .await
    .expect("insert legacy agents row");
    sqlx::query(
        "INSERT INTO agent_events (id, sequence, event_type, event, recorded_at) \
         VALUES ($1, 1, 'initialized', $2, NOW())",
    )
    .bind(agent_id)
    .bind(&event_json)
    .execute(&pool)
    .await
    .expect("insert legacy agent_events row");

    let templated_skill = create_skill(&stack, "Needs: ${{ trigger.payload.x }}").await;
    let plain_skill = create_skill(&stack, "No templates here, just text.").await;

    let subject = AuthSubject::Agent(stack.project_id, agent_id, Vec::new());

    // A "clear tool error" in this codebase's convention (matching the
    // pre-existing "Unknown skill" response) is a successful RPC
    // carrying `is_error: true` — never unresolved `${{ … }}` text
    // reported as a plain success.
    let templated_args = invoke_args(&templated_skill, None);
    let templated_result = stack
        .tool
        .call(&subject, templated_args)
        .await
        .expect("call itself must not hard-fail");
    assert_eq!(
        templated_result.is_error,
        Some(true),
        "a legacy agent asking for a workflow-templated skill must get an explicit tool \
         error, never unresolved ${{{{ … }}}} text as a success"
    );
    let templated_text = tool_text(&templated_result);
    assert!(!templated_text.contains("${{"));

    // A skill with no workflow template syntax at all is harmless to
    // hand back even without a recoverable step context.
    let plain_args = invoke_args(&plain_skill, None);
    let plain_result = stack.tool.call(&subject, plain_args).await.unwrap();
    assert_eq!(tool_text(&plain_result), "No templates here, just text.");
}

// Test 9 (workflow system guidance identifies the assigned skill as
// already invoked) is a unit test in
// `core/src/agent/system_prompt.rs::workflow_role_header_states_assigned_skill_already_invoked`
// — `system_blocks_for_role` is private to the `agent` module.
