use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use drua_core::agent::session::{BreakerConfig, CompactionConfig, ResetTimeDeltaSeconds};
use drua_core::agent::{AgentRole, Agents, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::primitives::{AuthSubject, ContextGeneration, ProjectId, UserId};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::skill::Skills;
use drua_core::toolset::{SubmitOutputTool, ToolSets, ToolSetsConfig, ToolSetsError, TopLevelTool};
use drua_core::workflow::executor::Executor;
use drua_core::workflow::repo::WorkflowDefinitionRepo;
use drua_core::workflow::run::{BudgetStopReason, NewWorkflowRun};
use drua_core::workflow::{
    default_output_schema, NewWorkflowDefinition, WorkflowRunRepo, WorkflowRunState,
    WorkflowStepDef, WorkflowTrigger,
};
use llm::prompt::{AssistantBlock, Message, ToolChoice, UserBlock};
use llm::response::StopReason;
use llm::{ModelChain, PromptRequest, PromptResponse, PromptResult, Usage};
use rmcp::model::{CallToolResult, Content, JsonObject};
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

/// Wires an `Executor` against a real pool with a fake LLM provider
/// (the returned `prompt_rx` stands in for the provider — tests drive
/// it turn by turn) and a `WorkflowStepAgent` role bound to `chain` /
/// `breaker`. Also registers `submit_output` and creates the project's
/// lead agent (required by `create_for_workflow_run_in_op`'s project
/// name lookup).
async fn build_stack(
    pool: &sqlx::PgPool,
    chain: ModelChain,
    breaker: BreakerConfig,
) -> (
    Executor,
    WorkflowDefinitionRepo,
    WorkflowRunRepo,
    Arc<Skills>,
    ProjectId,
    AuthSubject,
    mpsc::Receiver<PromptRequest>,
) {
    build_stack_inner(
        pool,
        chain,
        breaker,
        CompactionConfig::default(),
        None,
        |_| {},
    )
    .await
}

/// Same as [`build_stack`], but forces every subsequent prompt onto a
/// fresh (orphaned) thread — `reset_time_delta_seconds: 0` means any
/// positive gap since the last turn exceeds the threshold. Lets a test
/// exercise budget accumulation across a genuine thread reset without
/// needing to grow a real conversation past the token-count trigger,
/// matching the curate-live evidence (spend split across an initial
/// thread and a post-refresh one) — handoff §7's "compacts or refreshes
/// the thread between two charges" row.
async fn build_stack_with_thread_reset(
    pool: &sqlx::PgPool,
    chain: ModelChain,
    breaker: BreakerConfig,
) -> (
    Executor,
    WorkflowDefinitionRepo,
    WorkflowRunRepo,
    Arc<Skills>,
    ProjectId,
    AuthSubject,
    mpsc::Receiver<PromptRequest>,
) {
    let compaction = CompactionConfig {
        reset_time_delta_seconds: Some(ResetTimeDeltaSeconds(0)),
        ..CompactionConfig::default()
    };
    build_stack_inner(pool, chain, breaker, compaction, None, |_| {}).await
}

/// Same as [`build_stack`], but also registers a real, dispatchable
/// `ping` top-level tool — for tests that need the step agent to make a
/// genuine (non-`submit_output`) tool call.
async fn build_stack_with_ping_tool(
    pool: &sqlx::PgPool,
    chain: ModelChain,
    breaker: BreakerConfig,
) -> (
    Executor,
    WorkflowDefinitionRepo,
    WorkflowRunRepo,
    Arc<Skills>,
    ProjectId,
    AuthSubject,
    mpsc::Receiver<PromptRequest>,
) {
    build_stack_inner(
        pool,
        chain,
        breaker,
        CompactionConfig::default(),
        None,
        |toolsets| {
            toolsets.register_top_level(PingTool::new());
        },
    )
    .await
}

/// Same as [`build_stack`], but wires a real `Audit` into the toolset so
/// tool-call dispatch records rows to `audit_entries` — needed to assert
/// on the audit row a step agent's tool call produces.
async fn build_stack_with_audit(
    pool: &sqlx::PgPool,
    chain: ModelChain,
    breaker: BreakerConfig,
) -> (
    Executor,
    WorkflowDefinitionRepo,
    WorkflowRunRepo,
    Arc<Skills>,
    ProjectId,
    AuthSubject,
    mpsc::Receiver<PromptRequest>,
    Arc<drua_core::audit::Audit>,
) {
    let audit = Arc::new(drua_core::audit::Audit::new(pool));
    let (executor, definitions, runs, skills, project_id, sub, prompt_rx) = build_stack_inner(
        pool,
        chain,
        breaker,
        CompactionConfig::default(),
        Some(Arc::clone(&audit)),
        |_| {},
    )
    .await;
    (
        executor,
        definitions,
        runs,
        skills,
        project_id,
        sub,
        prompt_rx,
        audit,
    )
}

async fn build_stack_inner(
    pool: &sqlx::PgPool,
    chain: ModelChain,
    breaker: BreakerConfig,
    step_agent_compaction: CompactionConfig,
    audit: Option<Arc<drua_core::audit::Audit>>,
    register_extra_tools: impl FnOnce(&ToolSets),
) -> (
    Executor,
    WorkflowDefinitionRepo,
    WorkflowRunRepo,
    Arc<Skills>,
    ProjectId,
    AuthSubject,
    mpsc::Receiver<PromptRequest>,
) {
    let (prompt_tx, prompt_rx) = mpsc::channel::<PromptRequest>(64);

    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::ProjectLead,
        RoleConfig {
            chain: Some(chain.clone()),
            compaction: Default::default(),
            breaker: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::Agent,
        RoleConfig {
            chain: Some(chain.clone()),
            compaction: Default::default(),
            breaker: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::WorkflowStepAgent,
        RoleConfig {
            chain: Some(chain.clone()),
            compaction: step_agent_compaction,
            breaker: breaker.clone(),
        },
    );
    let mut models = HashMap::new();
    for spec in chain.iter() {
        models.insert(
            spec.name.clone(),
            ModelDefaults {
                model: spec.name.clone(),
                max_tokens_per_response: 1024,
                context_window_tokens: 200_000,
                effort: None,
            },
        );
    }
    let config = AgentsConfig {
        builtin_roles,
        models,
        ..Default::default()
    };

    let toolsets = Arc::new(
        ToolSets::init(ToolSetsConfig::default(), audit, None, None)
            .await
            .expect("init toolsets"),
    );
    register_extra_tools(&toolsets);
    let sandboxes = Arc::new(
        Sandboxes::init(
            pool,
            SandboxConfig::default(),
            std::sync::Arc::new(drua_git_proxy::Allowlist::default()),
        )
        .await
        .expect("init sandboxes"),
    );
    let skills = Arc::new(Skills::new_without_library(pool, Arc::clone(&sandboxes)));
    let agents = Arc::new(Agents::new(
        pool,
        config,
        Arc::clone(&toolsets),
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    ));
    toolsets.register_top_level(SubmitOutputTool::new(Arc::clone(&agents)));

    let project_id = insert_project(pool).await;
    let sub = AuthSubject::User(UserId::new());
    agents
        .create_project_lead(&sub, project_id, "lead", "test-project")
        .await
        .expect("create lead");

    let definitions = WorkflowDefinitionRepo::new_without_library(pool);
    let runs = WorkflowRunRepo::new(pool);
    let executor = Executor::new(
        runs.clone(),
        definitions.clone(),
        Arc::clone(&agents),
        Arc::clone(&skills),
        Arc::clone(&sandboxes),
        Arc::clone(&toolsets),
        None,
    );

    (
        executor,
        definitions,
        runs,
        skills,
        project_id,
        sub,
        prompt_rx,
    )
}

/// Creates a project-scoped skill and a one-`AgentStep` workflow
/// definition + run bound to it. Returns the run id and the run's
/// canonical `started_at` (read back once, before the executor
/// touches it) so tests can compute the expected `RUN_CONTEXT` date.
async fn seed_one_step_run(
    skills: &Skills,
    definitions: &WorkflowDefinitionRepo,
    runs: &WorkflowRunRepo,
    sub: &AuthSubject,
    project_id: ProjectId,
    chain: ModelChain,
    skill_body: &str,
) -> (
    drua_core::primitives::WorkflowRunId,
    chrono::DateTime<chrono::Utc>,
) {
    // `Skills::find_by_name` fetches only the first 10 candidates
    // (by creation order) sharing a name across every scope, then
    // filters by precedence in-process — a name reused across many
    // test runs against a long-lived DB eventually pushes the
    // just-created skill out of that page. Use a unique name per
    // seed call so the lookup is never ambiguous.
    let skill_name = format!("step-skill-{}", uuid::Uuid::new_v4());
    skills
        .create(
            sub,
            project_id,
            "test-project",
            skill_name.clone(),
            "test skill".to_string(),
            skill_body.to_string(),
        )
        .await
        .expect("create skill");

    let steps = vec![WorkflowStepDef::AgentStep {
        name: "step".to_string(),
        skill: skill_name,
        sandbox: None,
        sandbox_mode: None,
        timeout_seconds: None,
        model_chain: Some(chain),
        output_schema: Box::new(default_output_schema()),
        condition: None,
    }];

    let new_definition = NewWorkflowDefinition::builder()
        .project_id(project_id)
        .name(format!("test-wf-{}", uuid::Uuid::new_v4()))
        .trigger(WorkflowTrigger::Manual { condition: None })
        .steps(steps.clone())
        .space_writes(drua_core::workflow::SpaceWritesDecl {
            mode: drua_core::workflow::SpaceWritesMode::ReadOnly,
            ..Default::default()
        })
        .build()
        .expect("build definition");
    let mut op = definitions.begin_op().await.expect("begin op");
    let definition = definitions
        .create_in_op(&mut op, new_definition)
        .await
        .expect("create definition");
    op.commit().await.expect("commit");

    let new_run = NewWorkflowRun::builder()
        .definition_id(definition.id)
        .project_id(project_id)
        .trigger_context(serde_json::json!({}))
        .steps_snapshot(steps)
        .build()
        .expect("build run");
    let run = runs.create(new_run).await.expect("create run");
    let started_at = run.started_at();
    (run.id, started_at)
}

/// Same shape as [`seed_one_step_run`], but for a workflow with one
/// `AgentStep` per entry in `skill_bodies` (named `step-0`, `step-1`, …)
/// and a `max_cost_usd` snapshot on both the definition and the run.
#[allow(clippy::too_many_arguments)]
async fn seed_steps_run_with_max_cost(
    skills: &Skills,
    definitions: &WorkflowDefinitionRepo,
    runs: &WorkflowRunRepo,
    sub: &AuthSubject,
    project_id: ProjectId,
    chain: ModelChain,
    skill_bodies: &[&str],
    max_cost_usd: f64,
) -> drua_core::primitives::WorkflowRunId {
    let mut steps = Vec::with_capacity(skill_bodies.len());
    for (i, body) in skill_bodies.iter().enumerate() {
        let skill_name = format!("step-skill-{}", uuid::Uuid::new_v4());
        skills
            .create(
                sub,
                project_id,
                "test-project",
                skill_name.clone(),
                "test skill".to_string(),
                body.to_string(),
            )
            .await
            .expect("create skill");
        steps.push(WorkflowStepDef::AgentStep {
            name: format!("step-{i}"),
            skill: skill_name,
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: Some(chain.clone()),
            output_schema: Box::new(default_output_schema()),
            condition: None,
        });
    }

    let new_definition = NewWorkflowDefinition::builder()
        .project_id(project_id)
        .name(format!("test-wf-{}", uuid::Uuid::new_v4()))
        .trigger(WorkflowTrigger::Manual { condition: None })
        .steps(steps.clone())
        .space_writes(drua_core::workflow::SpaceWritesDecl {
            mode: drua_core::workflow::SpaceWritesMode::ReadOnly,
            ..Default::default()
        })
        .max_cost_usd(Some(max_cost_usd))
        .build()
        .expect("build definition");
    let mut op = definitions.begin_op().await.expect("begin op");
    let definition = definitions
        .create_in_op(&mut op, new_definition)
        .await
        .expect("create definition");
    op.commit().await.expect("commit");

    let new_run = NewWorkflowRun::builder()
        .definition_id(definition.id)
        .project_id(project_id)
        .trigger_context(serde_json::json!({}))
        .steps_snapshot(steps)
        .max_cost_usd(Some(max_cost_usd))
        .build()
        .expect("build run");
    let run = runs.create(new_run).await.expect("create run");
    run.id
}

/// Sets a response's reported USD cost (`None` mimics a direct
/// Anthropic/OpenAI turn, which never reports one).
fn priced(mut response: PromptResponse, cost_usd: Option<f64>) -> PromptResponse {
    response.usage.cost_usd = cost_usd;
    response
}

/// Bounded wait for the next `PromptRequest`. A regression that hangs
/// the executor (rather than cleanly erroring) must fail fast in CI,
/// not eat the whole test-binary timeout.
async fn recv_prompt(rx: &mut mpsc::Receiver<PromptRequest>, what: &str) -> PromptRequest {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("prompt channel closed while waiting for {what}"))
}

fn last_user_text(prompt: &llm::Prompt) -> String {
    for msg in prompt.messages.iter().rev() {
        if let Message::User { content } = msg {
            for block in content {
                if let UserBlock::Text { text } = block {
                    return text.clone();
                }
            }
        }
    }
    panic!("no user text found in prompt: {prompt:?}");
}

fn max_tokens_response() -> PromptResponse {
    PromptResponse {
        content: Vec::new(),
        usage: Usage::default(),
        stop_reason: Some(StopReason::MaxTokens),
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("length".to_string()),
        upstream_error: None,
    }
}

fn empty_stop_response() -> PromptResponse {
    PromptResponse {
        content: Vec::new(),
        usage: Usage::default(),
        stop_reason: Some(StopReason::EndTurn),
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("stop".to_string()),
        upstream_error: None,
    }
}

/// Handoff §4.3 test fixture: the shape a stream ending with no usable
/// `finish_reason` (D1) is recorded as — `stop_reason: None` upstream maps
/// to `StopReason::Error` in the session (D2), with empty content.
fn incomplete_response() -> PromptResponse {
    PromptResponse {
        content: Vec::new(),
        usage: Usage::default(),
        stop_reason: None,
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("error".to_string()),
        upstream_error: None,
    }
}

/// A turn that stops with ONLY a `Thinking` block and no text or tool
/// call — the shape of the production incident's provider, which
/// reported reasoning tokens but is not modelled at the wire level as
/// a `Thinking` block by every provider. Must be treated the same as
/// [`empty_stop_response`]: a `Thinking` block is not content.
fn thinking_only_response() -> PromptResponse {
    PromptResponse {
        content: vec![AssistantBlock::Thinking {
            text: "internal reasoning".to_string(),
            signature: None,
        }],
        usage: Usage::default(),
        stop_reason: Some(StopReason::EndTurn),
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("stop".to_string()),
        upstream_error: None,
    }
}

fn submit_output_response(id: &str, args: serde_json::Value) -> PromptResponse {
    PromptResponse {
        content: vec![AssistantBlock::ToolUse {
            id: id.to_string(),
            name: "submit_output".to_string(),
            input: args,
        }],
        usage: Usage::default(),
        stop_reason: Some(StopReason::ToolUse),
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("tool_calls".to_string()),
        upstream_error: None,
    }
}

fn tool_use_response(id: &str, name: &str) -> PromptResponse {
    PromptResponse {
        content: vec![AssistantBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        stop_reason: Some(StopReason::ToolUse),
        model_used: None,
        upstream_provider: None,
        finish_reason: Some("tool_calls".to_string()),
        upstream_error: None,
    }
}

/// A registered top-level tool that always returns "pong". Lets a test
/// drive a real (non-`submit_output`) tool-call round trip, so an empty
/// turn's streak can be interrupted by genuine progress.
struct PingTool {
    schema: serde_json::Value,
}

impl PingTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for PingTool {
    fn name(&self) -> &str {
        "ping"
    }
    fn description(&self) -> &str {
        "Returns pong. Test-only tool."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    async fn call(
        &self,
        _subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }
}

#[tokio::test]
async fn run_agent_step_prompt_carries_run_context() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, ModelChain::new(primary), BreakerConfig::default()).await;

    let (run_id, started_at) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new("claude-haiku-4-5-20251001"),
        "Say hi.",
    )
    .await;
    let expected_date = started_at.format("%Y-%m-%d").to_string();

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let request = recv_prompt(&mut prompt_rx, "first prompt request").await;
    let text = last_user_text(&request.prompt);
    assert!(
        text.contains("TRIGGER_CONTEXT:"),
        "prompt should still carry TRIGGER_CONTEXT: {text}"
    );
    assert!(
        text.contains("RUN_CONTEXT:"),
        "prompt should carry RUN_CONTEXT: {text}"
    );
    assert!(
        text.contains(&expected_date),
        "RUN_CONTEXT should carry the run's start date {expected_date}: {text}"
    );

    request
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "hi"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
    assert_eq!(
        run.step_results[0].output,
        Some(serde_json::json!({"success": true, "output": "hi"}))
    );
}

/// Handoff §3.2: a tool call made by a workflow step agent must be
/// stamped with `workflow_run_id` and `resource_ids.workflow_step`, not
/// just the entries the executor itself writes. `submit_output` is
/// dispatched through the same `fan_out_tool_calls` -> `call_top_level_tool`
/// path as any other agent tool call, so it doubles as the tool call
/// under test here.
///
/// Demonstrated RED against the pre-fix `drive_session_loop`: its
/// `tokio::spawn` started the tool-dispatch task with a fresh, empty
/// `EventContext` (task-locals never cross an un-wrapped `tokio::spawn`
/// boundary), so `workflow_run_id` / `workflow_step` — recorded by
/// `Executor::run` and `send_message_with_choice` on the CALLING task —
/// were invisible to `call_top_level_instance`'s audit context. The
/// query below returned zero rows.
#[tokio::test]
async fn agent_step_tool_call_is_stamped_with_run_and_step() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx, audit) =
        build_stack_with_audit(
            &pool,
            ModelChain::new(primary.clone()),
            BreakerConfig::default(),
        )
        .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let request = recv_prompt(&mut prompt_rx, "first prompt request").await;
    request
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "hi"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);

    // `Audit::record_from_context` persists fire-and-forget (a detached
    // `tokio::spawn`), so poll briefly rather than assuming the row has
    // landed the instant the run future resolves.
    let query = drua_core::audit::primitives::AuditLogQuery {
        workflow_run_id: Some(run_id),
        workflow_step: Some("step".to_string()),
        entrypoint: Some("mcp: submit_output".to_string()),
        limit: 10,
        ..Default::default()
    };
    let entries = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let entries = audit.find(&query).await.expect("query audit_entries");
            if !entries.is_empty() {
                return entries;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for the submit_output audit row");

    assert_eq!(
        entries.len(),
        1,
        "exactly one submit_output tool call: {entries:?}"
    );
    let entry = &entries[0];
    assert_eq!(entry.workflow_run_id, Some(run_id));
    assert_eq!(
        entry.resource_id("workflow_step"),
        Some("step"),
        "resource_ids: {:?}",
        entry.resource_ids
    );
}

/// Test (1) from handoff §3.3: two `MaxTokens` stops (empty content),
/// then a `submit_output` call on the third turn. Demonstrated RED
/// against the pre-fix executor: the unpatched
/// `run_agent_until_submit_output` treats the first empty `MaxTokens`
/// turn as a closed reply and forces `submit_output` with
/// `tool_choice` on the very next turn — it never sends a second
/// prompt request, so this test's second `prompt_rx.recv()` would
/// see the FORCED nudge (`tool_choice: Some(Tool{..})`) with the
/// "Investigation complete" text instead of the plain continuation
/// text asserted below, and the model would have to submit_output on
/// turn 2, not turn 3.
#[tokio::test]
async fn continues_past_max_tokens_until_tool_call() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary.clone()),
        "Do a very long plan before your first tool call.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let mut continuation_texts = Vec::new();
    for i in 0..2 {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        assert!(
            matches!(request.prompt.tool_choice, None | Some(ToolChoice::Auto)),
            "turn {i} must not force tool_choice: {:?}",
            request.prompt.tool_choice
        );
        if i > 0 {
            continuation_texts.push(last_user_text(&request.prompt));
        }
        request
            .response_channel
            .send(Ok(PromptResult::Complete(max_tokens_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    let third = recv_prompt(&mut prompt_rx, "third prompt request").await;
    assert!(
        matches!(third.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "third turn must still be a plain continuation, not the forced nudge: {:?}",
        third.prompt.tool_choice
    );
    continuation_texts.push(last_user_text(&third.prompt));
    for text in &continuation_texts {
        assert!(
            text.contains("Continue from where you were"),
            "continuation prompt text: {text}"
        );
        assert!(!text.contains("Investigation complete"));
    }
    third
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Test (2) from handoff §3.3: chain `[primary, fallback]`, three
/// consecutive `MaxTokens` stops on primary trip the breaker
/// (`consecutive_max_tokens` default 3 — NOT lowered), advancing the
/// chain; the fourth prompt request lands on the fallback model.
#[tokio::test]
async fn breaker_chain_advance_interacts_with_continuation_loop() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let fallback = "claude-haiku-4-5-fallback".to_string();
    let chain = ModelChain::new(primary.clone()).with_fallback(fallback.clone());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        "Do a very long plan before your first tool call.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        assert_eq!(
            request.prompt.chain.primary.name, primary,
            "turn {i} should still be on the primary model"
        );
        request
            .response_channel
            .send(Ok(PromptResult::Complete(max_tokens_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    let fourth = recv_prompt(&mut prompt_rx, "fourth prompt request after chain advance").await;
    assert_eq!(
        fourth.prompt.chain.primary.name, fallback,
        "breaker should have advanced the chain to the fallback"
    );
    fourth
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "recovered on fallback"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Regression test for a Cursor Bugbot finding on PR #494: the
/// continuation budget was a single flat counter, never reset when
/// the breaker advanced the chain — so a fallback model's first
/// `max_tokens` stop inherited a budget already exhausted by the
/// primary and went straight to the forced `submit_output` nudge,
/// the exact defect this PR exists to fix, just one model later.
#[tokio::test]
async fn fallback_gets_its_own_continuation_budget_after_chain_advance() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let fallback = "claude-haiku-4-5-fallback".to_string();
    let chain = ModelChain::new(primary.clone()).with_fallback(fallback.clone());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        "Do a very long plan before your first tool call.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    // Primary: 3 consecutive MaxTokens stops trip the breaker and advance
    // the chain to the fallback.
    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("primary prompt #{i}")).await;
        assert_eq!(
            request.prompt.chain.primary.name, primary,
            "turn {i} should still be on the primary model"
        );
        request
            .response_channel
            .send(Ok(PromptResult::Complete(max_tokens_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    // Fallback's own first turn ALSO hits max_tokens. It must still get a
    // plain continuation, not the forced nudge — its budget must not have
    // been exhausted by the primary's streak.
    let fallback_first = recv_prompt(&mut prompt_rx, "fallback first prompt").await;
    assert_eq!(fallback_first.prompt.chain.primary.name, fallback);
    assert!(
        matches!(
            fallback_first.prompt.tool_choice,
            None | Some(ToolChoice::Auto)
        ),
        "fallback's first max_tokens stop must not force submit_output: {:?}",
        fallback_first.prompt.tool_choice
    );
    fallback_first
        .response_channel
        .send(Ok(PromptResult::Complete(max_tokens_response())))
        .expect("send response");

    // Fallback recovers on its second turn.
    let fallback_second = recv_prompt(&mut prompt_rx, "fallback second prompt").await;
    assert!(
        matches!(
            fallback_second.prompt.tool_choice,
            None | Some(ToolChoice::Auto)
        ),
        "still a plain continuation: {:?}",
        fallback_second.prompt.tool_choice
    );
    fallback_second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "recovered"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Test (3) from handoff §3.3: no fallback declared; three
/// consecutive `MaxTokens` stops trip the breaker with the chain
/// exhausted — the step errors instead of forcing a content-free
/// `submit_output`.
#[tokio::test]
async fn breaker_trip_without_fallback_errors_the_step() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary.clone()),
        "Do a very long plan before your first tool call.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        request
            .response_channel
            .send(Ok(PromptResult::Complete(max_tokens_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    handle.await.expect("join").expect("run() itself succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "chain has no fallback; no further prompt should be sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Errored);
    let reason = run.step_results[0]
        .error
        .as_ref()
        .expect("step should have errored");
    assert!(
        reason.contains("breaker tripped"),
        "reason should name the breaker: {reason}"
    );
}

/// Test (4) from handoff §3.3 — REGRESSION GUARD, must be green both
/// before and after the fix: a turn that closes with `EndTurn` text
/// and no `submit_output` call still triggers the existing forced
/// nudge (`tool_choice: Tool { name: "submit_output" }`).
#[tokio::test]
async fn end_turn_without_submit_output_still_triggers_forced_nudge() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first prompt request").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(PromptResponse {
            content: vec![AssistantBlock::Text {
                text: "I looked around but did not call a tool.".to_string(),
            }],
            usage: Usage::default(),
            stop_reason: Some(StopReason::EndTurn),
            model_used: None,
            upstream_provider: None,
            finish_reason: Some("stop".to_string()),
            upstream_error: None,
        })))
        .expect("send response");

    let second = recv_prompt(&mut prompt_rx, "forced nudge prompt").await;
    assert!(
        matches!(
            second.prompt.tool_choice,
            Some(ToolChoice::Tool { ref name }) if name == "submit_output"
        ),
        "expected forced tool_choice on the nudge: {:?}",
        second.prompt.tool_choice
    );
    let text = last_user_text(&second.prompt);
    assert!(text.contains("Investigation complete"));

    second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §3.1 test (1): an empty tool-less `Stop` (no text, no tool
/// call) is continued with a PLAIN user message — no forced
/// `tool_choice` — carrying the empty-stop continuation text, not the
/// "Investigation complete" forced-nudge text. Demonstrated RED against
/// the pre-fix executor: the unpatched `run_agent_until_submit_output`
/// only continues on `StopReason::Length`, so an empty `Stop` falls
/// straight through to `_ => break` and the second prompt request is
/// the forced nudge (`tool_choice: Tool { submit_output }`, text
/// "Investigation complete") instead of a plain continuation — this
/// test's `tool_choice` assertion and its text assertion both fail.
#[tokio::test]
async fn empty_stop_is_continued_then_submits_output() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first prompt request").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(empty_stop_response())))
        .expect("send response");

    let second = recv_prompt(&mut prompt_rx, "empty-stop continuation prompt").await;
    assert!(
        matches!(second.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "empty-stop continuation must not force tool_choice: {:?}",
        second.prompt.tool_choice
    );
    let text = last_user_text(&second.prompt);
    assert!(
        text.contains("ended without any text or tool call"),
        "continuation prompt text: {text}"
    );
    assert!(!text.contains("Investigation complete"));

    second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §3.1 test (1), thinking-only variant: a turn whose only
/// content is a `Thinking` block (no `Text`, no `ToolUse`) must be
/// treated exactly like a literally-empty turn — a stored `Thinking`
/// block is reasoning, not a reply. Without this case the fix only
/// covers the provider that returned `content: []`; a provider that
/// stores its reasoning as a `Thinking` block would silently fall
/// through to the forced nudge instead.
#[tokio::test]
async fn thinking_only_stop_is_continued_as_empty() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first prompt request").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(thinking_only_response())))
        .expect("send response");

    let second = recv_prompt(&mut prompt_rx, "empty-stop continuation prompt").await;
    assert!(
        matches!(second.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "a Thinking-only turn must not trigger the forced nudge: {:?}",
        second.prompt.tool_choice
    );
    let text = last_user_text(&second.prompt);
    assert!(
        text.contains("ended without any text or tool call"),
        "continuation prompt text: {text}"
    );

    second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §3.1 test (2): three consecutive empty stops. Pins down the
/// `MAX_EMPTY_STOP_CONTINUATIONS = 2` boundary exactly — prompts two and
/// three are plain continuations (the budgeted retries), and the fourth
/// is the forced nudge, not a fifth plain continuation. Demonstrated RED
/// against the pre-fix executor: prompt #2 alone would already be the
/// forced nudge, so this test's loop over prompts 2 and 3 would see the
/// forced `tool_choice` / "Investigation complete" text on its very
/// first iteration.
#[tokio::test]
async fn three_empty_stops_exhaust_budget_then_forced_nudge() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let mut continuation_texts = Vec::new();
    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        if i > 0 {
            assert!(
                matches!(request.prompt.tool_choice, None | Some(ToolChoice::Auto)),
                "turn {i} must be a budgeted plain continuation, not the forced nudge: {:?}",
                request.prompt.tool_choice
            );
            continuation_texts.push(last_user_text(&request.prompt));
        }
        request
            .response_channel
            .send(Ok(PromptResult::Complete(empty_stop_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }
    assert_eq!(
        continuation_texts.len(),
        2,
        "exactly two budgeted continuations (MAX_EMPTY_STOP_CONTINUATIONS)"
    );
    for text in &continuation_texts {
        assert!(text.contains("ended without any text or tool call"));
        assert!(!text.contains("Investigation complete"));
    }

    let fourth = recv_prompt(&mut prompt_rx, "fourth prompt request (forced nudge)").await;
    assert!(
        matches!(
            fourth.prompt.tool_choice,
            Some(ToolChoice::Tool { ref name }) if name == "submit_output"
        ),
        "budget exhausted; the 4th turn must be the forced nudge: {:?}",
        fourth.prompt.tool_choice
    );
    let text = last_user_text(&fourth.prompt);
    assert!(text.contains("Investigation complete"));

    fourth
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no fifth prompt should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §5 / D3: empty stops and `max_tokens` stops draw from
/// separate counters. Interleaves both stop reasons so that neither
/// budget alone would cover the full sequence (2 empty-stop
/// continuations + 2 max-tokens continuations = 4 continuations, while
/// `MAX_EMPTY_STOP_CONTINUATIONS` is 2 and the default
/// `consecutive_max_tokens` breaker limit is 3) — if the two reasons
/// shared one counter, one of the later turns would hit the forced
/// nudge instead of continuing.
#[tokio::test]
async fn empty_stop_and_max_tokens_budgets_do_not_starve_each_other() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    // P1 -> empty stop, P2 -> max_tokens, P3 -> empty stop, P4 -> max_tokens,
    // P5 -> submit_output. Each of the two continuation reasons is used
    // twice, interleaved, without either exhausting its budget early.
    let responses = [
        empty_stop_response(),
        max_tokens_response(),
        empty_stop_response(),
        max_tokens_response(),
    ];
    let mut empty_stop_continuations = 0;
    let mut max_tokens_continuations = 0;
    for (i, response) in responses.into_iter().enumerate() {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        if i > 0 {
            assert!(
                matches!(request.prompt.tool_choice, None | Some(ToolChoice::Auto)),
                "turn {i} must not be the forced nudge: {:?}",
                request.prompt.tool_choice
            );
            let text = last_user_text(&request.prompt);
            if text.contains("ended without any text or tool call") {
                empty_stop_continuations += 1;
            } else if text.contains("hit the output token limit") {
                max_tokens_continuations += 1;
            } else {
                panic!("unexpected continuation text at turn {i}: {text}");
            }
        }
        request
            .response_channel
            .send(Ok(PromptResult::Complete(response)))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    let fifth = recv_prompt(&mut prompt_rx, "fifth prompt request").await;
    assert!(
        matches!(fifth.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "neither budget should have been exhausted by the other's turns: {:?}",
        fifth.prompt.tool_choice
    );
    let fifth_text = last_user_text(&fifth.prompt);
    assert!(
        fifth_text.contains("hit the output token limit"),
        "the continuation for the final max_tokens response: {fifth_text}"
    );
    max_tokens_continuations += 1;
    fifth
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert_eq!(empty_stop_continuations, 2);
    assert_eq!(max_tokens_continuations, 2);

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Bugbot review of PR #515 (inline comment on 87f5c173,
/// executor.rs:1092): `empty_stops` was not reset alongside
/// `continuations` when the breaker advances the model chain, so a
/// fallback model inherited whatever empty-stop budget the primary had
/// already spent.
///
/// UPDATED for the handoff's D6/D7: this test's original premise — spend
/// the primary's empty-stop budget, then trip the breaker via a separate
/// `max_tokens` detour — no longer holds. Two consecutive empty stops now
/// trip the session breaker directly (D7), so the chain has already
/// advanced by the time a `max_tokens` detour would run. The two
/// consecutive empty stops below trip the breaker themselves; what this
/// test still pins is the original Bugbot concern, generalised: the
/// FALLBACK's own first turn, itself an empty stop, must still get a
/// budgeted continuation rather than inheriting a counter the primary
/// already exhausted.
#[tokio::test]
async fn empty_stop_budget_resets_on_chain_advance() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let fallback = "claude-haiku-4-5-fallback".to_string();
    let chain = ModelChain::new(primary.clone()).with_fallback(fallback.clone());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        "Do a very long plan before your first tool call.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    // Two consecutive empty stops on the primary trip the breaker (D7)
    // and advance the chain.
    for i in 0..2 {
        let request = recv_prompt(&mut prompt_rx, &format!("primary empty-stop prompt #{i}")).await;
        assert_eq!(
            request.prompt.chain.primary.name, primary,
            "turn {i} should still be on the primary model"
        );
        request
            .response_channel
            .send(Ok(PromptResult::Complete(empty_stop_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    // The fallback's first turn is ITSELF an empty stop. If the
    // executor's counters weren't reset alongside the chain advance, this
    // immediately exhausts a budget the fallback never got to spend, and
    // the next prompt is the forced nudge instead of a plain continuation.
    let fallback_first = recv_prompt(&mut prompt_rx, "fallback first prompt").await;
    assert_eq!(
        fallback_first.prompt.chain.primary.name, fallback,
        "the breaker should have advanced the chain to the fallback"
    );
    fallback_first
        .response_channel
        .send(Ok(PromptResult::Complete(empty_stop_response())))
        .expect("send response");

    let fallback_second = recv_prompt(&mut prompt_rx, "fallback second prompt").await;
    assert!(
        matches!(
            fallback_second.prompt.tool_choice,
            None | Some(ToolChoice::Auto)
        ),
        "fallback's first empty stop must not force submit_output: {:?}",
        fallback_second.prompt.tool_choice
    );
    let text = last_user_text(&fallback_second.prompt);
    assert!(
        text.contains("ended without any text or tool call"),
        "must be the plain empty-stop continuation, not the forced nudge: {text}"
    );

    fallback_second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "recovered"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §4.3 test 1: an incomplete stream (D1 — recorded with
/// `stop_reason: Error`, empty content) is continued with a PLAIN user
/// message, exactly like a literally-empty `Stop`.
#[tokio::test]
async fn incomplete_turn_is_continued_then_submits_output() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first prompt request").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(incomplete_response())))
        .expect("send response");

    let second = recv_prompt(&mut prompt_rx, "incomplete-turn continuation prompt").await;
    assert!(
        matches!(second.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "incomplete-turn continuation must not force tool_choice: {:?}",
        second.prompt.tool_choice
    );
    let text = last_user_text(&second.prompt);
    assert!(
        text.contains("ended without any text or tool call"),
        "continuation prompt text: {text}"
    );
    assert!(!text.contains("Investigation complete"));

    second
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §4.3 test 2, primary only: three consecutive incomplete turns.
/// Pins `MAX_CONSECUTIVE_EMPTY_TURNS = 2` exactly — prompts two and three
/// are plain continuations, and the fourth is the forced nudge.
#[tokio::test]
async fn three_incomplete_turns_exhaust_budget_then_forced_nudge() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let mut continuation_texts = Vec::new();
    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("prompt request #{i}")).await;
        if i > 0 {
            assert!(
                matches!(request.prompt.tool_choice, None | Some(ToolChoice::Auto)),
                "turn {i} must be a budgeted plain continuation, not the forced nudge: {:?}",
                request.prompt.tool_choice
            );
            continuation_texts.push(last_user_text(&request.prompt));
        }
        request
            .response_channel
            .send(Ok(PromptResult::Complete(incomplete_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }
    assert_eq!(
        continuation_texts.len(),
        2,
        "exactly two budgeted continuations (MAX_CONSECUTIVE_EMPTY_TURNS)"
    );
    for text in &continuation_texts {
        assert!(text.contains("ended without any text or tool call"));
        assert!(!text.contains("Investigation complete"));
    }

    let fourth = recv_prompt(&mut prompt_rx, "fourth prompt request (forced nudge)").await;
    assert!(
        matches!(
            fourth.prompt.tool_choice,
            Some(ToolChoice::Tool { ref name }) if name == "submit_output"
        ),
        "budget exhausted; the 4th turn must be the forced nudge: {:?}",
        fourth.prompt.tool_choice
    );
    let text = last_user_text(&fourth.prompt);
    assert!(text.contains("Investigation complete"));

    fourth
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no fifth prompt should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §4.3 test 3: primary + fallback, two consecutive incomplete
/// turns trip the session breaker (D7) and advance the chain — the third
/// prompt is a continuation sent to the fallback model, not a forced
/// nudge.
#[tokio::test]
async fn two_incomplete_turns_advance_chain_to_fallback() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let fallback = "claude-haiku-4-5-fallback".to_string();
    let chain = ModelChain::new(primary.clone()).with_fallback(fallback.clone());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..2 {
        let request = recv_prompt(&mut prompt_rx, &format!("primary prompt #{i}")).await;
        assert_eq!(
            request.prompt.chain.primary.name, primary,
            "turn {i} should still be on the primary model"
        );
        request
            .response_channel
            .send(Ok(PromptResult::Complete(incomplete_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    let third = recv_prompt(&mut prompt_rx, "third prompt request after chain advance").await;
    assert_eq!(
        third.prompt.chain.primary.name, fallback,
        "the breaker should have advanced the chain to the fallback"
    );
    assert!(
        matches!(third.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "the refreshed thread's first turn must not force submit_output: {:?}",
        third.prompt.tool_choice
    );
    third
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "recovered on fallback"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §4.3 test 4 — run 8's shape, primary only: incomplete, tool
/// call, incomplete, tool call, incomplete, `submit_output`. The step
/// completes with no forced nudge, which requires D6's *consecutive*
/// counting: a real tool call between each incomplete turn resets
/// `trailing_empty` back to 1, so the streak never reaches the breaker's
/// or the executor's per-turn ceiling of 2. Under the #515-era flat,
/// never-reset `empty_stops` total this replaces, the third incomplete
/// turn would have exhausted the budget and hit the forced nudge instead
/// of continuing straight to `submit_output` — this test must go RED if
/// that counter comes back.
#[tokio::test]
async fn run_8_shape_incomplete_turns_interleaved_with_tool_calls_completes_without_forced_nudge() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack_with_ping_tool(
            &pool,
            ModelChain::new(primary.clone()),
            BreakerConfig::default(),
        )
        .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..2 {
        let incomplete = recv_prompt(&mut prompt_rx, &format!("incomplete turn #{i}")).await;
        assert!(
            matches!(incomplete.prompt.tool_choice, None | Some(ToolChoice::Auto)),
            "turn {i} must not have forced tool_choice: {:?}",
            incomplete.prompt.tool_choice
        );
        incomplete
            .response_channel
            .send(Ok(PromptResult::Complete(incomplete_response())))
            .unwrap_or_else(|_| panic!("send incomplete response #{i}"));

        let tool_call = recv_prompt(&mut prompt_rx, &format!("tool call turn #{i}")).await;
        assert!(
            matches!(tool_call.prompt.tool_choice, None | Some(ToolChoice::Auto)),
            "turn {i}'s tool call must not have forced tool_choice: {:?}",
            tool_call.prompt.tool_choice
        );
        tool_call
            .response_channel
            .send(Ok(PromptResult::Complete(tool_use_response(
                &format!("tu_ping_{i}"),
                "ping",
            ))))
            .unwrap_or_else(|_| panic!("send tool call response #{i}"));
    }

    let third_incomplete = recv_prompt(&mut prompt_rx, "third incomplete turn").await;
    assert!(
        matches!(
            third_incomplete.prompt.tool_choice,
            None | Some(ToolChoice::Auto)
        ),
        "the third incomplete turn must still be a plain continuation, not the forced nudge: {:?}",
        third_incomplete.prompt.tool_choice
    );
    third_incomplete
        .response_channel
        .send(Ok(PromptResult::Complete(incomplete_response())))
        .expect("send third incomplete response");

    let submit = recv_prompt(&mut prompt_rx, "submit_output turn").await;
    assert!(
        matches!(submit.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "submit_output must arrive on a plain continuation, not the forced nudge: {:?}",
        submit.prompt.tool_choice
    );
    submit
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send submit_output response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// Handoff §4.3 test 5: seven NON-consecutive incomplete turns (each
/// separated by a real tool call, so `trailing_empty` never exceeds 1 and
/// the breaker never trips) still exhaust
/// `MAX_EMPTY_TURN_CONTINUATIONS = 6` — the per-step total that bounds
/// cost regardless of how the streak resets. The seventh falls through to
/// the forced nudge.
#[tokio::test]
async fn seven_non_consecutive_incomplete_turns_exhaust_total_budget() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack_with_ping_tool(
            &pool,
            ModelChain::new(primary.clone()),
            BreakerConfig::default(),
        )
        .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..6 {
        let incomplete = recv_prompt(&mut prompt_rx, &format!("incomplete turn #{i}")).await;
        assert!(
            matches!(incomplete.prompt.tool_choice, None | Some(ToolChoice::Auto)),
            "turn {i} must not have forced tool_choice: {:?}",
            incomplete.prompt.tool_choice
        );
        incomplete
            .response_channel
            .send(Ok(PromptResult::Complete(incomplete_response())))
            .unwrap_or_else(|_| panic!("send incomplete response #{i}"));

        let tool_call = recv_prompt(&mut prompt_rx, &format!("tool call turn #{i}")).await;
        tool_call
            .response_channel
            .send(Ok(PromptResult::Complete(tool_use_response(
                &format!("tu_ping_{i}"),
                "ping",
            ))))
            .unwrap_or_else(|_| panic!("send tool call response #{i}"));
    }

    let seventh = recv_prompt(&mut prompt_rx, "seventh incomplete turn").await;
    seventh
        .response_channel
        .send(Ok(PromptResult::Complete(incomplete_response())))
        .expect("send seventh incomplete response");

    let nudge = recv_prompt(&mut prompt_rx, "forced nudge after total budget exhausted").await;
    assert!(
        matches!(
            nudge.prompt.tool_choice,
            Some(ToolChoice::Tool { ref name }) if name == "submit_output"
        ),
        "total budget exhausted; must be the forced nudge: {:?}",
        nudge.prompt.tool_choice
    );

    nudge
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "done"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}

/// A hard LLM failure (the provider channel closes without a response)
/// stores an `Error` turn with empty content via `assistant_response_failed`
/// — but the executor never reaches `run_agent_until_submit_output`'s
/// continuation match, because `stream_agent_response` already returns
/// `StepErrored` on `ChatOutputEvent::Error`. Without this test a future
/// refactor could turn a real failure into a silent continuation loop.
#[tokio::test]
async fn hard_llm_failure_errors_the_step_without_reaching_continuation_logic() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) = build_stack(
        &pool,
        ModelChain::new(primary.clone()),
        BreakerConfig::default(),
    )
    .await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        ModelChain::new(primary),
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first prompt request").await;
    // Simulate a hard provider failure: the response channel closes
    // without ever sending a result.
    drop(first.response_channel);

    handle.await.expect("join").expect("run() itself succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "a hard failure must not trigger continuation logic — no further prompt should be sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Errored);
    let reason = run.step_results[0]
        .error
        .as_ref()
        .expect("step should have errored");
    assert!(
        reason.contains("prompt response channel closed"),
        "reason: {reason}"
    );
}

// -- max_cost_usd budget enforcement (handoff-workflow-max-cost-usd-2026-09-30.md) --

/// handoff §2/§7 "Zero limit": a `max_cost_usd: 0` run must deny the
/// very first model dispatch — no `PromptRequest` is ever sent, so this
/// test never touches `prompt_rx` at all. `Executor::run` takes `&self`,
/// so no spawn is needed; the whole run resolves synchronously.
#[tokio::test]
async fn zero_max_cost_usd_denies_first_model_dispatch() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let run_id = seed_steps_run_with_max_cost(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        &["Say hi."],
        0.0,
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    executor.run(run_id, cancel).await.expect("run completes");

    assert!(
        prompt_rx.try_recv().is_err(),
        "a zero budget must never dispatch a model request"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
    assert_eq!(run.max_cost_usd, Some(0.0));
    assert_eq!(run.spent_usd(), 0.0);
    assert_eq!(run.remaining_cost_usd(), Some(0.0));
    let stop = run.budget_stop.expect("budget_stop recorded");
    assert_eq!(stop.reason, BudgetStopReason::LimitReached);
    assert_eq!(stop.overshoot.as_dollars(), 0.0);
}

/// handoff §5/§7 "Terminal job retry/resume": once a run is
/// budget-stopped, re-running the executor job must be a clean no-op —
/// no further model dispatch, no state change.
#[tokio::test]
async fn budget_stopped_run_rejects_further_dispatch_on_retry() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let run_id = seed_steps_run_with_max_cost(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        &["Say hi."],
        0.0,
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    executor
        .run(run_id, Arc::clone(&cancel))
        .await
        .expect("first run stops on budget");
    let first = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(first.state, WorkflowRunState::BudgetExceeded);

    // Simulate the job system retrying the same run (e.g. after a
    // reschedule). `Executor::run` checks `state.is_terminal()` before
    // doing anything else.
    executor
        .run(run_id, cancel)
        .await
        .expect("retry on a terminal run is a clean no-op");

    assert!(
        prompt_rx.try_recv().is_err(),
        "a terminal budget-stopped run must never dispatch again"
    );
    let second = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(second.state, WorkflowRunState::BudgetExceeded);
    assert_eq!(second.budget_stop, first.budget_stop);
}

/// handoff §3's worked example: a $4.80 continuation followed by a
/// $0.35 `submit_output` turn persists $5.15 spent / $0.15 overshoot,
/// and stops the run — even though the threshold-crossing turn *is* the
/// one carrying `submit_output`. "Stop wins": the response is retained
/// for diagnosis, but the run must not read as a normal success
/// (handoff §7 "Threshold response contains tool calls or
/// submit_output").
#[tokio::test]
async fn overshoot_amounts_match_handoff_example_and_stop_wins_over_submit_output() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let run_id = seed_steps_run_with_max_cost(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        &["Investigate and report."],
        5.00,
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "first turn").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            max_tokens_response(),
            Some(4.80),
        ))))
        .expect("send first response");

    let second = recv_prompt(&mut prompt_rx, "continuation turn").await;
    second
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            submit_output_response("tu_1", serde_json::json!({"success": true, "output": "x"})),
            Some(0.35),
        ))))
        .expect("send second response");

    handle.await.expect("join").expect("run resolves");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no further model request after the threshold-crossing turn"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(
        run.state,
        WorkflowRunState::BudgetExceeded,
        "stop wins even though the crossing turn carried submit_output"
    );
    assert!((run.spent_usd() - 5.15).abs() < 1e-9);
    let stop = run.budget_stop.expect("budget_stop recorded");
    assert_eq!(stop.reason, BudgetStopReason::LimitReached);
    assert!((stop.overshoot.as_dollars() - 0.15).abs() < 1e-9);
    assert_eq!(run.remaining_cost_usd(), Some(0.0));
    // "No tool execution": the crossing turn's `submit_output` call is
    // never dispatched (handoff §3/§5), so the session never records
    // `OutputSubmitted` and this step reads as errored, not completed —
    // `step_results[0].output` must stay `None`. "Retained for
    // diagnosis" is satisfied one level down: `assistant_response_received`
    // durably persists the assistant's full turn (including its raw
    // `submit_output` tool-call JSON) into the session's own message
    // history unconditionally, before the budget check ever runs.
    assert_eq!(run.step_results[0].output, None);
    assert!(
        run.step_results[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("budget")),
        "step error should explain the budget stop: {:?}",
        run.step_results[0].error
    );
}

/// handoff §4/P3: a turn with no reported cost (the shape of every
/// direct-Anthropic/OpenAI response, or a healthy router fallback
/// landing on one) must block further dispatch with a distinct
/// `cost_metering_unavailable` reason — not settle as a free turn and
/// keep going (handoff §7 "Known zero versus missing usage/cost").
#[tokio::test]
async fn unresolvable_cost_blocks_dispatch_with_metering_reason() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let run_id = seed_steps_run_with_max_cost(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        &["Say hi."],
        5.00,
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let request = recv_prompt(&mut prompt_rx, "first turn").await;
    request
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            submit_output_response("tu_1", serde_json::json!({"success": true, "output": "hi"})),
            None,
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run resolves");

    assert!(prompt_rx.try_recv().is_err());

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
    let stop = run.budget_stop.expect("budget_stop recorded");
    assert_eq!(stop.reason, BudgetStopReason::CostMeteringUnavailable);
    assert_eq!(
        run.spent_usd(),
        0.0,
        "an unresolvable cost is not a known charge — it blocks, it doesn't settle as spend"
    );
    // Same "no tool execution" reasoning as the overshoot test: the
    // unpriceable turn's `submit_output` call is never dispatched, so
    // this step reads as errored, not completed.
    assert_eq!(run.step_results[0].output, None);
}

/// Regression: an unlimited run (`max_cost_usd` never set) must behave
/// exactly as it did before this feature, no matter how expensive the
/// reported turns are.
#[tokio::test]
async fn unlimited_run_ignores_cost_entirely() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), BreakerConfig::default()).await;

    let (run_id, _) =
        seed_one_step_run(&skills, &definitions, &runs, &sub, project_id, chain, "Hi").await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let request = recv_prompt(&mut prompt_rx, "first turn").await;
    request
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            submit_output_response("tu_1", serde_json::json!({"success": true, "output": "hi"})),
            Some(1_000_000.0),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run resolves");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
    assert_eq!(run.max_cost_usd, None);
    assert_eq!(run.spent_usd(), 0.0);
    assert_eq!(run.remaining_cost_usd(), None);
    assert!(run.budget_stop.is_none());
}

/// handoff §7 "Include a test that compacts or refreshes the thread
/// between two charges; this matches the actual curate-live run and
/// catches the tempting current-thread-only implementation." Forces an
/// orphaned (fresh) thread between the two turns of a single step via
/// `reset_time_delta_seconds: 0`; accumulation must still see both
/// charges. Limit and per-turn costs are chosen so that neither turn
/// alone crosses the limit, but their SUM does — a per-thread
/// implementation that forgot the pre-reset charge would wrongly let
/// the run continue.
#[tokio::test]
async fn spend_accumulates_correctly_across_a_forced_thread_reset() {
    let pool = pool().await;
    let chain = ModelChain::new("claude-haiku-4-5-20251001".to_string());
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack_with_thread_reset(&pool, chain.clone(), BreakerConfig::default()).await;

    let run_id = seed_steps_run_with_max_cost(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        &["Investigate and report."],
        0.010,
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    let first = recv_prompt(&mut prompt_rx, "pre-reset turn").await;
    first
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            max_tokens_response(),
            Some(0.005),
        ))))
        .expect("send first response");

    // The continuation prompt lands on a brand-new (orphaned) thread —
    // `reset_time_delta_seconds: 0` means any elapsed time trips it.
    let second = recv_prompt(&mut prompt_rx, "post-reset turn").await;
    second
        .response_channel
        .send(Ok(PromptResult::Complete(priced(
            submit_output_response("tu_1", serde_json::json!({"success": true, "output": "x"})),
            Some(0.006),
        ))))
        .expect("send second response");

    handle.await.expect("join").expect("run resolves");

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(
        run.state,
        WorkflowRunState::BudgetExceeded,
        "the pre-reset $0.005 must still count toward the run total"
    );
    assert!(
        (run.spent_usd() - 0.011).abs() < 1e-9,
        "spent {} should be 0.005 + 0.006, not just the post-reset charge",
        run.spent_usd()
    );
    let stop = run.budget_stop.expect("budget_stop recorded");
    assert_eq!(stop.reason, BudgetStopReason::LimitReached);
    assert!((stop.overshoot.as_dollars() - 0.001).abs() < 1e-9);
}

/// Bugbot finding: the executor's empty-turn ceiling must read
/// `breaker_config.consecutive_empty_turns`, not a hardcoded value —
/// they're documented as the same budget (D6). With the threshold
/// configured above the old hardcoded 2, three consecutive incomplete
/// turns must all be plain continuations: the third turn both stays
/// within the executor's (now correctly 3) ceiling AND simultaneously
/// trips the session breaker, advancing the chain — so the fallback's
/// first turn must land as a normal continuation, not a forced nudge.
#[tokio::test]
async fn executor_respects_a_raised_consecutive_empty_turns_threshold() {
    let pool = pool().await;
    let primary = "claude-haiku-4-5-20251001".to_string();
    let fallback = "claude-haiku-4-5-fallback".to_string();
    let chain = ModelChain::new(primary.clone()).with_fallback(fallback.clone());
    let breaker = BreakerConfig {
        consecutive_empty_turns: 3,
        ..BreakerConfig::default()
    };
    let (executor, definitions, runs, skills, project_id, sub, mut prompt_rx) =
        build_stack(&pool, chain.clone(), breaker).await;

    let (run_id, _) = seed_one_step_run(
        &skills,
        &definitions,
        &runs,
        &sub,
        project_id,
        chain,
        "Say hi.",
    )
    .await;

    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move { executor.run(run_id, cancel).await });

    for i in 0..3 {
        let request = recv_prompt(&mut prompt_rx, &format!("primary prompt #{i}")).await;
        assert_eq!(
            request.prompt.chain.primary.name, primary,
            "turn {i} should still be on the primary model"
        );
        assert!(
            matches!(request.prompt.tool_choice, None | Some(ToolChoice::Auto)),
            "turn {i} must not be forced before the configured threshold is reached: {:?}",
            request.prompt.tool_choice
        );
        request
            .response_channel
            .send(Ok(PromptResult::Complete(incomplete_response())))
            .unwrap_or_else(|_| panic!("send response #{i}"));
    }

    let fourth = recv_prompt(&mut prompt_rx, "fourth prompt request after chain advance").await;
    assert_eq!(
        fourth.prompt.chain.primary.name, fallback,
        "three consecutive incomplete turns should have tripped the breaker"
    );
    assert!(
        matches!(fourth.prompt.tool_choice, None | Some(ToolChoice::Auto)),
        "the fallback's first turn must be a normal continuation, not a forced nudge: {:?}",
        fourth.prompt.tool_choice
    );
    fourth
        .response_channel
        .send(Ok(PromptResult::Complete(submit_output_response(
            "tu_1",
            serde_json::json!({"success": true, "output": "recovered on fallback"}),
        ))))
        .expect("send response");

    handle.await.expect("join").expect("run succeeds");

    assert!(
        prompt_rx.try_recv().is_err(),
        "no forced nudge should have been sent"
    );

    let run = runs.find_by_id(run_id).await.expect("reload run");
    assert_eq!(run.state, WorkflowRunState::Succeeded);
}
