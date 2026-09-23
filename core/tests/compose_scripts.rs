#![recursion_limit = "256"]
//! Integration coverage for authorized compose script loading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use drua_core::agent::{AgentRole, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AgentId, AuthSubject, UserId};
use drua_core::{App, AppConfig};
use drua_library::CommitAttribution;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn tests_artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("compose-scripts-e2e")
}

fn fresh_dir(name: &str) -> PathBuf {
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = tests_artifact_dir().join(format!("{name}-{stamp}"));
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    dir
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// Bare upstream seeded with an initial commit so libgit2's first
/// fetch finds a HEAD ref.
fn init_bare_upstream() -> PathBuf {
    let upstream = fresh_dir("upstream").with_extension("git");
    std::fs::create_dir_all(&upstream).expect("create upstream");
    git(
        &upstream,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );

    let work = fresh_dir("seed");
    std::fs::create_dir_all(&work).expect("create work");
    git(&work, &["init", "--quiet", "--initial-branch=main"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    git(&work, &["config", "user.name", "Test"]);
    git(
        &work,
        &["remote", "add", "origin", &upstream.to_string_lossy()],
    );
    std::fs::write(work.join("README.md"), "init\n").expect("write readme");
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "-m", "initial commit"]);
    git(&work, &["push", "--quiet", "-u", "origin", "main"]);

    upstream
}

async fn reset_db(pool: &sqlx::PgPool) {
    let stmt = "TRUNCATE TABLE \
            jobs, job_events, job_executions, \
            library_documents, \
            skills, skill_events, \
            spaces, space_events, \
            session_threads, session_thread_events, \
            agent_sessions, agent_session_events, \
            agents, agent_events, \
            projects, project_events \
        RESTART IDENTITY CASCADE";
    sqlx::query(stmt)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("reset_db failed: {e}"));
}

/// Minimal `AgentsConfig` satisfying `validate()` — `App::init`
/// requires `ProjectLead` + `Agent` roles and a matching model entry.
fn agents_config_for_tests() -> AgentsConfig {
    let model = "test-model".to_string();
    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::ProjectLead,
        RoleConfig {
            chain: Some(llm::ModelChain::new(model.clone())),
            compaction: Default::default(),
            breaker: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::Agent,
        RoleConfig {
            chain: Some(llm::ModelChain::new(model.clone())),
            compaction: Default::default(),
            breaker: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::WorkflowStepAgent,
        RoleConfig {
            chain: Some(llm::ModelChain::new(model.clone())),
            compaction: Default::default(),
            breaker: Default::default(),
        },
    );
    let mut models = HashMap::new();
    models.insert(
        model.clone(),
        ModelDefaults {
            model,
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

async fn setup(test_name: &str) -> (App, AuthSubject, AuthSubject) {
    let pool = pool().await;
    reset_db(&pool).await;

    let upstream = init_bare_upstream();
    let data_dir = fresh_dir(&format!("{test_name}-library-data"));

    let config = AppConfig {
        agents: agents_config_for_tests(),
        library: LibraryConfig {
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            repo_url: Some(upstream.to_string_lossy().into_owned()),
            skill_sync_interval_secs: 1,
        },
        ..Default::default()
    };
    let app = App::init(&pool, config, String::new())
        .await
        .expect("App::init");

    let user = AuthSubject::User(UserId::new());
    let project = app
        .projects()
        .create(&user, format!("proj-{test_name}"), None)
        .await
        .expect("create project");
    let space = app
        .projects()
        .create_and_mount_space(&user, project.id, "docs", None)
        .await
        .expect("create+mount space");

    // A local write through the engine — synchronous, no fetch to
    // wait for, unlike an out-of-band push.
    app.library()
        .spaces()
        .write_file(
            &space.slug,
            "a.md",
            "hello\n".into(),
            CommitAttribution::library_default(),
        )
        .await
        .expect("write a.md");

    // Not a project admin, not a sandbox attachment — just enough to
    // pass `can_use_agent_file_tools` and mount-scoped `space:` access.
    let agent = AuthSubject::Agent(project.id, AgentId::new(), Vec::new());
    (app, user, agent)
}

struct CallerProbe;

// Pause a real compose invocation while the test changes Git HEAD or mounts.
struct Checkpoint(tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>);

#[async_trait::async_trait]
impl drua_core::toolset::TopLevelTool for Checkpoint {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> &str {
        "Wait for the integration test's external change."
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"type":"object"}));
        &SCHEMA
    }
    async fn call(
        &self,
        _: &AuthSubject,
        _: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, drua_core::toolset::ToolSetsError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.0.send(send).await.unwrap();
        receive.await.unwrap();
        Ok(rmcp::model::CallToolResult::success(Vec::new()))
    }
}

#[async_trait::async_trait]
impl drua_core::toolset::TopLevelTool for CallerProbe {
    fn name(&self) -> &str {
        "caller_probe"
    }
    fn description(&self) -> &str {
        "Return the dispatcher caller for integration testing."
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"type":"object"}));
        &SCHEMA
    }
    async fn call(
        &self,
        subject: &AuthSubject,
        _: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, drua_core::toolset::ToolSetsError> {
        let kind = match subject {
            AuthSubject::Agent(..) => "agent",
            AuthSubject::ExportedAgent(..) => "exported_agent",
            _ => "unexpected",
        };
        let mut result = rmcp::model::CallToolResult::success(Vec::new());
        result.structured_content = Some(serde_json::json!({"type":kind}));
        Ok(result)
    }
}

#[tokio::test]
#[ignore = "requires isolated postgres + local library clone"]
async fn scripts_use_ordinary_reads_and_invocation_local_caches() {
    use drua_core::auth::AuthScope;
    use drua_core::primitives::McpCredsId;

    let (app, user, agent) = setup("scripts").await;
    app.toolsets().register_top_level(CallerProbe);
    let (checkpoints, mut changes) = tokio::sync::mpsc::channel(1);
    app.toolsets().register_top_level(Checkpoint(checkpoints));
    let spaces = app.library().spaces();
    spaces
        .write_file(
            "docs",
            "helper.js",
            "return {relative: (a,b) => a + b};".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    let other_project = app.projects().create(&user, "other", None).await.unwrap();
    let private = app
        .projects()
        .create_and_mount_space(&user, other_project.id, "private", None)
        .await
        .unwrap();
    spaces
        .write_file(
            &private.slug,
            "helper.js",
            "return 7;".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    spaces
        .write_file(
            "docs",
            "dependency.js",
            "return await loadScript('space:private/helper.js');".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();

    spaces
        .write_file(
            "docs",
            "caller.js",
            "return {run: async () => await tools.caller_probe({})};".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    let external =
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec![AuthScope::Admin]);
    let scoped = AuthSubject::ExportedAgent(
        UserId::new(),
        McpCredsId::new(),
        vec![AuthScope::ProjectAdmin(agent.project_id().unwrap())],
    );
    for subject in [&agent, &external] {
        let compose = app
            .toolsets()
            .top_level_tool_arcs(subject)
            .find(|t| t.name() == "compose")
            .unwrap();
        let output = compose.call(subject, serde_json::json!({"script":"const links = await loadScript('space:docs/helper.js'); return links.relative('a','b');"}).as_object().cloned()).await.unwrap();
        let output = output.structured_content.unwrap();
        assert_eq!(output["result"], "ab");
        assert!(output.get("script_execution").is_none());
        assert!(!output.to_string().contains("relative:"));
        assert!(compose.description().contains("loadScript"));
        let types = app
            .toolsets()
            .top_level_tool_arcs(subject)
            .find(|t| t.name() == "compose_types")
            .unwrap();
        let declarations = types
            .call(
                subject,
                serde_json::json!({"tool_names":["caller_probe"]})
                    .as_object()
                    .cloned(),
            )
            .await
            .unwrap();
        assert!(declarations.structured_content.unwrap()["declarations"]
            .as_str()
            .unwrap()
            .contains(js_engine::LOAD_SCRIPT_DECLARATION));
        let caller = compose.call(subject, serde_json::json!({"script":"const helper = await loadScript('space:docs/caller.js'); return await helper.run();"}).as_object().cloned()).await.unwrap().structured_content.unwrap();
        let expected = if subject.project_id().is_some() {
            "agent"
        } else {
            "exported_agent"
        };
        assert_eq!(caller["result"]["type"], expected);
    }
    for subject in [&agent, &scoped] {
        let compose = app
            .toolsets()
            .top_level_tool_arcs(subject)
            .find(|t| t.name() == "compose")
            .unwrap();
        let error = compose.call(subject, serde_json::json!({"script":"return await loadScript('space:docs/dependency.js');"}).as_object().cloned()).await.unwrap_err();
        assert!(error.to_string().contains("access_denied"), "{error}");
    }

    let compose = app
        .toolsets()
        .top_level_tool_arcs(&agent)
        .find(|t| t.name() == "compose")
        .unwrap();
    let (result, ()) = tokio::join!(
        compose.call(&agent, serde_json::json!({"script": "const first = await loadScript('space:docs/helper.js'); await tools.checkpoint({}); const again = await loadScript('space:docs/helper.js'); const fresh = await loadScript('space:docs/fresh.js'); return {same: first === again, old: again.relative('a','b'), fresh};"}).as_object().cloned()),
        async {
            let resume = changes.recv().await.unwrap();
            for path in ["helper.js", "fresh.js"] {
                spaces.write_file("docs", path, "return {version: 2};".into(), CommitAttribution::library_default()).await.unwrap();
            }
            resume.send(()).unwrap();
        }
    );
    assert_eq!(
        result.unwrap().structured_content.unwrap()["result"],
        serde_json::json!({"same":true,"old":"ab","fresh":{"version":2}})
    );
    let next = compose
        .call(
            &agent,
            serde_json::json!({"script":"return await loadScript('space:docs/helper.js');"})
                .as_object()
                .cloned(),
        )
        .await
        .unwrap();
    assert_eq!(
        next.structured_content.unwrap()["result"],
        serde_json::json!({"version":2})
    );
    spaces
        .write_file(
            "docs",
            "dir.js/child",
            "child".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    for (path, expected) in [("missing.js", "missing_file"), ("dir.js", "directory")] {
        let script = format!("return await loadScript('space:docs/{path}');");
        let error = compose
            .call(
                &agent,
                serde_json::json!({"script":script}).as_object().cloned(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    spaces
        .write_file(
            "docs",
            "failure.js",
            "throw new Error('attributed failure');".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    let failing_subject = AuthSubject::User(UserId::new());
    let error = app
        .toolsets()
        .call_top_level_tool(
            &failing_subject,
            "compose",
            serde_json::json!({"script":"return await loadScript('space:docs/failure.js');"})
                .as_object()
                .cloned(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("space:docs/failure.js:1"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let rows = app
            .audit()
            .find(&drua_core::audit::AuditLogQuery {
                acting_user_id: failing_subject.user_id().ok(),
                limit: 20,
                ..Default::default()
            })
            .await
            .unwrap();
        if let Some(parent) = rows.iter().find(|r| r.action == "compose") {
            assert_eq!(parent.outcome, "error");
            let source = &parent.metadata["script_execution"]["loads"]["sources"][0];
            assert_eq!(source["path"], "space:docs/failure.js");
            assert_eq!(source["initialized"], false);
            assert!(source["error"]
                .as_str()
                .unwrap()
                .contains("attributed failure"));
            assert!(source.get("text").is_none());
            use sha2::{Digest, Sha256};
            assert_eq!(
                source["sha256"],
                format!(
                    "{:x}",
                    Sha256::digest(b"throw new Error('attributed failure');")
                )
            );
            assert_eq!(source["byte_length"], 38);
            assert!(source.get("revision").is_none());
            assert!(source.get("blob_oid").is_none());
            assert!(parent.metadata.get("arguments").is_some());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "missing parent audit");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let docs = spaces.maybe_find_by_slug("docs").await.unwrap().unwrap();
    let (result, ()) = tokio::join!(
        compose.call(&agent, serde_json::json!({"script": "const helper = await loadScript('space:docs/helper.js'); await tools.checkpoint({}); const same = helper === await loadScript('space:docs/helper.js'); let denied; try {await loadScript('space:docs/caller.js')} catch(e) {denied = String(e)} return {same,denied};"}).as_object().cloned()),
        async {
            let resume = changes.recv().await.unwrap();
            app.projects().unmount_space(&user, agent.project_id().unwrap(), docs.id).await.unwrap();
            resume.send(()).unwrap();
        }
    );
    let result = result.unwrap().structured_content.unwrap();
    assert_eq!(result["result"]["same"], true);
    assert!(result["result"]["denied"]
        .as_str()
        .unwrap()
        .contains("access_denied"));
    assert!(compose
        .call(
            &agent,
            serde_json::json!({"script":"return await loadScript('space:docs/helper.js');"})
                .as_object()
                .cloned()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("access_denied"));
}

#[tokio::test]
#[ignore = "requires isolated postgres + local library clone"]
async fn workflow_scripts_validate_execute_and_preserve_provenance() {
    use drua_core::agent::Agents;
    use drua_core::audit::{Audit, AuditLogQuery};
    use drua_core::primitives::{ContextGeneration, WorkflowDefinitionId, WorkflowRunId};
    use drua_core::toolset::{
        ComposeConfig, SubmitOutputTool, TextEditor, ToolSets, ToolSetsConfig,
    };
    use drua_core::workflow::executor::Executor;
    use drua_core::workflow::repo::WorkflowDefinitionRepo;
    use drua_core::workflow::run::NewWorkflowRun;
    use drua_core::workflow::{
        WorkflowRunRepo, WorkflowRunState, WorkflowStepDef, WorkflowTrigger,
    };
    use es_entity::context::WithEventContext;
    use serde_json::json;
    use std::sync::{atomic::AtomicBool, Arc};
    use std::time::Duration;

    let (app, user, agent) = setup("workflow-scripts").await;
    let pool = pool().await;
    let project = agent.project_id().unwrap();
    let script_subject =
        AuthSubject::workflow_script(project, WorkflowDefinitionId::new(), WorkflowRunId::new());
    let visible: Vec<_> = app
        .toolsets()
        .top_level_tool_arcs(&script_subject)
        .map(|t| t.name().to_owned())
        .collect();
    for name in ["Read", "LS", "Glob", "Grep", "Edit", "Move", "Delete"] {
        assert!(
            visible.iter().any(|n| n == name),
            "missing {name}: {visible:?}"
        );
    }
    for name in [
        "Bash",
        "submit_output",
        "spaces",
        "skill",
        "sandbox",
        "agent",
    ] {
        assert!(!visible.iter().any(|n| n == name), "unexpected {name}");
    }
    assert!(!app
        .toolsets()
        .top_level_tool_arcs(&script_subject)
        .find(|t| t.name() == "compose")
        .unwrap()
        .composable());
    let admin_subject =
        AuthSubject::workflow_executor(project, WorkflowDefinitionId::new(), WorkflowRunId::new());
    assert!(admin_subject.is_project_admin());
    assert!(!admin_subject.can_use_agent_file_tools());

    let base = json!({"type":"script_step","name":"inventory","script":"space:docs/tasks.js"});
    for patch in [
        json!({"script":"tasks.js"}),
        json!({"script":"space:docs/tasks.md"}),
        json!({"script":"space:docs/../tasks.js"}),
        json!({"entry":"bad-entry"}),
        json!({"entry":"1run"}),
        json!({"name":"bad-name"}),
        json!({"args":"${{ steps.later.outputs }}"}),
        json!({"args":"${{ unknown.value }}"}),
        json!({"max_tool_calls":2001}),
    ] {
        let mut value = base.clone();
        value
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let result = app
            .workflows()
            .create(
                &user,
                project,
                "proj-workflow-scripts",
                "invalid".into(),
                None,
                WorkflowTrigger::Manual { condition: None },
                vec![serde_json::from_value(value).unwrap()],
                vec![],
                None,
            )
            .await;
        assert!(result.is_err(), "accepted invalid step {patch}");
    }
    let valid = app
        .workflows()
        .create(
            &user,
            project,
            "proj-workflow-scripts",
            "valid".into(),
            None,
            WorkflowTrigger::Manual { condition: None },
            vec![serde_json::from_value(base.clone()).unwrap()],
            vec![],
            None,
        )
        .await
        .unwrap();
    let invalid_update = serde_json::from_value(json!({"type":"script_step","name":"inventory","script":"space:docs/tasks.js","max_tool_calls":2001})).unwrap();
    // Create-time validation does not read the script: tasks.js does not exist yet.
    assert_eq!(valid.steps[0].name(), "inventory");
    assert!(app
        .workflows()
        .update(
            &user,
            valid.id,
            None,
            None,
            None,
            Some(vec![invalid_update]),
            None,
            None
        )
        .await
        .is_err());

    let users = Arc::new(drua_core::user::Users::new(&pool));
    let fs = Arc::new(drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        users,
    ));
    let audit = Arc::new(Audit::new(&pool));
    let toolsets = Arc::new(
        ToolSets::init(ToolSetsConfig::default(), Some(audit.clone()), None, None)
            .await
            .unwrap(),
    );
    let sandboxes = Arc::new(app.sandboxes().clone());
    let skills = Arc::new(app.skills().clone());
    toolsets.register_compose(ComposeConfig::default(), fs.clone());
    toolsets.register_top_level(TextEditor::new(sandboxes.clone(), fs.clone()));
    toolsets.register_top_level(drua_core::toolset::Read::new(sandboxes.clone(), fs));
    let (prompt_tx, mut prompts) = tokio::sync::mpsc::channel(8);
    let agents = Arc::new(Agents::new(
        &pool,
        agents_config_for_tests(),
        toolsets.clone(),
        prompt_tx,
        sandboxes.clone(),
        skills.clone(),
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    ));
    toolsets.register_top_level(SubmitOutputTool::new(agents.clone()));
    let definitions = WorkflowDefinitionRepo::new_without_library(&pool);
    let runs = WorkflowRunRepo::new(&pool);
    let executor = Executor::new(
        runs.clone(),
        definitions.clone(),
        agents,
        skills.clone(),
        sandboxes,
        toolsets,
    );

    let source = r#"
return {
  run: async (args, run) => {
    const path = `space:docs/runs/${run.id}/inventory.json`;
    await tools.Edit({command: 'create', path, file_text: JSON.stringify({args, run})});
    await tools.Read({path});
    return {success: true, output: path, args, run};
  },
  failed: () => ({success: false, output: 'declined', reason: 'test'}),
  missing: () => ({success: true}),
  throwing: () => { throw new Error('intentional failure'); },
  many: async () => { for(let i=0;i<100;i++) await tools.Read({path:'space:docs/a.md'}); return {success:true,output:'done'}; },
  slow: async () => { await new Promise(resolve => setTimeout(resolve, 2000)); return {success:true,output:'late'}; },
  unmounted: async () => await loadScript('space:unmounted/private.js'),
  echo: args => ({success:true,output:'echo',args})
};
"#;
    app.library()
        .spaces()
        .write_file(
            "docs",
            "tasks.js",
            source.into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();
    let other = app
        .projects()
        .create(&user, "other-script-project", None)
        .await
        .unwrap();
    app.projects()
        .create_and_mount_space(&user, other.id, "unmounted", None)
        .await
        .unwrap();
    app.library()
        .spaces()
        .write_file(
            "unmounted",
            "private.js",
            "return {};".into(),
            CommitAttribution::library_default(),
        )
        .await
        .unwrap();

    async fn seed(
        definitions: &WorkflowDefinitionRepo,
        runs: &WorkflowRunRepo,
        project: drua_core::primitives::ProjectId,
        steps: Vec<WorkflowStepDef>,
    ) -> WorkflowRunId {
        let new = drua_core::workflow::NewWorkflowDefinition::builder()
            .project_id(project)
            .name(format!("script-{}", uuid::Uuid::new_v4()))
            .trigger(WorkflowTrigger::Manual { condition: None })
            .steps(steps.clone())
            .build()
            .unwrap();
        let mut op = definitions.begin_op().await.unwrap();
        let definition = definitions.create_in_op(&mut op, new).await.unwrap();
        op.commit().await.unwrap();
        runs.create(
            NewWorkflowRun::builder()
                .definition_id(definition.id)
                .project_id(project)
                .steps_snapshot(steps)
                .trigger_context(json!({"count":3}))
                .build()
                .unwrap(),
        )
        .await
        .unwrap()
        .id
    }
    let step = |entry: &str, extra: serde_json::Value| -> WorkflowStepDef {
        let mut value = base.clone();
        value["entry"] = json!(entry);
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    };
    for (entry, extra, expected) in [
        ("absent", json!({}), "not an exported function"),
        ("throwing", json!({}), "space:docs/tasks.js:"),
        ("missing", json!({}), "missing required field `output`"),
        ("many", json!({"max_tool_calls":2}), "tool call"),
        (
            "slow",
            json!({"timeout_seconds":1}),
            "side effects may have occurred",
        ),
        ("unmounted", json!({}), "access_denied"),
    ] {
        let run_id = seed(&definitions, &runs, project, vec![step(entry, extra)]).await;
        executor
            .run(run_id, Arc::new(AtomicBool::new(false)))
            .with_event_context(serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap();
        let run = runs.find_by_id(run_id).await.unwrap();
        assert_eq!(run.state, WorkflowRunState::Errored, "{entry}");
        let error = run.step_results[0].error.as_ref().unwrap();
        assert!(error.to_lowercase().contains(expected), "{entry}: {error}");
        if entry == "many" || entry == "slow" {
            assert!(error.contains("side effects may have occurred"));
        }
        assert!(prompts.try_recv().is_err(), "script requested a model turn");
    }
    let run_id = seed(
        &definitions,
        &runs,
        project,
        vec![
            step("failed", json!({})),
            step(
                "echo",
                json!({"name":"skipped","condition":"steps.inventory.outputs.success"}),
            ),
        ],
    )
    .await;
    executor
        .run(run_id, Arc::new(AtomicBool::new(false)))
        .with_event_context(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    let run = runs.find_by_id(run_id).await.unwrap();
    assert_eq!(run.state, WorkflowRunState::Failed);
    assert_eq!(
        run.step_results[0].output.as_ref().unwrap()["success"],
        false
    );
    assert!(run.step_results[0].error.is_none());
    assert!(run.step_results[1].skipped.is_some());

    // A script exceeding the MCP compose default of 50 calls uses its own limits.
    let run_id = seed(&definitions, &runs, project, vec![step("many", json!({"max_tool_calls":110})), step("echo", json!({"name":"echo","args":{"plan":"${{ steps.inventory.outputs }}","step":"${{ run.step }}"}}))]).await;
    executor
        .run(run_id, Arc::new(AtomicBool::new(false)))
        .with_event_context(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    let run = runs.find_by_id(run_id).await.unwrap();
    assert_eq!(
        run.state,
        WorkflowRunState::Succeeded,
        "{:?}",
        run.step_results
    );
    assert_eq!(
        run.step_results[1].output.as_ref().unwrap()["args"]["plan"],
        json!({"success":true,"output":"done"})
    );
    assert_eq!(
        run.step_results[1].output.as_ref().unwrap()["args"]["step"],
        "echo"
    );
    assert!(prompts.try_recv().is_err());

    skills
        .create(
            &user,
            project,
            "proj-workflow-scripts",
            "consume-inventory".into(),
            "Read inventory".into(),
            "Read this output: ${{ steps.inventory.outputs }}".into(),
        )
        .await
        .unwrap();
    let agent_step: WorkflowStepDef = serde_json::from_value(
        json!({"type":"agent_step","name":"judge","skill":"consume-inventory"}),
    )
    .unwrap();
    let run_id = seed(
        &definitions,
        &runs,
        project,
        vec![
            step(
                "run",
                json!({"args":{"count":"${{ trigger.payload.count }}","step":"${{ run.step }}"}}),
            ),
            agent_step,
        ],
    )
    .await;
    let execute = executor
        .run(run_id, Arc::new(AtomicBool::new(false)))
        .with_event_context(serde_json::from_value(json!({})).unwrap());
    let respond = async {
        let request = tokio::time::timeout(Duration::from_secs(20), prompts.recv())
            .await
            .unwrap()
            .unwrap();
        let text = format!("{:?}", request.prompt);
        assert!(text.contains("inventory.json"), "{text}");
        let file = app
            .library()
            .read_blob_at_head(&format!("spaces/docs/runs/{run_id}/inventory.json"))
            .await
            .unwrap()
            .unwrap();
        let written: serde_json::Value = serde_json::from_slice(&file).unwrap();
        assert_eq!(written["args"]["count"], 3);
        assert_eq!(written["args"]["step"], "inventory");
        assert_eq!(written["run"]["step"], "inventory");
        request
            .response_channel
            .send(Ok(llm::PromptResult::Complete(llm::PromptResponse {
                content: vec![llm::prompt::AssistantBlock::ToolUse {
                    id: "submit".into(),
                    name: "submit_output".into(),
                    input: json!({"success":true,"output":"reviewed"}),
                }],
                usage: Default::default(),
                stop_reason: Some(llm::response::StopReason::ToolUse),
                model_used: None,
            })))
            .unwrap();
    };
    let (result, ()) = tokio::join!(execute, respond);
    result.unwrap();
    assert!(prompts.try_recv().is_err(), "unexpected extra model turn");
    let run = runs.find_by_id(run_id).await.unwrap();
    assert_eq!(
        run.state,
        WorkflowRunState::Succeeded,
        "{:?}",
        run.step_results
    );
    assert!(run.step_results[0]
        .output
        .as_ref()
        .unwrap()
        .get("tool_calls")
        .is_none());
    let agent_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agents WHERE workflow_run_id=$1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(agent_count, 1, "only the agent step creates an agent");
    let query = AuditLogQuery {
        workflow_run_id: Some(run_id),
        workflow_step: Some("inventory".into()),
        limit: 100,
        ..Default::default()
    };
    let entries = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = audit.find(&query).await.unwrap();
            if entries.len() >= 3 {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(entries.iter().any(|e| e.action == "compose"));
    assert!(entries
        .iter()
        .any(|e| e.entrypoint.as_deref() == Some("compose > mcp: Edit")));
    assert!(entries
        .iter()
        .any(|e| e.entrypoint.as_deref() == Some("compose > mcp: Read")));
    for entry in &entries {
        assert!(entry.acting_agent_id.is_none());
        assert!(entry.acting_user_id.is_none());
    }
    let log = app
        .toolsets()
        .call_top_level_tool(
            &admin_subject,
            "log",
            json!({"workflow_run_id":run_id,"workflow_step":"inventory"})
                .as_object()
                .cloned(),
        )
        .await
        .unwrap();
    assert!(!log.is_error.unwrap_or(false));
    assert!(serde_json::to_string(&log)
        .unwrap()
        .contains(&run_id.to_string()));
    let git_log = Command::new("git")
        .args([
            "log",
            "--format=%B",
            "--",
            &format!("spaces/docs/runs/{run_id}/inventory.json"),
        ])
        .current_dir(app.library().repo_path())
        .output()
        .unwrap();
    let trailers = String::from_utf8(git_log.stdout).unwrap();
    for trailer in [
        format!("Drua-Workflow-Run: {run_id}"),
        "Drua-Workflow-Step: inventory".into(),
        "Drua-Subject-Type: workflow_agent".into(),
    ] {
        assert!(trailers.contains(&trailer), "{trailers}");
    }
    assert!(!trailers.contains("Drua-Acting-Agent"));
    assert!(!trailers.contains("Co-Authored-By"));
    app.shutdown().await;
}
