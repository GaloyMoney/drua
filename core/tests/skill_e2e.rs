#![recursion_limit = "256"]
//! End-to-end test of the skill forward-sync pipeline through the
//! full `App` stack. Exercises:
//!   App::init
//!     → Projects::create        (project row + project_lead agent +
//!                                runtime/projects/<name>/.gitkeep batch)
//!     → Skills::create          (skill row)
//!         → SkillRepo post_persist_hook = sync_to_library
//!             → Library::sync_entity_in_op
//!                 → drua_library: search row in pre_commit hook +
//!                                LibraryWriteJob spawned
//!                     → upstream commit + push
//!
//! Asserts:
//!   1. `library.search(...)` returns the new skill immediately after
//!      `Skills::create` returns (search row is written synchronously
//!      in the commit hook).
//!   2. The canonical markdown file lands in the bare upstream's HEAD
//!      tree within a generous timeout (write job is async).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use drua_core::agent::{AgentRole, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::library::LibraryConfig;
use drua_core::primitives::{AuthSubject, UserId};
use drua_core::skill::SKILL_DOC_TYPE;
use drua_core::{App, AppConfig};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn tests_artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("skill-e2e")
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
    // CASCADE so we don't have to enumerate FK order; RESTART IDENTITY
    // for cleanliness. The list covers everything App::init touches.
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
        },
    );
    builtin_roles.insert(
        AgentRole::Agent,
        RoleConfig {
            chain: Some(llm::ModelChain::new(model.clone())),
            compaction: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::WorkflowStepAgent,
        RoleConfig {
            chain: Some(llm::ModelChain::new(model.clone())),
            compaction: Default::default(),
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

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn skill_create_propagates_to_search_and_upstream() {
    let pool = pool().await;
    reset_db(&pool).await;

    let upstream = init_bare_upstream();
    let data_dir = fresh_dir("library-data");

    let config = AppConfig {
        agents: agents_config_for_tests(),
        library: LibraryConfig {
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            repo_url: Some(upstream.to_string_lossy().into_owned()),
            // 1s is plenty short for the test; the forward-sync hook
            // doesn't depend on the fetch tick — only the inbound
            // (reverse) sync would.
            skill_sync_interval_secs: 1,
        },
        ..Default::default()
    };
    let app = App::init(&pool, config).await.expect("App::init");

    let sub = AuthSubject::User(UserId::new());

    // Create a real project — drives Projects::create which builds the
    // project_lead agent and queues a `runtime/projects/<name>/.gitkeep`
    // scaffold commit through the same forward-sync pipeline.
    let project = app
        .projects()
        .create(&sub, "alpha", None)
        .await
        .expect("create project");

    // Create the skill we'll then assert on.
    let skill = app
        .skills()
        .create(
            &sub,
            project.id,
            &project.name,
            "deploy-prod".into(),
            "Deploys the app to production".into(),
            "#!/bin/bash\necho deploy".into(),
        )
        .await
        .expect("create skill");

    // 1. Search row written synchronously in the SkillRepo
    // post_persist_hook → LibrarySyncHook pre_commit. Visible
    // immediately, no waiting.
    let hits = app
        .library()
        .search()
        .search("deploy", None, &[], &[SKILL_DOC_TYPE], &[], 10)
        .await
        .expect("search");
    let hit = hits
        .iter()
        .find(|h| h.fields.doc_id == uuid::Uuid::from(skill.id))
        .unwrap_or_else(|| panic!("skill not in search index; got {hits:#?}"));
    assert_eq!(hit.fields.doc_type, SKILL_DOC_TYPE);
    assert_eq!(hit.fields.name, "deploy-prod");

    // 2. Upstream commit lands asynchronously via the LibraryWriteJob.
    // Poll until the path lands in the bare upstream's HEAD tree
    // (or fail after a generous timeout). With path-as-identity the
    // path is just `<scope>/skills/<name>.md` — no id-suffix.
    let path = format!("runtime/projects/{}/skills/deploy-prod.md", project.name);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if read_blob(&upstream, &path).is_some() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("upstream commit for {path} did not land within timeout");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let bytes = read_blob(&upstream, &path).expect("blob present");
    let rendered = String::from_utf8(bytes).expect("utf8");
    assert!(
        rendered.contains("name: \"deploy-prod\""),
        "expected frontmatter; got: {rendered}"
    );
    assert!(
        rendered.contains("#!/bin/bash"),
        "expected body; got: {rendered}"
    );
}

fn read_blob(bare: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open_bare(bare).ok()?;
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let tree = head.tree().ok()?;
    let entry = tree.get_path(Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}
