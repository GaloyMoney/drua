mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{
    CommitAttribution, DraftHandle, DraftName, Library, LibraryConfig, LibraryError, SpaceTarget,
};

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const WRITER_FETCH_INTERVAL_MS: u64 = 100;
/// Effectively disabled: the reader must converge purely on the obix
/// wake-up + read-side wait, never the ticker backstop.
const READER_FETCH_INTERVAL_MS: u64 = 3_600_000;
const READ_CATCH_UP_TIMEOUT_MS: u64 = 5_000;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn init_replica(
    test_name: &str,
    replica: &str,
    repo_url: &str,
    pool: &sqlx::PgPool,
    fetch_interval_ms: u64,
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
        fetch_interval_ms,
        read_catch_up_timeout_ms: READ_CATCH_UP_TIMEOUT_MS,
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

/// Red-first requirement (handoff §6): applied to main WITHOUT the
/// read-wait gate, this must fail with a stale read on round >= 1 — the
/// write acks, B still has the old HEAD. See the "space reads at HEAD
/// wait for the published main head" commit body for the captured
/// failure output.
///
/// (B's `fetch_interval_ms` is 3_600_000 — the ticker cannot be what
/// accidentally makes this pass.)
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn reader_sees_writer_main_writes_without_a_fetch_ticker() {
    let test_name = "reader_sees_writer_main_writes_without_a_fetch_ticker";
    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let pool = pool().await;
    reset_library_db_state(&pool).await;
    let repo_url = fixture.path().to_string_lossy().to_string();

    let (writer, _jobs_w) = init_replica(
        test_name,
        "writer",
        &repo_url,
        &pool,
        WRITER_FETCH_INTERVAL_MS,
        true,
    )
    .await;
    let (reader, _jobs_r) = init_replica(
        test_name,
        "reader",
        &repo_url,
        &pool,
        READER_FETCH_INTERVAL_MS,
        false,
    )
    .await;

    let slug = "ryw-main";
    writer
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    const ROUNDS: usize = 5;
    let mut completed_rounds = 0;
    for round in 0..ROUNDS {
        let body = format!("round {round}\n");
        writer
            .spaces()
            .write_file(slug, "doc.md", body.clone(), attr(), &SpaceTarget::Main)
            .await
            .expect("write");

        // Exactly one read, no sleeps/polling/retries around it — the
        // wait (if any) must happen inside the read itself.
        let read = reader
            .spaces()
            .read_file(slug, "doc.md", &SpaceTarget::Main)
            .await
            .expect("read");
        assert_eq!(
            read,
            Some(body.into_bytes()),
            "round {round}: reader must see the writer's just-acked write"
        );
        completed_rounds += 1;
    }
    assert_eq!(
        completed_rounds, ROUNDS,
        "a zero-round pass must be impossible"
    );
}

/// New behaviour (no red-first requirement — this path doesn't exist
/// before the read-wait gate lands): a reader whose own clone can never
/// fetch again must fail with `StaleReplica` within the catch-up
/// timeout, never silently serve the old content.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn stale_reader_times_out_rather_than_serving_stale_content() {
    let test_name = "stale_reader_times_out_rather_than_serving_stale_content";
    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let pool = pool().await;
    reset_library_db_state(&pool).await;
    let repo_url = fixture.path().to_string_lossy().to_string();

    let (writer, _jobs_w) = init_replica(
        test_name,
        "writer",
        &repo_url,
        &pool,
        WRITER_FETCH_INTERVAL_MS,
        true,
    )
    .await;
    let (reader, _jobs_r) = init_replica(
        test_name,
        "reader",
        &repo_url,
        &pool,
        READER_FETCH_INTERVAL_MS,
        false,
    )
    .await;

    let slug = "ryw-timeout";
    writer
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    writer
        .spaces()
        .write_file(slug, "doc.md", "first\n".into(), attr(), &SpaceTarget::Main)
        .await
        .expect("write 1");

    let path = format!("spaces/{slug}/doc.md");
    // Catch the reader up on the first write before severing it, so the
    // timeout below is genuinely "upstream unreachable", not just
    // "never fetched since boot".
    let seen = reader
        .read_blob_at_head(&path)
        .await
        .expect("initial read catches the reader up");
    assert_eq!(seen, Some(b"first\n".to_vec()));

    // Sever ONLY the reader's ability to fetch: point its own clone's
    // `origin` remote at a path that doesn't exist. The shared upstream
    // (and the writer's pushes to it) is untouched.
    let reader_repo_path = library_data_dir(test_name).join("reader");
    {
        let repo = git2::Repository::open_bare(&reader_repo_path).expect("open reader clone");
        repo.remote_set_url("origin", "/nonexistent/does-not-exist.git")
            .expect("sever reader's origin");
    }

    writer
        .spaces()
        .write_file(
            slug,
            "doc.md",
            "second\n".into(),
            attr(),
            &SpaceTarget::Main,
        )
        .await
        .expect("write 2");

    let start = Instant::now();
    let result = reader.read_blob_at_head(&path).await;
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(LibraryError::StaleReplica { .. })),
        "a reader that can never catch up must error, not serve stale content: {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(READ_CATCH_UP_TIMEOUT_MS) + Duration::from_secs(2),
        "must fail within the catch-up timeout plus a small epsilon, not hang: {elapsed:?}"
    );
}

/// Red-first requirement (handoff §6): applied to the draft path
/// WITHOUT the `Drafts::handle` fix, this must fail specifically in
/// the "ref exists locally but is behind" case — round 2 below, where
/// B already resolved the ref once (round 1) and so has it locally,
/// but hasn't fetched the second write. That is the case the
/// already-handled "ref missing" path does not cover. See the "draft
/// handles wait for a replica that is behind head_oid" commit body
/// for the captured failure output.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn reader_draft_handle_waits_for_a_replica_that_is_behind_head_oid() {
    let test_name = "reader_draft_handle_waits_for_a_replica_that_is_behind_head_oid";
    let fixture = TestRepo::init(&[("README.md", "init\n")]);
    let pool = pool().await;
    reset_library_db_state(&pool).await;
    let repo_url = fixture.path().to_string_lossy().to_string();

    let (writer, _jobs_w) = init_replica(
        test_name,
        "writer",
        &repo_url,
        &pool,
        WRITER_FETCH_INTERVAL_MS,
        true,
    )
    .await;
    let (reader, _jobs_r) = init_replica(
        test_name,
        "reader",
        &repo_url,
        &pool,
        READER_FETCH_INTERVAL_MS,
        false,
    )
    .await;

    let slug = "ryw-draft";
    writer
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    let base = writer.drafts().fresh_base().await.expect("fresh base");
    let name = DraftName::from(uuid::Uuid::new_v4());
    writer.drafts().open(name, &base).await.expect("open draft");

    // Round 1: A writes the draft's first content. B resolves the ref
    // for the first time (it doesn't exist in B's clone yet) — this is
    // the already-handled "missing ref" case, done here to put the ref
    // in B's clone for round 2.
    let tip1 = writer
        .spaces()
        .write_file(
            slug,
            "note.md",
            "round 1\n".into(),
            attr(),
            &SpaceTarget::Draft(DraftHandle::new(name, base.clone())),
        )
        .await
        .expect("write 1")
        .expect("real commit");

    let handle_b1 = reader
        .drafts()
        .handle(name, &tip1)
        .await
        .expect("round 1: handle resolves (ref missing -> fetch)");
    assert_eq!(handle_b1.tip(), tip1, "round 1: handle is at the new tip");
    let read1 = reader
        .spaces()
        .read_file(slug, "note.md", &SpaceTarget::Draft(handle_b1))
        .await
        .expect("round 1: read");
    assert_eq!(read1, Some(b"round 1\n".to_vec()));

    // Round 2: A writes again, advancing the draft. B's ref now EXISTS
    // locally (from round 1) but is BEHIND tip2 — the case that
    // distinguishes the real bug from round 1's missing-ref case.
    let tip2 = writer
        .spaces()
        .write_file(
            slug,
            "note.md",
            "round 2\n".into(),
            attr(),
            &SpaceTarget::Draft(DraftHandle::new(name, tip1.clone())),
        )
        .await
        .expect("write 2")
        .expect("real commit");
    assert_ne!(tip1, tip2, "round 2 must actually advance the draft");

    // One handle() call, no sleeps/polling/retries around it.
    let handle_b2 = reader
        .drafts()
        .handle(name, &tip2)
        .await
        .expect("round 2: handle must wait for the fetch, not return the stale tip");
    assert_eq!(
        handle_b2.tip(),
        tip2,
        "round 2: handle must be the new tip, not the stale local one"
    );
    let read2 = reader
        .spaces()
        .read_file(slug, "note.md", &SpaceTarget::Draft(handle_b2))
        .await
        .expect("round 2: read");
    assert_eq!(read2, Some(b"round 2\n".to_vec()));
}
