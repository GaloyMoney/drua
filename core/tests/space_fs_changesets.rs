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
use drua_core::auth::AuthScope;
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AuthSubject, McpCredsId, UserId};
use drua_core::project::ProjectError;
use drua_core::toolset::SearchableToolSet;
use drua_core::{App, AppConfig};
use drua_library::{CommitAttribution, SpaceError};
use rmcp::model::CallToolResult;

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
    // `create` already persisted a real `Agent` row for the lead —
    // reuse its id, but build the subject with a plain `ProjectMember`
    // scope rather than `ProjectAdmin`, so these tests exercise the
    // same path a real chat/task agent takes: read-only on spaces
    // (rev6 D44) — no writes at all, admin or a workflow run only.
    AuthSubject::Agent(
        project.id,
        project.lead_agent_id,
        vec![drua_core::auth::AuthScope::ProjectMember(project.id)],
    )
}

/// Same as `project_with_space`, but also returns the real lead
/// subject (`ProjectAdmin` — `Update` on spaces) alongside a
/// `ProjectMember`-scoped one, for tests exercising rev2 §2's
/// authority-resolved context table on both sides at once.
async fn project_with_space_and_lead(
    app: &App,
    user: &AuthSubject,
    project_name: &str,
    slug: &str,
) -> (AuthSubject, AuthSubject) {
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
    let member = AuthSubject::Agent(
        project.id,
        project.lead_agent_id,
        vec![drua_core::auth::AuthScope::ProjectMember(project.id)],
    );
    let lead = AuthSubject::Agent(
        project.id,
        project.lead_agent_id,
        vec![drua_core::auth::AuthScope::ProjectAdmin(project.id)],
    );
    (member, lead)
}

fn space_fs(app: &App) -> drua_core::space_fs::SpaceFs {
    drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    )
}

/// rev6: a workflow run is the one interactive-shaped actor (besides
/// an admin) that can still open a draft (D44/D45). `Changeset.
/// opened_by`/`Changesets::actor_key_for` FK into `workflow_runs`, so
/// a synthetic `WorkflowRunId` alone isn't enough — this creates a
/// minimal definition + run in `project_id` and returns the
/// `WorkflowExecutor` subject for it. Shared by
/// `workflow_run_subject_overlays_its_run_draft` and
/// `explicit_read_of_foreign_project_changeset_is_rejected`.
async fn run_subject_for(
    pool: &sqlx::PgPool,
    project_id: drua_core::primitives::ProjectId,
) -> AuthSubject {
    let definitions = drua_core::workflow::repo::WorkflowDefinitionRepo::new_without_library(pool);
    let runs = drua_core::workflow::WorkflowRunRepo::new(pool);
    let new_def = drua_core::workflow::NewWorkflowDefinition::builder()
        .project_id(project_id)
        .name(format!("run-{}", uuid::Uuid::new_v4()))
        .trigger(drua_core::workflow::WorkflowTrigger::Manual { condition: None })
        .steps(Vec::new())
        .build()
        .expect("build definition");
    let mut op = definitions.begin_op().await.expect("begin op");
    let definition = definitions
        .create_in_op(&mut op, new_def)
        .await
        .expect("create definition");
    op.commit().await.expect("commit");
    let run = runs
        .create(
            drua_core::workflow::run::NewWorkflowRun::builder()
                .definition_id(definition.id)
                .project_id(project_id)
                .steps_snapshot(Vec::new())
                .trigger_context(serde_json::json!({}))
                .build()
                .expect("build run"),
        )
        .await
        .expect("create run");
    AuthSubject::workflow_executor(project_id, definition.id, run.id)
}

/// rev6 §6.2: the two remaining tool-level e2e tests exercise
/// `drua_admin_spaces` directly — it's pinned, unchanged, and it's the
/// only place a draft/PR flow is still reachable through a top-level
/// tool call. `SearchableToolSet::call` is public, so a test can
/// construct the toolset from `App`'s own accessors and call it
/// without the full MCP gateway (`search_tools`/`describe_tool`/
/// `call_tool`) in the loop.
fn admin_tool_set(app: &App) -> drua_core::toolset::AdminToolSet {
    drua_core::toolset::AdminToolSet::new(
        Arc::new(app.agents().clone()),
        Arc::new(app.sandboxes().clone()),
        Arc::new(app.audit().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.spaces().clone()),
        Arc::new(space_fs(app)),
        Arc::new(app.search().clone()),
        Arc::new(app.workflows().clone()),
        Arc::new(app.skills().clone()),
        Arc::new(app.notes().clone()),
        Arc::new(app.changesets().clone()),
    )
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn bound_write_lands_on_changeset_branch_and_leaves_main_untouched() {
    let (app, user) = setup("bound_write").await;
    // rev6 D44/D45: drafting is admin-or-run only — `user` (`AuthSubject::
    // User`, admin via `has_scope`) stands in for the interactive actor
    // that used to be a `ProjectMember`. The project/space fixture is
    // still needed regardless of who drafts against it.
    project_with_space(&app, &user, "proj-bound-write", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&user, Some("add a paragraph".into()), None, None)
        .await
        .expect("open changeset");

    let fs = drua_core::space_fs::SpaceFs::new(
        Arc::new(app.library().spaces().clone()),
        Arc::new(app.projects().clone()),
        Arc::new(app.users().clone()),
        Arc::new(app.changesets().clone()),
    );

    fs.write_file(&user, "draft:docs/a.md", "staged content\n".into())
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
    let status = app.changesets().status(&user, cs.id).await.expect("status");
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
    let owner_agent = project_with_space(&app, &user, "proj-owner", "docs").await;
    let owner_project_id = owner_agent.project_id().expect("owner has a project");
    let outsider = project_with_space(&app, &user, "proj-outsider", "notes").await;
    // The mount gate (`Projects::space_for_subject`) runs before the
    // changeset-ownership check — mount "docs" on the outsider's
    // project too, so this test exercises `ChangesetForeign` and not
    // just `NotMounted`.
    app.projects()
        .mount_space(
            &user,
            outsider.project_id().expect("outsider has a project"),
            "docs",
        )
        .await
        .expect("mount docs onto the outsider's project");

    // rev6 D44/D45: the owner is a workflow run — the only interactive-
    // shaped actor besides an admin that can still open a draft.
    // Reading it stays allowed for the outsider (a plain `ProjectMember`,
    // unaffected by D44); only the cross-project ownership check matters
    // here.
    let pool = pool().await;
    let owner = run_subject_for(&pool, owner_project_id).await;

    let cs = app
        .changesets()
        .draft_for(&owner, Some("owner's work".into()), None, None)
        .await
        .expect("open changeset");

    let fs = space_fs(&app);

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
    project_with_space(&app, &user, "proj-move", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&user, Some("moving things".into()), None, None)
        .await
        .expect("open changeset");

    let fs = space_fs(&app);

    let from = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .move_file(&user, &from, "space:docs/b.md")
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
    project_with_space(&app, &user, "proj-discard", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&user, Some("will be discarded".into()), None, None)
        .await
        .expect("open changeset");
    app.changesets()
        .discard(&user, cs.id, Some("no longer needed".into()))
        .await
        .expect("discard");

    let fs = space_fs(&app);

    let path = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .write_file(&user, &path, "too late\n".into())
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

/// rev4 D25/D26, rev5 D37: a workflow-run subject's `space:` writes
/// are refused (`RunReadOnly`, never lazily staged) until a run draft
/// is open, and once one is, `space:`/`draft:` both overlay it — reads
/// and writes hit the same branch, `main` stays untouched, and the
/// stamp says "run draft". An explicit `@<id>` write is `BadRequest`
/// (a run writes only its own draft); an explicit `@<id>` **read**
/// stays allowed (OQ-29).
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn workflow_run_subject_overlays_its_run_draft() {
    let (app, user) = setup("run_overlay").await;
    let agent = project_with_space(&app, &user, "proj-run-overlay", "docs").await;
    let project_id = agent.project_id().expect("agent has a project");

    let pool = pool().await;
    let run_sub = run_subject_for(&pool, project_id).await;

    let fs = space_fs(&app);

    // No run draft open yet: refused, not lazily created.
    let err = fs
        .write_file(&run_sub, "space:docs/a.md", "nope\n".into())
        .await
        .expect_err("a run subject must not lazily create its draft");
    assert!(
        matches!(err, ProjectError::Space(SpaceError::RunReadOnly { .. })),
        "expected RunReadOnly, got: {err}"
    );

    // Stand-in for the executor's pre-flight (lands in a later rev5
    // commit): open the run's draft by hand.
    let cs = app
        .changesets()
        .draft_for(&run_sub, Some("wf run".into()), None, None)
        .await
        .expect("open run draft");

    // `space:` now overlays the run draft; the stamp names it a "run
    // draft" and preserves the typed `space:` prefix.
    let stamp = fs
        .write_file(&run_sub, "space:docs/a.md", "staged by run\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    assert!(stamp.contains("space:docs · run draft"), "got: {stamp}");

    // `main` is untouched.
    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n");

    // `space:` reads the overlay, not `main`.
    let read = fs
        .view_file(&run_sub, "space:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    match read {
        drua_core::space_fs::FileView::File(content) => {
            assert_eq!(content, "staged by run\n");
        }
        drua_core::space_fs::FileView::Dir(_) => panic!("expected a file"),
    }

    // `draft:` is an accepted alias for the same overlay.
    let read = fs
        .view_file(&run_sub, "draft:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    match read {
        drua_core::space_fs::FileView::File(content) => {
            assert_eq!(content, "staged by run\n");
        }
        drua_core::space_fs::FileView::Dir(_) => panic!("expected a file"),
    }

    // An explicit `@<id>` write from a run subject is `BadRequest` —
    // a run writes only its own draft.
    let idpath = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .write_file(&run_sub, &idpath, "explicit\n".into())
        .await
        .expect_err("explicit @id write must be rejected for a run subject");
    assert!(
        matches!(err, ProjectError::Space(SpaceError::BadRequest { .. })),
        "expected BadRequest, got: {err}"
    );

    // The same explicit `@<id>` form stays readable (OQ-29).
    fs.view_file(&run_sub, &idpath, None)
        .await
        .expect("view_file dispatch")
        .expect("explicit @id reads stay allowed for a run subject");
}

/// bugbot 2026-09-25 (Medium): `submit` already rejected a
/// zero-commit changeset as `Empty`, but `apply` didn't — a lead
/// `spaces publish` (or run-end `allow_land`) on a draft nobody ever
/// wrote to would land a no-op merge commit on `main`.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn apply_on_an_empty_draft_is_rejected() {
    let (app, user) = setup("empty_apply").await;
    project_with_space(&app, &user, "proj-empty-apply", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&user, Some("never touched".into()), None, None)
        .await
        .expect("open changeset");
    assert_eq!(cs.commit_count(), 0);

    let err = match app.changesets().apply(&user, cs.id, None, None).await {
        Ok(_) => panic!("an empty draft must not be landable"),
        Err(e) => e,
    };
    assert!(
        matches!(err, drua_core::changeset::ChangesetError::Empty { id } if id == cs.id),
        "expected Empty, got: {err}"
    );
}

/// rev6 D44/D45: interactive agents (lead, member) are read-only on
/// spaces — every `space:`/`draft:` write is refused with `ReadOnly`,
/// no draft is ever created, and `main` never gets touched by them.
/// `Changesets::{draft_for, apply}` fail the same way at the service
/// layer for a subject that isn't an admin or a run. Replaces rev3's
/// `member_lazy_draft_write_leaves_main_untouched_until_a_lead_lands_it`,
/// `draft_scheme_stages_even_for_a_lead`, and
/// `member_space_write_is_refused_with_use_draft`.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn interactive_agents_are_read_only() {
    let (app, user) = setup("interactive_read_only").await;
    let (member, lead) = project_with_space_and_lead(&app, &user, "proj-read-only", "docs").await;
    let fs = space_fs(&app);

    for (label, sub) in [("member", &member), ("lead", &lead)] {
        for path in ["space:docs/a.md", "draft:docs/a.md"] {
            let err = fs
                .write_file(sub, path, "nope\n".into())
                .await
                .expect_err(&format!("{label}'s write to {path} must be refused"));
            assert!(
                matches!(err, ProjectError::Space(SpaceError::ReadOnly { ref slug }) if slug == "docs"),
                "{label} {path}: expected ReadOnly, got: {err}"
            );
        }
        assert!(
            app.changesets()
                .open_draft_for(sub)
                .await
                .expect("open_draft_for")
                .is_none(),
            "{label} must never end up with an open draft"
        );
    }

    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(
        main, b"main content\n",
        "a refused write must not touch main"
    );

    let stamp = fs
        .resolved_stamp(&member, "space:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(stamp, "[space:docs · main]");

    // The service layer fails the same way for `draft_for`/`apply`,
    // regardless of which tool got there. `Changeset` isn't `Debug`, so
    // `match` instead of `expect_err` (mirrors `apply_on_an_empty_draft_
    // is_rejected`).
    let err = match app
        .changesets()
        .draft_for(&lead, Some("lead tries anyway".into()), None, None)
        .await
    {
        Ok(_) => panic!("a lead must not be able to open a draft"),
        Err(e) => e,
    };
    assert!(
        matches!(err, drua_core::changeset::ChangesetError::Authorization(_)),
        "expected Authorization, got: {err}"
    );

    let admin_draft = app
        .changesets()
        .draft_for(&user, Some("admin draft".into()), None, None)
        .await
        .expect("admin opens a draft");
    let err = match app
        .changesets()
        .apply(&lead, admin_draft.id, None, None)
        .await
    {
        Ok(_) => panic!("a lead must not be able to land someone else's draft either"),
        Err(e) => e,
    };
    assert!(
        matches!(err, drua_core::changeset::ChangesetError::Authorization(_)),
        "expected Authorization, got: {err}"
    );
}

/// rev2 §3 rule 3: a read against `space:` with no open draft yet has
/// nothing to overlay, so it falls back to `main` rather than erroring
/// or inventing a draft.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn space_read_with_no_open_draft_falls_back_to_main() {
    let (app, user) = setup("read_no_draft").await;
    let agent = project_with_space(&app, &user, "proj-read-fallback", "docs").await;
    let fs = space_fs(&app);

    let view = fs
        .view_file(&agent, "space:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    let text = match view {
        drua_core::space_fs::FileView::File(t) => t,
        drua_core::space_fs::FileView::Dir(_) => panic!("expected a file"),
    };
    assert_eq!(text, "main content\n");

    assert!(
        app.changesets()
            .open_draft_for(&agent)
            .await
            .expect("open_draft_for")
            .is_none(),
        "a read must never lazily create a draft"
    );
}

/// D10/D19/§5.3's exact stamp formats, for the shapes reachable
/// without a GitHub App configured: `main` (no draft), the "started"
/// first-write form, the normal touched-count form, the `draft:`
/// "no draft" fallback, D19's "differs" form, and the explicit
/// `@<id>` form (exercised here still `Open`, since `status`
/// transitions need a GitHub App this test fixture doesn't configure).
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn stamp_formats_match_the_documented_forms() {
    let (app, user) = setup("stamp_formats").await;
    let member = project_with_space(&app, &user, "proj-stamp", "docs").await;
    let fs = space_fs(&app);

    // rev6 D44: a member is read-only, but a plain `space:` read still
    // sees `main` exactly as before.
    let member_stamp = fs
        .resolved_stamp(&member, "space:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(member_stamp, "[space:docs · main]");

    // Every draft-shaped stamp below is now admin-only (D44/D45) — the
    // admin `user` subject stands in for what used to be a member's
    // `draft:` write and a lead's explicit `@<id>` read.
    let no_draft_stamp = fs
        .resolved_stamp(&user, "draft:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(no_draft_stamp, "[draft:docs · no draft]");

    // The write that lazily creates the draft gets "started" feedback
    // instead of a touched-file count.
    let started_stamp = fs
        .resolved_stamp(&user, "draft:docs/a.md", true)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    fs.write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    let draft = app
        .changesets()
        .open_draft_for(&user)
        .await
        .expect("open_draft_for")
        .expect("a draft exists");
    let short_id: String = draft.id.to_string().chars().take(8).collect();
    assert_eq!(
        started_stamp,
        format!(
            "[draft:docs · draft {short_id} \"{}\" · started]",
            draft.title
        )
    );

    let draft_scheme_stamp = fs
        .resolved_stamp(&user, "draft:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(
        draft_scheme_stamp,
        format!(
            "[draft:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
    );

    // D19: a `space:` read of the same path the admin's draft has
    // touched names the draft rather than silently showing `main`.
    let differs_stamp = fs
        .resolved_stamp(&user, "space:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(
        differs_stamp,
        format!("[space:docs · main · differs in your draft {short_id}]")
    );

    let explicit_path = format!("space:docs@{}/a.md", draft.id);
    let explicit_stamp = fs
        .resolved_stamp(&user, &explicit_path, false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    // `Open` via the explicit form still renders the draft-shaped
    // stamp (only a non-`Open` status switches to the "changeset"
    // form) — same id/title/count as the `draft:`-resolved one above,
    // just under the `@<id>` scheme text.
    assert_eq!(
        explicit_stamp,
        format!(
            "[space:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
    );
}

/// A write's own returned stamp must report its own effect, not the
/// prior write's — the second write of a sequence should say
/// `2 files`, not `1 file` (the count as of the first).
/// `stamp_after_write` re-derives it from a fresh entity once the
/// write (and `record_write`'s `head_oid` update) have landed.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn write_file_returns_its_own_touched_count_not_the_prior_writes() {
    let (app, user) = setup("stamp_own_count").await;
    project_with_space(&app, &user, "proj-stamp-own-count", "docs").await;
    let fs = space_fs(&app);

    let started = fs
        .write_file(&user, "draft:docs/a.md", "one\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    assert!(started.contains("started"), "got: {started}");

    let second = fs
        .write_file(&user, "draft:docs/b.md", "two\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    assert!(second.contains("2 files"), "got: {second}");

    let third = fs
        .write_file(&user, "draft:docs/c.md", "three\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    assert!(third.contains("3 files"), "got: {third}");
}

/// rev3 D15 (admin-only as of rev6 D44): an admin who holds
/// `can_draft_spaces()` still can't write `space:` directly once a
/// draft is open — fail closed, not a silent land.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn admin_space_write_with_open_draft_is_refused_with_draft_open() {
    let (app, user) = setup("admin_draft_open").await;
    project_with_space(&app, &user, "proj-draft-open", "docs").await;
    let fs = space_fs(&app);

    fs.write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    let draft = app
        .changesets()
        .open_draft_for(&user)
        .await
        .expect("open_draft_for")
        .expect("a draft is open");

    let err = fs
        .write_file(&user, "space:docs/b.md", "direct\n".into())
        .await
        .expect_err("an admin with an open draft must not write space: directly");
    let short_id: String = draft.id.to_string().chars().take(8).collect();
    assert!(
        matches!(
            err,
            ProjectError::Space(SpaceError::DraftOpen { ref id, ref slug, .. })
                if *id == short_id && slug == "docs"
        ),
        "expected DraftOpen, got: {err}"
    );
}

/// rev2 D4: two concurrent first-writes from the same actor must not
/// create two `Open` drafts — the partial unique index on
/// `opened_by_actor` and `draft_for`'s catch-and-re-read make this
/// safe without a lock.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn concurrent_draft_for_calls_yield_one_changeset() {
    let (app, user) = setup("concurrent_draft_for").await;
    project_with_space(&app, &user, "proj-concurrent", "docs").await;

    let (a, b) = tokio::join!(
        app.changesets().draft_for(&user, None, None, Some("a.md")),
        app.changesets().draft_for(&user, None, None, Some("a.md")),
    );
    let (a, b) = (a.expect("draft_for a"), b.expect("draft_for b"));
    assert_eq!(a.id, b.id, "both calls must resolve to the same draft");

    // rev6 D48: `list` is admin-only now; the fresh-per-test DB (see
    // `reset_db`) means this is the only changeset in existence.
    let all = app
        .changesets()
        .list(
            &user,
            Some(drua_core::changeset::ChangesetStatus::Open),
            None,
        )
        .await
        .expect("list");
    assert_eq!(all.len(), 1, "exactly one Open changeset for this actor");
}

fn text_of(res: &CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// rev6 §6.2: rewritten against `drua_admin_spaces` (rev3 D17/D18/D19,
/// pinned by rev6 — leads no longer have a `spaces edit`/draft surface
/// at all) — `edit`/`view` with an explicit `target`, the verb-noun
/// draft commands, and the D10/D19 stamp wired into the rendered text
/// (`inspect.rs::dispatch_view`/`dispatch_edit`'s own `stamped` helper,
/// not just `SpaceFs::resolved_stamp`). This is the only e2e coverage
/// the pinned admin draft surface has.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn spaces_tool_target_param_and_verb_noun_commands_end_to_end() {
    let (app, user) = setup("spaces_tool_e2e").await;
    project_with_space(&app, &user, "proj-tool-e2e", "docs").await;

    let admin =
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec![AuthScope::Admin]);
    let admin_tools = admin_tool_set(&app);

    // `edit target: main` (explicit) as an admin with no open draft
    // lands directly and stamps `[space:docs · main]`.
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "edit", "slug": "docs", "edit_op": "write",
                "op_args": {"path": "a.md", "content": "admin direct\n"},
                "target": "main",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect("edit target: main");
    let text = text_of(&res);
    assert!(text.contains("[space:docs · main]"), "got: {text}");
    assert!(text.contains("Wrote space:docs/a.md"), "got: {text}");

    // `start-draft` is explicit and idempotent.
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "start-draft", "title": "admin's draft"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("start-draft");
    assert!(text_of(&res).contains("started"), "got: {}", text_of(&res));
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "start-draft"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("start-draft again");
    assert!(
        text_of(&res).contains("already open"),
        "got: {}",
        text_of(&res)
    );

    // `edit target: draft` stages; direct `edit target: main` is now
    // refused (`DraftOpen`) even though the admin holds write authority.
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "edit", "slug": "docs", "edit_op": "write",
                "op_args": {"path": "b.md", "content": "staged\n"},
                "target": "draft",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect("edit target: draft");
    assert!(
        text_of(&res).contains("[draft:docs"),
        "got: {}",
        text_of(&res)
    );

    let err = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "edit", "slug": "docs", "edit_op": "write",
                "op_args": {"path": "c.md", "content": "nope\n"},
                "target": "main",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect_err("an admin with an open draft must not write target: main directly");
    assert!(err.to_string().contains("DraftOpen"), "got: {err}");

    // `list-drafts` sees it; `merge-draft` lands it; `draft-status`
    // then reports no open draft.
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "list-drafts"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("list-drafts");
    assert!(
        text_of(&res).contains("admin's draft"),
        "got: {}",
        text_of(&res)
    );

    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "merge-draft"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("merge-draft");
    assert!(text_of(&res).contains("landed"), "got: {}", text_of(&res));

    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "draft-status"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("draft-status");
    assert_eq!(text_of(&res), "No open draft.");

    // The landed content is now on `main`.
    let landed = app
        .library()
        .spaces()
        .read_file("docs", "b.md", None)
        .await
        .expect("read main")
        .expect("b.md landed on main");
    assert_eq!(landed, b"staged\n");

    // rev4 D32: `merge-draft` with an explicit `title`/`body`
    // overrides the default `changeset: <title>` merge-commit message.
    admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({"command": "start-draft", "title": "second draft"})
                .as_object()
                .cloned(),
        )
        .await
        .expect("start second draft");
    admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "edit", "slug": "docs", "edit_op": "write",
                "op_args": {"path": "d.md", "content": "second\n"},
                "target": "draft",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect("edit target: draft (second)");
    let res = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "merge-draft",
                "title": "custom merge title",
                "body": "custom merge body",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect("merge-draft with title/body");
    let text = text_of(&res);
    let merge_oid = text
        .strip_prefix("Changeset ")
        .and_then(|s| s.split(" landed as ").nth(1))
        .and_then(|s| s.strip_suffix('.'))
        .expect("merge_oid in merge-draft text");
    let git_log = Command::new("git")
        .args(["log", "-1", "--format=%B", merge_oid])
        .current_dir(app.library().repo_path())
        .output()
        .expect("git log");
    let message = String::from_utf8(git_log.stdout).unwrap();
    assert!(
        message.starts_with("custom merge title\n\ncustom merge body"),
        "got: {message}"
    );
}

/// rev6 §6.2: admin variant of the deleted
/// `member_open_pr_reaches_pr_unavailable_not_forbidden` — `open-pr` is
/// reachable by any subject that can draft at all (D45); the local
/// test fixture has no GitHub App configured, so it ends in
/// `PrUnavailable`, proving `title`/`body` reached the call (a missing
/// one would fail argument parsing first). `member_merge_draft_without_
/// update_is_forbidden` is gone too — its assertion (a non-admin,
/// non-run subject can't `draft_for`/`apply`) now lives in
/// `interactive_agents_are_read_only`, at the service layer.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn admin_open_pr_reaches_pr_unavailable_not_forbidden() {
    let (app, user) = setup("admin_open_pr_pr_unavailable").await;
    project_with_space(&app, &user, "proj-open-pr", "docs").await;

    let admin =
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec![AuthScope::Admin]);
    let admin_tools = admin_tool_set(&app);

    admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "edit", "slug": "docs", "edit_op": "write",
                "op_args": {"path": "a.md", "content": "staged\n"},
                "target": "draft",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect("edit target: draft stages a lazy draft");

    let err = admin_tools
        .call(
            &admin,
            "spaces",
            serde_json::json!({
                "command": "open-pr",
                "title": "my PR title",
                "body": "my PR body",
            })
            .as_object()
            .cloned(),
        )
        .await
        .expect_err("no GitHub App is configured in this fixture");
    assert!(
        err.to_string().contains("PrUnavailable"),
        "expected PrUnavailable, got: {err}"
    );
}

/// rev6 D47: the top-level `spaces` tool no longer advertises `edit` or
/// any draft command, and it's invisible to a plain member (only a
/// lead, or external-lead creds, get `Update` on `Project`).
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn spaces_tool_has_no_draft_commands() {
    let (app, user) = setup("spaces_no_draft_commands").await;
    let (member, lead) = project_with_space_and_lead(&app, &user, "proj-no-draft", "docs").await;

    assert!(
        app.toolsets()
            .top_level_tool_arcs(&member)
            .all(|t| t.name() != "spaces"),
        "spaces must not be visible to a plain member"
    );

    let spaces = app
        .toolsets()
        .top_level_tool_arcs(&lead)
        .find(|t| t.name() == "spaces")
        .expect("spaces tool visible to a lead");

    for command in ["edit", "start-draft", "merge-draft"] {
        let err = spaces
            .call(
                &lead,
                serde_json::json!({"command": command, "slug": "docs"})
                    .as_object()
                    .cloned(),
            )
            .await
            .expect_err(&format!("{command} must not be a valid spaces command"));
        assert!(
            matches!(err, drua_core::toolset::ToolSetsError::InvalidArgument(_)),
            "{command}: expected a schema/argument-parse error, got: {err}"
        );
    }
}
