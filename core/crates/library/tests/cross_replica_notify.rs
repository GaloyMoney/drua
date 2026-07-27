mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

/// Ticker effectively disabled: convergence can only come from the
/// cross-replica `library_head_changed` PG NOTIFY wake-up.
const FETCH_INTERVAL_MS: u64 = 3_600_000;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn init_replica(
    test_name: &str,
    replica: &str,
    repo_url: &str,
    pool: &sqlx::PgPool,
    start_poll: bool,
) -> (Library, job::Jobs) {
    let data_dir = library_data_dir(test_name).join(replica);
    let embedder = Arc::new(code_assistant_core::embedder::Embedder::new().expect("embedder"));
    let job_config = job::JobSvcConfig::builder()
        .pool(pool.clone())
        .build()
        .expect("job config");
    let mut jobs = job::Jobs::init(job_config).await.expect("jobs init");
    let config = LibraryConfig {
        data_dir: data_dir.to_string_lossy().to_string(),
        repo_url: repo_url.to_string(),
        fetch_interval_ms: FETCH_INTERVAL_MS,
    };
    let library = Library::init(pool, &config, embedder, &mut jobs, None)
        .await
        .expect("library init");
    if start_poll {
        jobs.start_poll().await.expect("start poll");
    }
    // `jobs` must outlive the test: dropping it drops the sync-job
    // initializer holding the fetcher's tick receiver, and the fetcher
    // exits on the first failed send.
    (library, jobs)
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn write_on_one_replica_is_visible_on_peer_without_ticker() {
    let test_name = "write_on_one_replica_is_visible_on_peer_without_ticker";
    let _ = tracing_subscriber::fmt()
        .with_env_filter("drua_library=debug,info")
        .try_init();
    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let pool = pool().await;
    reset_library_db_state(&pool).await;

    // Only replica A polls jobs, so A is guaranteed to execute the
    // library.write job; B has no poller and a disabled ticker, so it
    // can only converge via the `library_head_changed` PG NOTIFY.
    let repo_url = fixture.path().to_string_lossy().to_string();
    let (replica_a, _jobs_a) = init_replica(test_name, "a", &repo_url, &pool, true).await;
    let (replica_b, _jobs_b) = init_replica(test_name, "b", &repo_url, &pool, false).await;

    let slug = "notify";
    replica_a
        .spaces()
        .create(slug.into(), None, CommitAttribution::library_default())
        .await
        .expect("create space");
    replica_a
        .spaces()
        .write_file(
            slug,
            "doc.md",
            "alpha bravo\n".into(),
            CommitAttribution::library_default(),
        )
        .await
        .expect("write");

    let path = format!("spaces/{slug}/doc.md");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(content)) = replica_b.read_blob_at_head(&path).await {
            assert_eq!(content, b"alpha bravo\n");
            return;
        }
        if Instant::now() >= deadline {
            panic!("peer replica did not converge within timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
