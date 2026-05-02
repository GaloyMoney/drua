#![recursion_limit = "256"]
//! End-to-end test for the skill forward-sync pipeline:
//! Skills::create → SkillRepo post_persist_hook → Library::sync_entity_in_op
//! → drua_library projection (SearchableFields + WriteOp) → search row in
//! pre_commit hook → search index hit + git commit pushed upstream.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drua_core::library::{DocType, Library, LibraryConfig};
use drua_core::primitives::{AuthSubject, ContextGeneration, ProjectId, UserId};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::skill::Skills;

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

/// Bare upstream — Library clones from this and pushes back to it.
fn init_bare_upstream() -> PathBuf {
    let upstream = fresh_dir("upstream").with_extension("git");
    std::fs::create_dir_all(&upstream).expect("create upstream");
    git(
        &upstream,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );

    // Seed with an initial commit so the bare repo has a HEAD; otherwise
    // libgit2's initial fetch finds no refs.
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
    for q in [
        "DELETE FROM job_executions",
        "DELETE FROM job_events",
        "DELETE FROM jobs",
        "DELETE FROM library_documents",
        "DELETE FROM skill_events",
        "DELETE FROM skills",
        "DELETE FROM space_events",
        "DELETE FROM spaces",
    ] {
        sqlx::query(q)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{q} failed: {e}"));
    }
}

#[tokio::test]
#[ignore = "requires postgres + writes a working library clone; run with --ignored"]
async fn skill_create_propagates_to_search_and_upstream() {
    let pool = pool().await;
    reset_db(&pool).await;

    let upstream = init_bare_upstream();
    let data_dir = fresh_dir("library-data");

    let embedder = Arc::new(
        code_assistant_core::embedder::Embedder::new().expect("embedder init"),
    );

    let job_config = job::JobSvcConfig::builder()
        .pool(pool.clone())
        .build()
        .expect("job config");
    let mut jobs = job::Jobs::init(job_config).await.expect("jobs init");

    let library_config = LibraryConfig {
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        repo_url: Some(upstream.to_string_lossy().into_owned()),
        skill_sync_interval_secs: 1,
    };
    let library = Library::init(&library_config, &pool, embedder.clone(), &mut jobs, None)
        .await
        .expect("library init");

    let sandboxes = Arc::new(
        Sandboxes::init(&pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    let skills = Skills::new(
        &pool,
        Arc::clone(&sandboxes),
        library.clone(),
        ContextGeneration::new(),
    );

    jobs.start_poll().await.expect("start poll");

    let sub = AuthSubject::User(UserId::new());
    let project_id = ProjectId::new();
    let project_name = "alpha";
    // Skills.project_id has a FK to projects(id); seed the row so the
    // post_persist_hook → forward_sync chain isn't blocked by a 23503.
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(uuid::Uuid::from(project_id))
        .bind(project_name)
        .execute(&pool)
        .await
        .expect("insert project");
    let skill = skills
        .create(
            &sub,
            project_id,
            project_name,
            "Deploy Prod".into(),
            "Deploys the app to production".into(),
            "#!/bin/bash\necho deploy".into(),
        )
        .await
        .expect("create skill");

    // Forward sync: search row is written synchronously in the
    // SkillRepo post_persist_hook → LibrarySyncHook pre_commit, so it
    // must be visible immediately after Skills::create returns.
    let hits = library
        .search(
            uuid::Uuid::from(project_id),
            "deploy",
            Some(DocType::Skill),
            10,
        )
        .await
        .expect("search");
    let hit = hits
        .iter()
        .find(|h| h.doc_id == uuid::Uuid::from(skill.id))
        .unwrap_or_else(|| panic!("skill not found in search; got {hits:#?}"));
    assert_eq!(hit.doc_type, DocType::Skill);
    assert_eq!(hit.title, "Deploy Prod");

    // Upstream commit lands asynchronously via the LibraryWriteJob.
    // Poll until the canonical path appears in the bare upstream's HEAD
    // tree (or fail after a generous timeout).
    let id_prefix = &uuid::Uuid::from(skill.id).to_string()[..8];
    let canonical_path =
        format!("runtime/projects/{project_name}/skills/deploy-prod-{id_prefix}.md");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if read_blob(&upstream, &canonical_path).is_some() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("upstream commit for {canonical_path} did not land within timeout");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let bytes = read_blob(&upstream, &canonical_path).expect("blob present");
    let rendered = String::from_utf8(bytes).expect("utf8");
    assert!(
        rendered.contains("name: \"Deploy Prod\""),
        "expected canonical frontmatter; got: {rendered}"
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
