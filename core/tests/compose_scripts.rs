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
