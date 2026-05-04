mod common;

use std::path::Path;
use std::sync::Arc;

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig, SpaceError};

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn fresh_library(test_name: &str) -> (TestRepo, Library, sqlx::PgPool) {
    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let data_dir = library_data_dir(test_name);
    let pool = pool().await;
    reset_library_db_state(&pool).await;

    let embedder = Arc::new(code_assistant_core::embedder::Embedder::new().expect("embedder"));
    let job_config = job::JobSvcConfig::builder()
        .pool(pool.clone())
        .build()
        .expect("job config");
    let mut jobs = job::Jobs::init(job_config).await.expect("jobs init");

    let config = LibraryConfig {
        data_dir: data_dir.to_string_lossy().to_string(),
        repo_url: fixture.path().to_string_lossy().to_string(),
        fetch_interval_ms: FETCH_INTERVAL_MS,
    };
    let library = Library::init(&pool, &config, embedder, &mut jobs, None)
        .await
        .expect("library init");
    jobs.start_poll().await.expect("start poll");
    (fixture, library, pool)
}

fn read_blob(repo_path: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let tree = head.tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

fn path_exists(repo_path: &Path, path: &str) -> bool {
    let Ok(repo) = git2::Repository::open(repo_path) else {
        return false;
    };
    let Ok(head) = repo.head().and_then(|h| h.peel_to_commit()) else {
        return false;
    };
    let Ok(tree) = head.tree() else { return false };
    tree.get_path(std::path::Path::new(path)).is_ok()
}

fn upstream_head(repo_path: &Path) -> String {
    let repo = git2::Repository::open(repo_path).expect("open upstream");
    let commit = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .expect("peel head");
    commit.id().to_string()
}

/// Walk upstream main back to (and excluding) `until_oid`, returning each
/// commit's summary. Used to assert how many commits a batch produced
/// without depending on the worker's internal batch-window timing.
fn commits_since(repo_path: &Path, until_oid: &str) -> Vec<String> {
    let repo = git2::Repository::open(repo_path).expect("open upstream");
    let until = git2::Oid::from_str(until_oid).expect("parse oid");
    let mut walk = repo.revwalk().expect("revwalk");
    walk.push_head().expect("push head");
    let mut out = Vec::new();
    for oid in walk {
        let oid = oid.expect("walk oid");
        if oid == until {
            break;
        }
        let commit = repo.find_commit(oid).expect("find commit");
        out.push(commit.summary().unwrap_or("").to_string());
    }
    out
}

/// Six writes submitted concurrently land in the engine's write queue
/// inside one batch window. Two are designed to fail (Validation), four
/// are valid. Asserts:
///
/// 1. Per-op result isolation — each future returns its OWN outcome.
/// 2. Final upstream state reflects only the valid ops.
/// 3. Upstream main grew by exactly four commits on top of the pre-batch
///    HEAD (one commit per valid op, zero for failures), proving the
///    "1 commit per op" semantic.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn concurrent_writes_batch_with_per_op_results() {
    let (fixture, library, _pool) = fresh_library("concurrent_writes_batch").await;
    let slug = "batch";

    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "a.md", "alpha bravo charlie\n".into(), attr())
        .await
        .expect("seed a.md");
    library
        .spaces()
        .write_file(slug, "c.md", "to be deleted\n".into(), attr())
        .await
        .expect("seed c.md");

    let head_before_batch = upstream_head(fixture.path());

    let spaces = library.spaces().clone();
    let s1 = spaces.clone();
    let s2 = spaces.clone();
    let s3 = spaces.clone();
    let s4 = spaces.clone();
    let s5 = spaces.clone();
    let s6 = spaces.clone();

    // Six near-simultaneous writes. The engine's batch window collects
    // them into a single push. Order within the batch follows submission
    // order, so str_replace lands on a.md before insert.
    let (r1, r2, r3, r4, r5, r6) = tokio::join!(
        async move { s1.write_file(slug, "d.md", "delta\n".into(), attr()).await },
        async move {
            s2.str_replace(slug, "a.md", "bravo".into(), "BRAVO".into(), attr())
                .await
        },
        async move {
            s3.str_replace(slug, "missing.md", "x".into(), "y".into(), attr())
                .await
        },
        async move { s4.delete_file(slug, "c.md", attr()).await },
        async move { s5.move_file(slug, "ghost.md", "elsewhere.md", attr()).await },
        async move { s6.insert(slug, "a.md", 1, "appended".into(), attr()).await },
    );

    r1.expect("write d.md");
    r2.expect("str_replace a.md");
    assert!(
        matches!(r3, Err(SpaceError::Validation(_))),
        "expected Validation for missing.md, got {r3:?}",
    );
    r4.expect("delete c.md");
    assert!(
        matches!(r5, Err(SpaceError::Validation(_))),
        "expected Validation for ghost.md, got {r5:?}",
    );
    r6.expect("insert a.md");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/d.md")).as_deref(),
        Some(&b"delta\n"[..]),
    );

    let a = read_blob(fixture.path(), &format!("spaces/{slug}/a.md")).expect("a.md exists");
    let a_str = std::str::from_utf8(&a).expect("utf8");
    assert_eq!(a_str, "alpha BRAVO charlie\nappended\n", "a.md = {a_str:?}");

    assert!(!path_exists(fixture.path(), &format!("spaces/{slug}/c.md"),));
    assert!(!path_exists(
        fixture.path(),
        &format!("spaces/{slug}/missing.md"),
    ));
    assert!(!path_exists(
        fixture.path(),
        &format!("spaces/{slug}/elsewhere.md"),
    ));

    let new_commits = commits_since(fixture.path(), &head_before_batch);
    assert_eq!(
        new_commits.len(),
        4,
        "expected 4 commits, got {} ({new_commits:?})",
        new_commits.len(),
    );
}
