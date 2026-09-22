#![recursion_limit = "256"]
//! End-to-end test of `details: true` on the top-level `LS`/`Glob`
//! tools against a real `space:` path, through the full `App` stack —
//! the same fixture shape as `skill_e2e.rs`. Asserts:
//!   1. `details: true` on a `space:` path appends
//!      `created=YYYY-MM-DD\tmodified=YYYY-MM-DD` to the matching line.
//!   2. `details: true` on a non-`space:` path is an `InvalidArgument`
//!      error, not a silent no-op — even with no sandbox attached, so
//!      the error can't be masked by "no sandbox attached" instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use drua_core::agent::{AgentRole, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AgentId, AuthSubject, UserId};
use drua_core::toolset::ToolSetsError;
use drua_core::{App, AppConfig};
use drua_library::CommitAttribution;
use rmcp::model::CallToolResult;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn tests_artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("space-details-e2e")
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

fn text_of(res: &CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn ls_details_true_shows_dates_on_space_path_and_errors_on_sandbox_path() {
    let (app, _user, agent) = setup("ls_details").await;

    let ls = app
        .toolsets()
        .top_level_tool_arcs(&agent)
        .find(|t| t.name() == "LS")
        .expect("LS visible to a non-admin agent");

    let args = serde_json::json!({"path": "space:docs", "details": true})
        .as_object()
        .unwrap()
        .clone();
    let res = ls.call(&agent, Some(args)).await.expect("LS call");
    let text = text_of(&res);
    let today = chrono::Utc::now().date_naive();
    assert!(
        text.contains(&format!("a.md\tcreated={today}\tmodified={today}")),
        "expected dated a.md line, got: {text:?}"
    );

    // No sandbox attached at all — proves the InvalidArgument fires
    // before the sandbox lookup, not as a fallback when one is missing.
    let sandbox_args = serde_json::json!({"path": "/workspace", "details": true})
        .as_object()
        .unwrap()
        .clone();
    let err = ls
        .call(&agent, Some(sandbox_args))
        .await
        .expect_err("details on a sandbox path must error, not silently ignore");
    assert!(
        matches!(&err, ToolSetsError::InvalidArgument(msg) if msg.contains("details is only supported for space: paths")),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn glob_details_true_shows_dates_on_space_path_and_errors_on_sandbox_path() {
    let (app, _user, agent) = setup("glob_details").await;

    let glob = app
        .toolsets()
        .top_level_tool_arcs(&agent)
        .find(|t| t.name() == "Glob")
        .expect("Glob visible to a non-admin agent");

    let args = serde_json::json!({
        "pattern": "**/*.md",
        "path": "space:docs",
        "details": true,
    })
    .as_object()
    .unwrap()
    .clone();
    let res = glob.call(&agent, Some(args)).await.expect("Glob call");
    let text = text_of(&res);
    let today = chrono::Utc::now().date_naive();
    assert!(
        text.contains(&format!("a.md\tcreated={today}\tmodified={today}")),
        "expected dated a.md line, got: {text:?}"
    );

    let sandbox_args = serde_json::json!({
        "pattern": "**/*.rs",
        "path": "/workspace",
        "details": true,
    })
    .as_object()
    .unwrap()
    .clone();
    let err = glob
        .call(&agent, Some(sandbox_args))
        .await
        .expect_err("details on a sandbox path must error, not silently ignore");
    assert!(
        matches!(&err, ToolSetsError::InvalidArgument(msg) if msg.contains("details is only supported for space: paths")),
        "unexpected error: {err:?}"
    );
}
