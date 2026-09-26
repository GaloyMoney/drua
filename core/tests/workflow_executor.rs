use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use drua_core::agent::session::BreakerConfig;
use drua_core::agent::{AgentRole, Agents, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::primitives::{AuthSubject, ContextGeneration, ProjectId, UserId};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::skill::Skills;
use drua_core::toolset::{SubmitOutputTool, ToolSets, ToolSetsConfig};
use drua_core::workflow::executor::Executor;
use drua_core::workflow::repo::WorkflowDefinitionRepo;
use drua_core::workflow::run::NewWorkflowRun;
use drua_core::workflow::{
    default_output_schema, NewWorkflowDefinition, WorkflowRunRepo, WorkflowRunState,
    WorkflowStepDef, WorkflowTrigger,
};
use llm::prompt::{AssistantBlock, Message, ToolChoice, UserBlock};
use llm::response::StopReason;
use llm::{ModelChain, PromptRequest, PromptResponse, PromptResult, Usage};
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
            compaction: Default::default(),
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
        ToolSets::init(ToolSetsConfig::default(), None, None, None)
            .await
            .expect("init toolsets"),
    );
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
