mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

/// Ticker effectively disabled: the reader can only converge through the
/// cross-replica wake-up, never through its own backstop poll.
const FETCH_INTERVAL_MS: u64 = 3_600_000;

const ROUNDS: usize = 6;
const DOC_PATH: &str = "doc.md";

/// Budget for the setup settle. The read under test never waits.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    // `jobs` must outlive the test: dropping it drops the sync-job initializer
    // holding the fetcher's tick receiver, and the fetcher exits on the first
    // failed send.
    (library, jobs)
}

fn marker(round: usize) -> String {
    format!("marker-{round}\n")
}

async fn write_marker(writer: &Library, slug: &str, round: usize) {
    writer
        .spaces()
        .write_file(
            slug,
            DOC_PATH,
            marker(round),
            CommitAttribution::library_default(),
        )
        .await
        .expect("write acked");
}

/// Parks until the reader has caught up to `round`. Setup only — never used to
/// retry the read under test.
async fn settle_reader_at(reader: &Library, slug: &str, round: usize) {
    let expected = marker(round);
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        if let Ok(Some(content)) = reader.spaces().read_file(slug, DOC_PATH).await {
            if content == expected.as_bytes() {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "reader never converged on {expected:?}; the replicas are not talking at all"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Once a write has been acked on one replica, a read served by another returns
/// it — with no sleeps, polling, or retries on the read side.
///
/// Stronger than `cross_replica_notify.rs`, which polls to a deadline and so
/// asserts only *eventual* convergence. Writes are synchronous through the push,
/// so callers are entitled to the stronger property.
///
/// The failing direction is statistical, as `dev/read-your-write-repro.sh` is: a
/// local upstream loses the race by a millisecond or two where a real git host
/// loses by hundreds. The passing direction is not — a read path that
/// establishes freshness before serving cannot flake.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn write_acked_on_one_replica_is_readable_on_peer_without_waiting() {
    let test_name = "write_acked_on_one_replica_is_readable_on_peer_without_waiting";
    let _ = tracing_subscriber::fmt()
        .with_env_filter("drua_library=debug,info")
        .try_init();

    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let repo_url = fixture.path().to_string_lossy().to_string();
    let pool = pool().await;
    reset_library_db_state(&pool).await;

    // The load balancer's unlucky hand, played deliberately: every write lands
    // on one replica, every read on the other.
    let (writer, _writer_jobs) = init_replica(test_name, "writer", &repo_url, &pool, true).await;
    let (reader, _reader_jobs) = init_replica(test_name, "reader", &repo_url, &pool, false).await;

    let slug = "ryw";
    writer
        .spaces()
        .create(slug.into(), None, CommitAttribution::library_default())
        .await
        .expect("create space");
    write_marker(&writer, slug, 0).await;

    // Settling keeps rounds independent: a fetch still in flight from the
    // previous round could otherwise luck into carrying this round's commit.
    let mut stale_rounds = Vec::new();
    for round in 1..=ROUNDS {
        settle_reader_at(&reader, slug, round - 1).await;
        write_marker(&writer, slug, round).await;

        let observed = reader
            .spaces()
            .read_file(slug, DOC_PATH)
            .await
            .expect("read on peer replica");

        if observed.as_deref() != Some(marker(round).as_bytes()) {
            stale_rounds.push(round);
        }
    }

    assert!(
        stale_rounds.is_empty(),
        "peer replica served stale content in {}/{ROUNDS} rounds: {stale_rounds:?}",
        stale_rounds.len(),
    );
}
