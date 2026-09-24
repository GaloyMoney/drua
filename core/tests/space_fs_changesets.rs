#![recursion_limit = "256"]
//! End-to-end coverage of `SpaceFs`'s changeset routing (handoff
//! `handoff-space-changesets-2026-09-23.md` §6) through the full `App`
//! stack: a bound write lands on the changeset branch and leaves
//! `main` untouched, an explicit `@id` read of another project's
//! changeset is `ChangesetForeign`, a `move` with an explicit id on
//! only one side is `BadRequest`, and a write against a non-`Open`
//! changeset is `ChangesetNotOpen`. Same fixture shape as
//! `space_details.rs`/`compose_scripts.rs` — requires Postgres and a
//! working git checkout, so every test is `#[ignore]`d.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use drua_core::agent::{AgentRole, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AuthSubject, UserId};
use drua_core::project::ProjectError;
use drua_core::{App, AppConfig};
use drua_library::{CommitAttribution, SpaceError};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn tests_artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("space-fs-changesets-e2e")
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
            changesets, changeset_events, \
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

async fn setup(test_name: &str) -> (App, AuthSubject) {
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
    (app, user)
}

/// Opens a project + mounted space + real lead agent, and writes an
/// initial `main` commit through the engine (so `open` has a non-unborn
/// HEAD to pin `base_oid` at). Returns the agent subject.
async fn project_with_space(
    app: &App,
    user: &AuthSubject,
    project_name: &str,
    slug: &str,
) -> AuthSubject {
    let project = app
        .projects()
        .create(user, project_name.to_string(), None)
        .await
        .expect("create project");
    let space = app
        .projects()
        .create_and_mount_space(user, project.id, slug, None)
        .await
        .expect("create+mount space");
    app.library()
        .spaces()
        .write_file(
            &space.slug,
            "a.md",
            "main content\n".into(),
            CommitAttribution::library_default(),
            None,
        )
        .await
        .expect("seed main");
    // `create` already persisted a real `Agent` row for the lead
    // (`bind_actor_entity_in_op` needs a live row to bind onto) — reuse
    // its id, but build the subject with a plain `ProjectMember` scope
    // (§4.2 OQ-2: `Propose`-only) rather than `ProjectAdmin`, so these
    // tests exercise the same path a real chat/task agent takes:
    // staging through a changeset, not writing `main` directly.
    AuthSubject::Agent(
        project.id,
        project.lead_agent_id,
        vec![drua_core::auth::AuthScope::ProjectMember(project.id)],
    )
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn bound_write_lands_on_changeset_branch_and_leaves_main_untouched() {
    let (app, user) = setup("bound_write").await;
    let agent = project_with_space(&app, &user, "proj-bound-write", "docs").await;

    let cs = app
        .changesets()
        .open(&agent, "add a paragraph".into(), None)
        .await
        .expect("open changeset");

    let fs = drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    );

    fs.write_file(&agent, "space:docs/a.md", "staged content\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    // main untouched.
    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n");

    // the changeset's own tip has the staged content.
    let status = app
        .changesets()
        .status(&agent, cs.id)
        .await
        .expect("status");
    assert_eq!(status.commits, 1);
    let staged = app
        .library()
        .spaces()
        .read_file("docs", "a.md", Some(&status.head_oid))
        .await
        .expect("read at tip")
        .expect("a.md exists on the branch");
    assert_eq!(staged, b"staged content\n");
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn explicit_read_of_foreign_project_changeset_is_rejected() {
    let (app, user) = setup("foreign_changeset").await;
    let owner = project_with_space(&app, &user, "proj-owner", "docs").await;
    let outsider = project_with_space(&app, &user, "proj-outsider", "notes").await;

    let cs = app
        .changesets()
        .open(&owner, "owner's work".into(), None)
        .await
        .expect("open changeset");

    let fs = drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    );

    let path = format!("space:docs@{}/a.md", cs.id);
    let Err(err) = fs.view_file(&outsider, &path, None).await else {
        panic!("outsider must not read owner's changeset");
    };
    assert!(
        matches!(
            err,
            ProjectError::Space(SpaceError::ChangesetForeign { .. })
        ),
        "expected ChangesetForeign, got: {err}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn move_with_changeset_annotation_on_only_one_side_is_bad_request() {
    let (app, user) = setup("move_mismatch").await;
    let agent = project_with_space(&app, &user, "proj-move", "docs").await;

    let cs = app
        .changesets()
        .open(&agent, "moving things".into(), None)
        .await
        .expect("open changeset");

    let fs = drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    );

    let from = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .move_file(&agent, &from, "space:docs/b.md")
        .await
        .expect_err("mismatched @id annotation must be rejected");
    assert!(
        matches!(err, ProjectError::Space(SpaceError::BadRequest { .. })),
        "expected BadRequest, got: {err}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn write_against_a_discarded_changeset_is_rejected() {
    let (app, user) = setup("discarded_write").await;
    let agent = project_with_space(&app, &user, "proj-discard", "docs").await;

    let cs = app
        .changesets()
        .open(&agent, "will be discarded".into(), None)
        .await
        .expect("open changeset");
    app.changesets()
        .discard(&agent, cs.id, Some("no longer needed".into()))
        .await
        .expect("discard");

    let fs = drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    );

    let path = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .write_file(&agent, &path, "too late\n".into())
        .await
        .expect_err("a discarded changeset must not accept writes");
    assert!(
        matches!(
            err,
            ProjectError::Space(SpaceError::ChangesetNotOpen { .. })
        ),
        "expected ChangesetNotOpen, got: {err}"
    );
}
