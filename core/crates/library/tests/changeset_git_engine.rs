mod common;

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig};

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

    let embedder =
        std::sync::Arc::new(code_assistant_core::embedder::Embedder::new().expect("embedder"));
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

/// A changeset branch, created and written to through the full async
/// `Library` surface (not the sync `git.rs` unit-test helpers), proving
/// `create_ref` + `write_file_at` + `read_blob_at`/`resolve_ref` compose
/// end to end and that `main` never moves.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn create_ref_write_and_read_at_tip_leave_main_untouched() {
    let (_fixture, library, _pool) = fresh_library("changeset_create_write_read").await;

    let base = library
        .resolve_ref("refs/heads/main")
        .await
        .expect("resolve main")
        .expect("main exists after init");

    let refname = "refs/heads/drua/test-changeset-1";
    library
        .create_ref(refname, &base)
        .await
        .expect("create changeset ref");

    let tip = library
        .write_file_at(
            Some(refname.to_string()),
            "spaces/demo/note.md".into(),
            b"staged content".to_vec(),
            "changeset: add note".into(),
            attr(),
        )
        .await
        .expect("write to changeset ref")
        .expect("real commit");

    // Visible at the changeset tip...
    assert_eq!(
        library
            .read_blob_at(&tip, "spaces/demo/note.md")
            .await
            .unwrap(),
        Some(b"staged content".to_vec())
    );
    // ...but not through main (HEAD).
    assert_eq!(
        library
            .read_blob_at_head("spaces/demo/note.md")
            .await
            .unwrap(),
        None
    );
    // main's oid is exactly what it was before the changeset write.
    let main_after = library
        .resolve_ref("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(main_after, base);
}

/// `merge_into_main` lands a changeset's tip as a 2-parent commit and
/// its content becomes visible at HEAD.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn merge_into_main_lands_changeset_content_at_head() {
    let (_fixture, library, _pool) = fresh_library("changeset_merge_into_main").await;

    let base = library
        .resolve_ref("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    let refname = "refs/heads/drua/test-changeset-2";
    library.create_ref(refname, &base).await.unwrap();
    let tip = library
        .write_file_at(
            Some(refname.to_string()),
            "spaces/demo/merged.md".into(),
            b"lands on main".to_vec(),
            "changeset: add merged.md".into(),
            attr(),
        )
        .await
        .unwrap()
        .unwrap();

    let merge_oid = library
        .merge_into_main(&tip, "changeset: land test-changeset-2".into(), attr())
        .await
        .expect("merge into main");

    assert_eq!(
        library
            .read_blob_at_head("spaces/demo/merged.md")
            .await
            .unwrap(),
        Some(b"lands on main".to_vec()),
        "merged content visible at HEAD"
    );
    let main_after = library
        .resolve_ref("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(main_after, merge_oid);
}

/// `rebase_ref` squashes the changeset branch onto a moved `main`,
/// carrying forward the changeset's own edit while main's edit stays.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn rebase_ref_squashes_onto_moved_main() {
    let (_fixture, library, _pool) = fresh_library("changeset_rebase").await;

    let base = library
        .resolve_ref("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    let refname = "refs/heads/drua/test-changeset-3";
    library.create_ref(refname, &base).await.unwrap();
    library
        .write_file_at(
            Some(refname.to_string()),
            "spaces/demo/changeset.md".into(),
            b"from changeset".to_vec(),
            "changeset: add changeset.md".into(),
            attr(),
        )
        .await
        .unwrap();

    // main moves independently in the meantime.
    library
        .write_file_at(
            None,
            "spaces/demo/on-main.md".into(),
            b"from main".to_vec(),
            "main: add on-main.md".into(),
            attr(),
        )
        .await
        .unwrap();
    let new_main = library
        .resolve_ref("refs/heads/main")
        .await
        .unwrap()
        .unwrap();

    let (new_base, new_head) = library
        .rebase_ref(refname, &new_main, "changeset: rebase".into(), attr())
        .await
        .expect("rebase_ref engine call")
        .expect("clean rebase, no conflicts");
    assert_eq!(new_base, new_main);

    assert_eq!(
        library
            .read_blob_at(&new_head, "spaces/demo/changeset.md")
            .await
            .unwrap(),
        Some(b"from changeset".to_vec()),
        "the changeset's own edit survives the rebase"
    );
    assert_eq!(
        library
            .read_blob_at(&new_head, "spaces/demo/on-main.md")
            .await
            .unwrap(),
        Some(b"from main".to_vec()),
        "main's edit is carried onto the rebased branch"
    );
}
