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
    // `create` already persisted a real `Agent` row for the lead —
    // reuse its id, but build the subject with a plain `ProjectMember`
    // scope (§4.2 OQ-2: `Propose`-only) rather than `ProjectAdmin`, so
    // these tests exercise the same path a real chat/task agent takes:
    // staging through a draft, not writing `main` directly.
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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn bound_write_lands_on_changeset_branch_and_leaves_main_untouched() {
    let (app, user) = setup("bound_write").await;
    let agent = project_with_space(&app, &user, "proj-bound-write", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&agent, Some("add a paragraph".into()), None, None)
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

    let cs = app
        .changesets()
        .draft_for(&owner, Some("owner's work".into()), None, None)
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
        .draft_for(&agent, Some("moving things".into()), None, None)
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
        .draft_for(&agent, Some("will be discarded".into()), None, None)
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

/// rev2 §2/§3: a `Propose`-only member's plain `space:` write creates
/// a draft lazily (no `open`/`bind` needed) and leaves `main`
/// untouched; a lead landing it via `apply` is what finally updates
/// `main`.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn member_lazy_draft_write_leaves_main_untouched_until_a_lead_lands_it() {
    let (app, user) = setup("member_lazy_draft").await;
    let (member, lead) = project_with_space_and_lead(&app, &user, "proj-lazy", "docs").await;
    let fs = space_fs(&app);

    fs.write_file(&member, "space:docs/a.md", "member edit\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n", "main must stay untouched");

    let draft = app
        .changesets()
        .open_draft_for(&member)
        .await
        .expect("open_draft_for")
        .expect("a draft was lazily created");
    assert_eq!(draft.commit_count(), 1);

    app.changesets()
        .apply(&lead, draft.id)
        .await
        .expect("lead lands the draft");

    let landed = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(landed, b"member edit\n");
}

/// rev2 D9/D14: `draft:` always stages, even for a lead who could
/// write `main` directly via `space:` — the whole point of the scheme
/// is an edit-time choice that doesn't depend on authority.
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn draft_scheme_stages_even_for_a_lead() {
    let (app, user) = setup("draft_scheme_lead").await;
    let (_, lead) = project_with_space_and_lead(&app, &user, "proj-lead-draft", "docs").await;
    let fs = space_fs(&app);

    fs.write_file(&lead, "draft:docs/a.md", "lead's staged edit\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", None)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n", "draft: must never touch main");

    let draft = app
        .changesets()
        .open_draft_for(&lead)
        .await
        .expect("open_draft_for")
        .expect("draft: created a draft");
    assert_eq!(draft.commit_count(), 1);
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

/// D10's exact stamp formats (§5.3), for the three shapes reachable
/// without a GitHub App configured: authority `main`, an
/// authority/`draft:`-resolved draft, and the explicit `@<id>` form
/// (exercised here still `Open`, since `status` transitions need a
/// GitHub App this test fixture doesn't configure).
#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn stamp_formats_match_the_documented_forms() {
    let (app, user) = setup("stamp_formats").await;
    let (member, lead) = project_with_space_and_lead(&app, &user, "proj-stamp", "docs").await;
    let fs = space_fs(&app);

    let lead_stamp = fs
        .resolved_stamp(&lead, "space:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(lead_stamp, "[space:docs · main]");

    fs.write_file(&member, "space:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    let draft = app
        .changesets()
        .open_draft_for(&member)
        .await
        .expect("open_draft_for")
        .expect("a draft exists");
    let short_id: String = draft.id.to_string().chars().take(8).collect();

    let member_stamp = fs
        .resolved_stamp(&member, "space:docs/a.md", false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    assert_eq!(
        member_stamp,
        format!(
            "[space:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
    );

    let draft_scheme_stamp = fs
        .resolved_stamp(&member, "draft:docs/a.md", false)
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

    let explicit_path = format!("space:docs@{}/a.md", draft.id);
    let explicit_stamp = fs
        .resolved_stamp(&lead, &explicit_path, false)
        .await
        .expect("resolved_stamp")
        .expect("space path");
    // `Open` via the explicit form still renders the draft-shaped
    // stamp (only a non-`Open` status switches to the "changeset"
    // form) — same id/title/count as the authority-resolved one above,
    // just under the `@<id>` scheme text.
    assert_eq!(
        explicit_stamp,
        format!(
            "[space:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
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
    let agent = project_with_space(&app, &user, "proj-concurrent", "docs").await;

    let (a, b) = tokio::join!(
        app.changesets().draft_for(&agent, None, None, Some("a.md")),
        app.changesets().draft_for(&agent, None, None, Some("a.md")),
    );
    let (a, b) = (a.expect("draft_for a"), b.expect("draft_for b"));
    assert_eq!(a.id, b.id, "both calls must resolve to the same draft");

    let all = app
        .changesets()
        .list(&agent, Some(drua_core::changeset::ChangesetStatus::Open))
        .await
        .expect("list");
    assert_eq!(all.len(), 1, "exactly one Open changeset for this actor");
}
