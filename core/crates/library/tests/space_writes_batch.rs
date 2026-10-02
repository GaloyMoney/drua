mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{
    CommitAttribution, DraftHandle, DraftName, Library, LibraryConfig, OnCommitted, SpaceError,
    SpaceTarget,
};

/// Mirrors `LIBRARY_PUSH_LOCK_NAMESPACE` in `git.rs` (not exported — it's
/// an implementation detail of the push-lock mechanism). Used only to
/// probe, from a second connection, that the lock is still held while an
/// `on_committed` callback runs (test 4 below).
const LIBRARY_PUSH_LOCK_NAMESPACE: i32 = 0x6472_7561;

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;
const READ_CATCH_UP_TIMEOUT_MS: u64 = 5_000;

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
        read_catch_up_timeout_ms: READ_CATCH_UP_TIMEOUT_MS,
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
        .write_file(
            slug,
            "a.md",
            "alpha bravo charlie\n".into(),
            attr(),
            &drua_library::SpaceTarget::Main,
            None,
        )
        .await
        .expect("seed a.md");
    library
        .spaces()
        .write_file(
            slug,
            "c.md",
            "to be deleted\n".into(),
            attr(),
            &drua_library::SpaceTarget::Main,
            None,
        )
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
        async move {
            s1.write_file(
                slug,
                "d.md",
                "delta\n".into(),
                attr(),
                &drua_library::SpaceTarget::Main,
                None,
            )
            .await
        },
        async move {
            s2.str_replace(
                slug,
                "a.md",
                "bravo".into(),
                "BRAVO".into(),
                attr(),
                &drua_library::SpaceTarget::Main,
                None,
            )
            .await
        },
        async move {
            s3.str_replace(
                slug,
                "missing.md",
                "x".into(),
                "y".into(),
                attr(),
                &drua_library::SpaceTarget::Main,
                None,
            )
            .await
        },
        async move {
            s4.delete_file(slug, "c.md", attr(), &drua_library::SpaceTarget::Main, None)
                .await
        },
        async move {
            s5.move_file(
                slug,
                "ghost.md",
                "elsewhere.md",
                attr(),
                &drua_library::SpaceTarget::Main,
                None,
            )
            .await
        },
        async move {
            s6.insert(
                slug,
                "a.md",
                1,
                "appended".into(),
                attr(),
                &drua_library::SpaceTarget::Main,
                None,
            )
            .await
        },
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

/// Commit oids reachable from `refname`'s tip back to (and excluding)
/// `until_oid`, oldest first — i.e. commit order. Used to check an
/// `on_committed` callback's invocation order against the branch's
/// actual history, independent of which op produced which commit.
fn commits_on_ref(repo_path: &Path, refname: &str, until_oid: &str) -> Vec<String> {
    let repo = git2::Repository::open(repo_path).expect("open upstream");
    let tip = repo
        .find_reference(refname)
        .expect("find ref")
        .target()
        .expect("ref target");
    let until = git2::Oid::from_str(until_oid).expect("parse oid");
    let mut walk = repo.revwalk().expect("revwalk");
    walk.push(tip).expect("push tip");
    let mut out = Vec::new();
    for oid in walk {
        let oid = oid.expect("walk oid");
        if oid == until {
            break;
        }
        out.push(oid.to_string());
    }
    out.reverse();
    out
}

/// §6 test 1 (handoff): six concurrent writes to one draft ref, each
/// carrying an `on_committed` that records its own oid and flips a
/// per-op flag. Asserts the engine answers each write's caller only
/// after that write's own callback has run, that every callback ran
/// exactly once, and that the recorded oids are exactly the branch's
/// commits in commit order — regardless of which op produced which
/// commit, since callbacks run in op order = commit order under the
/// ref's push lock.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn on_committed_runs_before_the_caller_is_answered_in_commit_order() {
    let (fixture, library, _pool) = fresh_library("on_committed_runs_before_caller_answered").await;
    let slug = "cb-order";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    let base = library.drafts().fresh_base().await.expect("fresh base");
    let name = DraftName::from(uuid::Uuid::new_v4());
    library
        .drafts()
        .open(name, &base)
        .await
        .expect("open draft");
    let refname = format!("refs/heads/{}", name.branch());
    let target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let spaces = library.spaces().clone();
    let mut handles = Vec::new();
    for i in 0..6 {
        let spaces = spaces.clone();
        let target = target.clone();
        let recorded = Arc::clone(&recorded);
        handles.push(tokio::spawn(async move {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag_cb = Arc::clone(&flag);
            let recorded_cb = Arc::clone(&recorded);
            let on_committed: OnCommitted = Box::new(move |oid: String| {
                Box::pin(async move {
                    recorded_cb.lock().unwrap().push(oid);
                    flag_cb.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });
            let result = spaces
                .write_file(
                    slug,
                    &format!("f{i}.md"),
                    format!("content {i}\n"),
                    attr(),
                    &target,
                    Some(on_committed),
                )
                .await;
            result.unwrap_or_else(|e| panic!("write {i} failed: {e}"));
            assert!(
                flag.load(Ordering::SeqCst),
                "write {i}'s caller was answered before its own on_committed callback ran"
            );
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let recorded = recorded.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        6,
        "every callback must have run exactly once"
    );
    let branch_commits = commits_on_ref(fixture.path(), &refname, &base);
    assert_eq!(
        recorded, branch_commits,
        "callback invocation order must equal commit order"
    );
}

/// §6 test 2 (handoff): a no-op write (identical content, `Ok(None)`)
/// and a failed RMW (`Err(Validation)`) must invoke no callback at all.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn no_op_and_failed_writes_invoke_no_callback() {
    let (_fixture, library, _pool) = fresh_library("no_op_and_failed_invoke_no_callback").await;
    let slug = "cb-none";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(
            slug,
            "a.md",
            "seed\n".into(),
            attr(),
            &SpaceTarget::Main,
            None,
        )
        .await
        .expect("seed");

    let calls = Arc::new(AtomicUsize::new(0));

    let count_cb = Arc::clone(&calls);
    let on_committed: OnCommitted = Box::new(move |_oid: String| {
        Box::pin(async move {
            count_cb.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let no_op_result = library
        .spaces()
        .write_file(
            slug,
            "a.md",
            "seed\n".into(),
            attr(),
            &SpaceTarget::Main,
            Some(on_committed),
        )
        .await
        .expect("identical-content write succeeds as a no-op");
    assert_eq!(
        no_op_result, None,
        "identical content must produce no commit"
    );

    let count_cb = Arc::clone(&calls);
    let on_committed: OnCommitted = Box::new(move |_oid: String| {
        Box::pin(async move {
            count_cb.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let failed_result = library
        .spaces()
        .str_replace(
            slug,
            "a.md",
            "absent".into(),
            "x".into(),
            attr(),
            &SpaceTarget::Main,
            Some(on_committed),
        )
        .await;
    assert!(
        matches!(failed_result, Err(SpaceError::Validation(_))),
        "{failed_result:?}"
    );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "neither the no-op nor the failed op may invoke on_committed"
    );
}

/// §6 test 3 (handoff): an `on_committed` callback that returns `Err`
/// must not fail the write — the commit already landed upstream.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn a_failing_callback_does_not_fail_the_write() {
    let (fixture, library, _pool) = fresh_library("failing_callback_does_not_fail_write").await;
    let slug = "cb-fails";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    let on_committed: OnCommitted = Box::new(move |_oid: String| {
        Box::pin(async move {
            Err(Box::<dyn std::error::Error + Send + Sync>::from(
                "simulated journal failure",
            ))
        })
    });
    let oid = library
        .spaces()
        .write_file(
            slug,
            "a.md",
            "content\n".into(),
            attr(),
            &SpaceTarget::Main,
            Some(on_committed),
        )
        .await
        .expect("write must still succeed despite the callback's Err")
        .expect("a real commit, not a no-op");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/a.md")).as_deref(),
        Some(&b"content\n"[..]),
        "the commit must be on the ref regardless of the callback's outcome"
    );
    assert_eq!(upstream_head(fixture.path()), oid);
}

/// §6 test 4 (handoff): from inside `on_committed`, the ref's push lock
/// must still be held — probed with the matching two-argument
/// `pg_try_advisory_lock(LIBRARY_PUSH_LOCK_NAMESPACE, hashtext(refname))`
/// form from a SECOND connection (advisory locks are re-entrant per
/// session, so checking from the writer's own connection would be
/// vacuous).
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn on_committed_runs_while_the_refs_push_lock_is_held() {
    let (_fixture, library, pool) = fresh_library("on_committed_runs_under_push_lock").await;
    let slug = "cb-lock";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    let refname = "refs/heads/main".to_string();
    let probe_pool = pool.clone();
    let probe_refname = refname.clone();
    let on_committed: OnCommitted = Box::new(move |_oid: String| {
        Box::pin(async move {
            let mut conn = probe_pool.acquire().await.expect("acquire probe conn");
            let acquired: bool =
                sqlx::query_scalar("SELECT pg_try_advisory_lock($1, hashtext($2))")
                    .bind(LIBRARY_PUSH_LOCK_NAMESPACE)
                    .bind(&probe_refname)
                    .fetch_one(&mut *conn)
                    .await
                    .expect("probe pg_try_advisory_lock");
            assert!(
                !acquired,
                "the ref's push lock must still be held while on_committed runs"
            );
            Ok(())
        })
    });

    library
        .spaces()
        .write_file(
            slug,
            "a.md",
            "content\n".into(),
            attr(),
            &SpaceTarget::Main,
            Some(on_committed),
        )
        .await
        .expect("write");
}
