mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{library_data_dir, TestRepo};
use drua_library::{Library, LibraryConfig};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn read_blob(repo: &git2::Repository, path: &str) -> Option<Vec<u8>> {
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let tree = head.tree().ok()?;
    let entry = tree.get_path(Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

#[tokio::test]
async fn init_clones_and_resyncs_fixture_repo() {
    let fixture = TestRepo::init(&[("spaces/test/dummy.md", "hello\n")]);
    let data_dir = library_data_dir("init_clones_and_resyncs_fixture_repo");

    let pool = pool().await;
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

    Library::init(&pool, &config, embedder, &mut jobs, None)
        .await
        .expect("library init");

    jobs.start_poll().await.expect("start poll");

    let repo = git2::Repository::open_bare(&data_dir).expect("open bare clone");
    assert_eq!(
        read_blob(&repo, "spaces/test/dummy.md").as_deref(),
        Some(&b"hello\n"[..]),
    );

    fixture.commit(
        &[("spaces/test/dummy2.md", "world\n")],
        "add spaces/test/dummy2.md",
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let repo = git2::Repository::open_bare(&data_dir).expect("open bare clone");
        if read_blob(&repo, "spaces/test/dummy2.md").as_deref() == Some(&b"world\n"[..]) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("dummy2.md not re-synced within timeout");
        }
        tokio::time::sleep(Duration::from_millis(FETCH_INTERVAL_MS)).await;
    }
}
