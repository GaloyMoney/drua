mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig, PathDatesMap};

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

/// Fixed day boundaries so `date_naive()` comparisons never straddle a
/// UTC midnight by accident. `T0` = 2025-01-01T00:00:00Z.
const T0: i64 = 1_735_689_600;
const DAY: i64 = 86_400;

fn t(unix_secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(unix_secs, 0).expect("valid timestamp")
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

/// `job::Jobs` is part of the return tuple and must stay bound in the
/// caller for the test's duration — dropping it tears down the
/// resident sync-job initializer holding the fetcher's tick receiver,
/// so the background fetcher exits after its first send fails and no
/// external (`TestRepo::commit`/`commit_at`) push is ever picked up
/// again (see the equivalent warning in `cross_replica_notify.rs`).
async fn fresh_library(test_name: &str) -> (TestRepo, Library, job::Jobs, sqlx::PgPool) {
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
    (fixture, library, jobs, pool)
}

/// Polls `library`'s local mirror (not the DB-side importer/search
/// pipeline — irrelevant here) until `path` reads back as `expected`,
/// or panics after a generous timeout. This is the same "commit
/// upstream, then wait for the fetcher to catch up" handshake
/// `init.rs`'s `init_clones_and_resyncs_fixture_repo` uses; no
/// sleep-based synchronisation.
async fn wait_for_blob(library: &Library, path: &str, expected: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if library
            .read_blob_at_head(path)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(expected)
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{path} did not reach expected content within timeout");
        }
        tokio::time::sleep(Duration::from_millis(FETCH_INTERVAL_MS)).await;
    }
}

async fn path_dates(library: &Library, slug: &str) -> Arc<PathDatesMap> {
    library
        .spaces()
        .path_dates(slug)
        .await
        .expect("path_dates")
        .expect("HEAD is born")
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn created_and_modified_follow_history() {
    let (fixture, library, _jobs, _pool) =
        fresh_library("created_and_modified_follow_history").await;
    let slug = "s";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    fixture.commit_at(&[("spaces/s/a.md", "v1\n")], "add a.md", T0);
    wait_for_blob(&library, "spaces/s/a.md", b"v1\n").await;

    fixture.commit_at(
        &[("spaces/s/a.md", "v2\n"), ("spaces/s/b.md", "new\n")],
        "update a.md, add b.md",
        T0 + DAY,
    );
    wait_for_blob(&library, "spaces/s/b.md", b"new\n").await;

    let dates = path_dates(&library, slug).await;
    let a = dates.get("a.md").expect("a.md present");
    assert_eq!(a.created.date_naive(), t(T0).date_naive());
    assert_eq!(a.modified.date_naive(), t(T0 + DAY).date_naive());
    let b = dates.get("b.md").expect("b.md present");
    assert_eq!(b.created.date_naive(), t(T0 + DAY).date_naive());
    assert_eq!(b.modified.date_naive(), t(T0 + DAY).date_naive());
}

/// The load-bearing test: `Spaces::move_file` commits the same blob at
/// a new path, and `created` must survive the move so the 7-day
/// curation clock doesn't reset on a pure re-file.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn move_keeps_created() {
    let (fixture, library, _jobs, _pool) = fresh_library("move_keeps_created").await;
    let slug = "s";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    fixture.commit_at(&[("spaces/s/a.md", "content\n")], "add a.md", T0);
    wait_for_blob(&library, "spaces/s/a.md", b"content\n").await;

    library
        .spaces()
        .move_file(slug, "a.md", "efforts/e/a.md", attr(), None)
        .await
        .expect("move");

    let dates = path_dates(&library, slug).await;
    assert!(
        !dates.contains_key("a.md"),
        "old path must not linger in the map"
    );
    let moved = dates.get("efforts/e/a.md").expect("moved path present");
    assert_eq!(
        moved.created.date_naive(),
        t(T0).date_naive(),
        "rename must preserve the original created date"
    );
    assert!(
        moved.modified.date_naive() > t(T0).date_naive(),
        "modified must move forward to the rename commit"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn incremental_update_after_head_advances() {
    let (fixture, library, _jobs, _pool) =
        fresh_library("incremental_update_after_head_advances").await;
    let slug = "s";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    fixture.commit_at(&[("spaces/s/a.md", "v1\n")], "add a.md", T0);
    wait_for_blob(&library, "spaces/s/a.md", b"v1\n").await;

    let first = path_dates(&library, slug).await;
    let a_first = *first.get("a.md").expect("a.md present");
    assert!(!first.contains_key("c.md"));

    fixture.commit_at(&[("spaces/s/c.md", "new\n")], "add c.md", T0 + DAY);
    wait_for_blob(&library, "spaces/s/c.md", b"new\n").await;

    let second = path_dates(&library, slug).await;
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a fresh HEAD must produce a fresh Arc, not the stale cached one"
    );
    let c = second
        .get("c.md")
        .expect("c.md present after the new commit");
    assert_eq!(c.created.date_naive(), t(T0 + DAY).date_naive());
    let a_second = *second.get("a.md").expect("a.md still present");
    assert_eq!(
        a_second, a_first,
        "the incremental fold must not perturb dates already established"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn deleted_then_readded_restarts_created() {
    let (fixture, library, _jobs, _pool) =
        fresh_library("deleted_then_readded_restarts_created").await;
    let slug = "s";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    fixture.commit_at(&[("spaces/s/a.md", "v1\n")], "add a.md", T0);
    wait_for_blob(&library, "spaces/s/a.md", b"v1\n").await;

    library
        .spaces()
        .delete_file(slug, "a.md", attr(), None)
        .await
        .expect("delete");

    fixture.commit_at(&[("spaces/s/a.md", "v2\n")], "re-add a.md", T0 + 3 * DAY);
    wait_for_blob(&library, "spaces/s/a.md", b"v2\n").await;

    let dates = path_dates(&library, slug).await;
    let a = dates.get("a.md").expect("a.md present after re-add");
    assert_eq!(a.created.date_naive(), t(T0 + 3 * DAY).date_naive());
    assert_eq!(a.modified.date_naive(), t(T0 + 3 * DAY).date_naive());
}
