#![recursion_limit = "256"]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use drua_core::agent::{AgentRole, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::auth::AuthScope;
use drua_core::changeset::repo::ChangesetRepo;
use drua_core::changeset::{ChangesetError, ChangesetStatus};
use drua_core::github_app::PullRequest;
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AuthSubject, ChangesetId, McpCredsId, UserId};
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

/// Whether `refname` currently exists in the bare repo at `cwd` — for
/// refs a `DraftName` can't represent (an unparseable name), so
/// `Drafts::list` can't be used to check them either.
fn ref_exists(cwd: &Path, refname: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", refname])
        .current_dir(cwd)
        .status()
        .expect("spawn git")
        .success()
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
            projects, project_events, \
            ephemeral_outbox_events \
        RESTART IDENTITY CASCADE";
    sqlx::query(stmt)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("reset_db failed: {e}"));
}

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
    let (app, user, _upstream) = setup_with_upstream(test_name).await;
    (app, user)
}

/// Like `setup`, but also hands back the bare upstream's path — for
/// tests that need to manipulate origin directly (a ref library's own
/// API can't produce, for instance) rather than through the app.
async fn setup_with_upstream(test_name: &str) -> (App, AuthSubject, PathBuf) {
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
            ..Default::default()
        },
        ..Default::default()
    };
    let app = App::init(&pool, config, String::new())
        .await
        .expect("App::init");
    let user = AuthSubject::User(UserId::new());
    (app, user, upstream)
}

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
            &drua_library::SpaceTarget::Main,
        )
        .await
        .expect("seed main");
    AuthSubject::Agent(
        project.id,
        project.lead_agent_id,
        vec![drua_core::auth::AuthScope::ProjectMember(project.id)],
    )
}

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
            &drua_library::SpaceTarget::Main,
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

/// Forces a changeset straight to Submitted with a `pr_number`, bypassing
/// `Changesets::submit` (which requires a GitHub App this fixture doesn't
/// configure). Mirrors `run_subject_for`'s pattern of reaching a raw repo
/// directly in tests.
async fn force_submit(pool: &sqlx::PgPool, id: ChangesetId, pr_number: u64) {
    let repo = ChangesetRepo::new(pool);
    let mut op = repo.begin_op().await.expect("begin op");
    let mut cs = repo
        .find_by_id_in_op(&mut op, id)
        .await
        .expect("find changeset");
    let head_oid = cs.head_oid.clone();
    cs.submit(
        head_oid,
        pr_number,
        format!("https://github.com/o/r/pull/{pr_number}"),
        "t".into(),
        "b".into(),
    )
    .expect("submit transition")
    .did_execute();
    repo.update_in_op(&mut op, &mut cs)
        .await
        .expect("update changeset");
    op.commit().await.expect("commit op");
}

fn open_pr(number: u64) -> PullRequest {
    PullRequest {
        number,
        html_url: format!("https://github.com/o/r/pull/{number}"),
        state: "open".into(),
        merged: false,
        merge_commit_sha: None,
    }
}

fn closed_unmerged_pr(number: u64) -> PullRequest {
    PullRequest {
        number,
        html_url: format!("https://github.com/o/r/pull/{number}"),
        state: "closed".into(),
        merged: false,
        merge_commit_sha: None,
    }
}

fn merged_pr(number: u64, merge_commit_sha: &str) -> PullRequest {
    PullRequest {
        number,
        html_url: format!("https://github.com/o/r/pull/{number}"),
        state: "closed".into(),
        merged: true,
        merge_commit_sha: Some(merge_commit_sha.to_string()),
    }
}

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

    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", &drua_library::SpaceTarget::Main)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n");

    let status = app.changesets().status(&user, cs.id).await.expect("status");
    assert_eq!(status.commits, 1);
    let at_tip = drua_library::SpaceTarget::Draft(drua_library::DraftHandle::new(
        drua_library::DraftName::from(uuid::Uuid::from(cs.id)),
        status.head_oid.clone(),
    ));
    let staged = app
        .library()
        .spaces()
        .read_file("docs", "a.md", &at_tip)
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
    app.projects()
        .mount_space(
            &user,
            outsider.project_id().expect("outsider has a project"),
            "docs",
        )
        .await
        .expect("mount docs onto the outsider's project");

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
        matches!(err, ProjectError::Changeset(ChangesetError::Foreign { .. })),
        "expected Foreign, got: {err}"
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
            ProjectError::Changeset(ChangesetError::ChangesetNotOpen { .. })
        ),
        "expected ChangesetNotOpen, got: {err}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn workflow_run_subject_overlays_its_run_draft() {
    let (app, user) = setup("run_overlay").await;
    let agent = project_with_space(&app, &user, "proj-run-overlay", "docs").await;
    let project_id = agent.project_id().expect("agent has a project");

    let pool = pool().await;
    let run_sub = run_subject_for(&pool, project_id).await;

    let fs = space_fs(&app);

    let err = fs
        .write_file(&run_sub, "space:docs/a.md", "nope\n".into())
        .await
        .expect_err("a run subject must not lazily create its draft");
    assert!(
        matches!(
            err,
            ProjectError::Changeset(ChangesetError::RunReadOnly { .. })
        ),
        "expected RunReadOnly, got: {err}"
    );

    let cs = app
        .changesets()
        .draft_for(&run_sub, Some("wf run".into()), None, None)
        .await
        .expect("open run draft");

    let stamp = fs
        .write_file(&run_sub, "space:docs/a.md", "staged by run\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    assert!(stamp.contains("space:docs · run draft"), "got: {stamp}");

    let main = app
        .library()
        .spaces()
        .read_file("docs", "a.md", &drua_library::SpaceTarget::Main)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(main, b"main content\n");

    let (read, _stamp) = fs
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

    let (read, _stamp) = fs
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

    let idpath = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .write_file(&run_sub, &idpath, "explicit\n".into())
        .await
        .expect_err("explicit @id write must be rejected for a run subject");
    assert!(
        matches!(err, ProjectError::Space(SpaceError::BadRequest { .. })),
        "expected BadRequest, got: {err}"
    );

    fs.view_file(&run_sub, &idpath, None)
        .await
        .expect("view_file dispatch")
        .expect("explicit @id reads stay allowed for a run subject");
}

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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn reconcile_pr_state_open_pr_leaves_submitted_changeset_unchanged() {
    let (app, user) = setup("reconcile_open").await;
    project_with_space(&app, &user, "proj-reconcile-open", "docs").await;
    let cs = app
        .changesets()
        .draft_for(&user, Some("staged".into()), None, None)
        .await
        .expect("open changeset");
    space_fs(&app)
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let pool = pool().await;
    force_submit(&pool, cs.id, 101).await;

    app.changesets()
        .reconcile_pr_state(cs.id, &open_pr(101))
        .await
        .expect("reconcile");

    let refreshed = app.changesets().status(&user, cs.id).await.expect("status");
    assert_eq!(refreshed.status, ChangesetStatus::Submitted);
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn reconcile_pr_state_closed_unmerged_pr_rejects_and_deletes_ref() {
    let (app, user) = setup("reconcile_rejected").await;
    project_with_space(&app, &user, "proj-reconcile-rejected", "docs").await;
    let cs = app
        .changesets()
        .draft_for(&user, Some("staged".into()), None, None)
        .await
        .expect("open changeset");
    space_fs(&app)
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let pool = pool().await;
    force_submit(&pool, cs.id, 202).await;

    app.changesets()
        .reconcile_pr_state(cs.id, &closed_unmerged_pr(202))
        .await
        .expect("reconcile");

    let refreshed = app.changesets().status(&user, cs.id).await.expect("status");
    assert_eq!(refreshed.status, ChangesetStatus::Rejected);

    let refs_after = app.library().drafts().list().await.expect("list refs");
    assert!(
        !refs_after.contains(&cs.draft_name()),
        "ref must be deleted after rejection"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn reconcile_pr_state_merged_pr_marks_merged_and_deletes_ref() {
    let (app, user) = setup("reconcile_merged").await;
    project_with_space(&app, &user, "proj-reconcile-merged", "docs").await;
    let cs = app
        .changesets()
        .draft_for(&user, Some("staged".into()), None, None)
        .await
        .expect("open changeset");
    space_fs(&app)
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let pool = pool().await;
    force_submit(&pool, cs.id, 303).await;
    let before = app.changesets().status(&user, cs.id).await.expect("status");

    app.changesets()
        .reconcile_pr_state(cs.id, &merged_pr(303, &before.head_oid))
        .await
        .expect("reconcile");

    let refreshed = app.changesets().status(&user, cs.id).await.expect("status");
    assert_eq!(refreshed.status, ChangesetStatus::Merged);

    let refs_after = app.library().drafts().list().await.expect("list refs");
    assert!(
        !refs_after.contains(&cs.draft_name()),
        "ref must be deleted after merge"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn reconcile_pr_state_on_an_already_applied_changeset_is_a_no_op() {
    let (app, user) = setup("reconcile_applied_race").await;
    project_with_space(&app, &user, "proj-reconcile-applied", "docs").await;
    let cs = app
        .changesets()
        .draft_for(&user, Some("staged".into()), None, None)
        .await
        .expect("open changeset");
    space_fs(&app)
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    app.changesets()
        .apply(&user, cs.id, None, None)
        .await
        .expect("apply");

    // `apply` commits Applied before it calls `close_pull`, so the poll
    // can see a closed PR for a changeset it already finished, via a
    // stale list read taken before `apply` landed.
    app.changesets()
        .reconcile_pr_state(cs.id, &closed_unmerged_pr(404))
        .await
        .expect("reconcile must not error on the expected race");

    let refreshed = app.changesets().status(&user, cs.id).await.expect("status");
    assert_eq!(refreshed.status, ChangesetStatus::Applied);
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn write_against_a_rejected_changeset_is_rejected() {
    let (app, user) = setup("rejected_write").await;
    project_with_space(&app, &user, "proj-rejected-write", "docs").await;

    let cs = app
        .changesets()
        .draft_for(&user, Some("will be rejected".into()), None, None)
        .await
        .expect("open changeset");
    let fs = space_fs(&app);
    fs.write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let pool = pool().await;
    force_submit(&pool, cs.id, 505).await;
    app.changesets()
        .reconcile_pr_state(cs.id, &closed_unmerged_pr(505))
        .await
        .expect("reconcile");

    let path = format!("space:docs@{}/a.md", cs.id);
    let err = fs
        .write_file(&user, &path, "too late\n".into())
        .await
        .expect_err("a rejected changeset must not accept writes");
    let ProjectError::Changeset(ChangesetError::ChangesetNotOpen { status, .. }) = &err else {
        panic!("expected ChangesetNotOpen, got: {err}");
    };
    assert_eq!(status, "Rejected");
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn sweep_finished_refs_deletes_the_ref_of_a_rejected_changeset_whose_ref_still_exists() {
    let (app, user) = setup("sweep_rejected").await;
    project_with_space(&app, &user, "proj-sweep-rejected", "docs").await;
    let cs = app
        .changesets()
        .draft_for(&user, Some("staged".into()), None, None)
        .await
        .expect("open changeset");
    space_fs(&app)
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let pool = pool().await;
    force_submit(&pool, cs.id, 606).await;

    // Mark Rejected through the entity and repo directly, bypassing
    // `mark_rejected_in_op`'s own best-effort `delete_ref` — the ref must
    // still be there for the sweep to find.
    let repo = ChangesetRepo::new(&pool);
    let mut op = repo.begin_op().await.expect("begin op");
    let mut rejected = repo
        .find_by_id_in_op(&mut op, cs.id)
        .await
        .expect("find changeset");
    rejected
        .mark_rejected(606)
        .expect("mark rejected")
        .did_execute();
    repo.update_in_op(&mut op, &mut rejected)
        .await
        .expect("update changeset");
    op.commit().await.expect("commit op");

    let before = app.library().drafts().list().await.expect("list refs");
    assert!(
        before.contains(&cs.draft_name()),
        "ref must still exist before the sweep"
    );

    app.changesets().sweep_finished_refs().await.expect("sweep");

    let after = app.library().drafts().list().await.expect("list refs");
    assert!(
        !after.contains(&cs.draft_name()),
        "sweep must delete the leftover ref"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn sweep_finished_refs_leaves_open_and_submitted_changesets_alone() {
    let (app, user) = setup("sweep_live").await;
    let agent = project_with_space(&app, &user, "proj-sweep-live", "docs").await;
    let project_id = agent.project_id().expect("agent has a project");

    let pool = pool().await;
    // Two distinct actors, so each keeps its own open draft — `draft_for`
    // returns the caller's existing open draft rather than opening a
    // second one for the same actor.
    let run_sub = run_subject_for(&pool, project_id).await;

    let open_cs = app
        .changesets()
        .draft_for(&user, Some("still open".into()), None, None)
        .await
        .expect("open changeset");
    let submitted_cs = app
        .changesets()
        .draft_for(&run_sub, Some("still submitted".into()), None, None)
        .await
        .expect("open changeset");
    let fs = space_fs(&app);
    fs.write_file(&user, "draft:docs/a.md", "open work\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    fs.write_file(&run_sub, "space:docs/a.md", "submitted work\n".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    force_submit(&pool, submitted_cs.id, 707).await;

    app.changesets().sweep_finished_refs().await.expect("sweep");

    let refs_after = app.library().drafts().list().await.expect("list refs");
    assert!(
        refs_after.contains(&open_cs.draft_name()),
        "sweep must leave an Open changeset's ref alone"
    );
    assert!(
        refs_after.contains(&submitted_cs.draft_name()),
        "sweep must leave a Submitted changeset's ref alone"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn sweep_finished_refs_leaves_unaccounted_and_unparseable_refs_alone() {
    let (app, user, upstream) = setup_with_upstream("sweep_unaccounted").await;
    project_with_space(&app, &user, "proj-sweep-unaccounted", "docs").await;

    let main_oid = app.library().drafts().current_main().await.expect("main");

    // No changeset row exists for this id, but the name is still a real
    // uuid — `Drafts::open` can create it directly.
    let no_row_name = drua_library::DraftName::from(uuid::Uuid::nil());
    app.library()
        .drafts()
        .open(no_row_name, &main_oid)
        .await
        .expect("create ref with no changeset row");
    // A name that isn't a uuid at all can't go through `Drafts` — it has
    // to be pushed to the upstream directly.
    let unparseable_ref = "refs/heads/drua/not-a-uuid";
    git(&upstream, &["update-ref", unparseable_ref, &main_oid]);

    app.changesets().sweep_finished_refs().await.expect("sweep");

    let refs_after = app.library().drafts().list().await.expect("list refs");
    assert!(
        refs_after.contains(&no_row_name),
        "sweep must not delete a drua/ ref with no changeset row"
    );
    assert!(
        ref_exists(&upstream, unparseable_ref),
        "sweep must not delete a drua/ ref whose name doesn't parse"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn glob_and_grep_match_relative_to_the_anchored_path() {
    let (app, user) = setup("glob_anchor").await;
    let agent = project_with_space(&app, &user, "proj-glob-anchor", "docs").await;
    let project_id = agent.project_id().expect("agent has a project");

    let pool = pool().await;
    let run_sub = run_subject_for(&pool, project_id).await;
    let fs = space_fs(&app);

    app.changesets()
        .draft_for(&run_sub, Some("glob anchor".into()), None, None)
        .await
        .expect("open run draft");

    fs.write_file(&run_sub, "space:docs/runs/r1/draft/run.json", "{}".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");
    fs.write_file(&run_sub, "space:docs/runs/r1/draft/a--b.json", "{}".into())
        .await
        .expect("write_file dispatch")
        .expect("space path");

    let from_run_root = fs
        .glob(&run_sub, "space:docs/runs/r1", "draft/*--*.json")
        .await
        .expect("glob dispatch")
        .map(|(matches, _stamp)| matches);
    assert_eq!(
        from_run_root,
        Some(vec!["runs/r1/draft/a--b.json".to_string()])
    );

    let from_space_root = fs
        .glob(&run_sub, "space:docs", "runs/r1/draft/*--*.json")
        .await
        .expect("glob dispatch")
        .map(|(matches, _stamp)| matches);
    assert_eq!(
        from_space_root,
        Some(vec!["runs/r1/draft/a--b.json".to_string()])
    );

    // A non-run subject's `space:` read is never overlaid with the run's
    // draft — it sees main, which never got these files (the space root
    // itself exists on main, so this is an empty match, not a NotFound).
    let from_main = fs
        .glob(&user, "space:docs", "runs/r1/draft/*--*.json")
        .await
        .expect("glob dispatch")
        .map(|(matches, _stamp)| matches);
    assert_eq!(from_main, Some(Vec::new()));
}

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
                matches!(err, ProjectError::Changeset(ChangesetError::ReadOnly { ref slug }) if slug == "docs"),
                "{label} {path}: expected ReadOnly, got: {err}"
            );
        }
        let err = match fs.view_file(sub, "draft:docs/a.md", None).await {
            Ok(_) => panic!("{label}'s read of draft:docs/a.md must be refused"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ProjectError::Changeset(ChangesetError::ReadOnly { ref slug }) if slug == "docs"),
            "{label} draft:docs/a.md read: expected ReadOnly, got: {err}"
        );
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
        .read_file("docs", "a.md", &drua_library::SpaceTarget::Main)
        .await
        .expect("read main")
        .expect("a.md exists on main");
    assert_eq!(
        main, b"main content\n",
        "a refused write must not touch main"
    );

    let (_, stamp) = fs
        .view_file(&member, "space:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(stamp, "[space:docs · main]");

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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn space_read_with_no_open_draft_falls_back_to_main() {
    let (app, user) = setup("read_no_draft").await;
    let agent = project_with_space(&app, &user, "proj-read-fallback", "docs").await;
    let fs = space_fs(&app);

    let (view, _stamp) = fs
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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn stamp_formats_match_the_documented_forms() {
    let (app, user) = setup("stamp_formats").await;
    let member = project_with_space(&app, &user, "proj-stamp", "docs").await;
    let fs = space_fs(&app);

    let (_, member_stamp) = fs
        .view_file(&member, "space:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(member_stamp, "[space:docs · main]");

    let (_, no_draft_stamp) = fs
        .view_file(&user, "draft:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(no_draft_stamp, "[draft:docs · no draft]");

    // The draft is created by this very write, so the write's own
    // returned stamp *is* the "started" stamp — `stamp_after_write`
    // keeps the pre-write render for a draft's first commit instead of
    // refreshing it into a touched-file count.
    let started_stamp = fs
        .write_file(&user, "draft:docs/a.md", "staged\n".into())
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

    let (_, draft_scheme_stamp) = fs
        .view_file(&user, "draft:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(
        draft_scheme_stamp,
        format!(
            "[draft:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
    );

    let (_, differs_stamp) = fs
        .view_file(&user, "space:docs/a.md", None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(
        differs_stamp,
        format!("[space:docs · main · differs in your draft {short_id}]")
    );

    let explicit_path = format!("space:docs@{}/a.md", draft.id);
    let (_, explicit_stamp) = fs
        .view_file(&user, &explicit_path, None)
        .await
        .expect("view_file dispatch")
        .expect("space path");
    assert_eq!(
        explicit_stamp,
        format!(
            "[space:docs · draft {short_id} \"{}\" · 1 file]",
            draft.title
        )
    );
}

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
            ProjectError::Changeset(ChangesetError::DraftOpen { ref id, ref slug, .. })
                if *id == short_id && slug == "docs"
        ),
        "expected DraftOpen, got: {err}"
    );
}

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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn spaces_tool_target_param_and_verb_noun_commands_end_to_end() {
    let (app, user) = setup("spaces_tool_e2e").await;
    project_with_space(&app, &user, "proj-tool-e2e", "docs").await;

    let admin =
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec![AuthScope::Admin]);
    let admin_tools = admin_tool_set(&app);

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

    let landed = app
        .library()
        .spaces()
        .read_file("docs", "b.md", &drua_library::SpaceTarget::Main)
        .await
        .expect("read main")
        .expect("b.md landed on main");
    assert_eq!(landed, b"staged\n");

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
