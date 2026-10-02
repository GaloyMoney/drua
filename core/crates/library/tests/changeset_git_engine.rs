mod common;

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{
    ApplyOutcome, CommitAttribution, DraftHandle, DraftName, DraftObservation, Library,
    LibraryConfig, RebaseOutcome, SpaceTarget,
};

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

fn new_draft_name() -> DraftName {
    DraftName::from(uuid::Uuid::new_v4())
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
        read_catch_up_timeout_ms: READ_CATCH_UP_TIMEOUT_MS,
    };
    let library = Library::init(&pool, &config, embedder, &mut jobs, None)
        .await
        .expect("library init");
    jobs.start_poll().await.expect("start poll");
    (fixture, library, pool)
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn open_write_and_read_at_tip_leave_main_untouched() {
    let (_fixture, library, _pool) = fresh_library("changeset_create_write_read").await;

    let base = library.drafts().fresh_base().await.expect("fresh base");
    let name = new_draft_name();
    library
        .drafts()
        .open(name, &base)
        .await
        .expect("open draft");
    let draft_target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));

    let tip = library
        .spaces()
        .write_file(
            "demo",
            "note.md",
            "staged content".into(),
            attr(),
            &draft_target,
            None,
        )
        .await
        .expect("write to draft ref")
        .expect("real commit");

    let at_tip = SpaceTarget::Draft(DraftHandle::new(name, tip));
    assert_eq!(
        library
            .spaces()
            .read_file("demo", "note.md", &at_tip)
            .await
            .unwrap(),
        Some(b"staged content".to_vec())
    );
    assert_eq!(
        library
            .spaces()
            .read_file("demo", "note.md", &SpaceTarget::Main)
            .await
            .unwrap(),
        None
    );
    let main_after = library.drafts().current_main().await.unwrap();
    assert_eq!(main_after, base);
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn apply_lands_draft_content_at_head() {
    let (_fixture, library, _pool) = fresh_library("changeset_apply").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    let draft_target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));
    let tip = library
        .spaces()
        .write_file(
            "demo",
            "merged.md",
            "lands on main".into(),
            attr(),
            &draft_target,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    let outcome = library
        .drafts()
        .apply(&tip, "changeset: land it".into(), attr())
        .await
        .expect("apply");
    let merge_oid = match outcome {
        ApplyOutcome::Merged { merge_oid } => merge_oid,
        ApplyOutcome::Conflicts(paths) => {
            panic!("expected a clean merge, got conflicts: {paths:?}")
        }
    };

    assert_eq!(
        library
            .spaces()
            .read_file("demo", "merged.md", &SpaceTarget::Main)
            .await
            .unwrap(),
        Some(b"lands on main".to_vec()),
        "merged content visible at HEAD"
    );
    let main_after = library.drafts().current_main().await.unwrap();
    assert_eq!(main_after, merge_oid);
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn rebase_squashes_onto_moved_main() {
    let (_fixture, library, _pool) = fresh_library("changeset_rebase").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    let draft_target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));
    let changeset_tip = library
        .spaces()
        .write_file(
            "demo",
            "changeset.md",
            "from changeset".into(),
            attr(),
            &draft_target,
            None,
        )
        .await
        .unwrap()
        .expect("real commit");

    library
        .spaces()
        .write_file(
            "demo",
            "on-main.md",
            "from main".into(),
            attr(),
            &SpaceTarget::Main,
            None,
        )
        .await
        .unwrap();

    let outcome = library
        .drafts()
        .rebase(name, &changeset_tip, "changeset: rebase".into(), attr())
        .await
        .expect("rebase");
    let (new_base, new_head) = match outcome {
        RebaseOutcome::Rebased { base_oid, head_oid } => (base_oid, head_oid),
        RebaseOutcome::Conflicts(paths) => {
            panic!("expected a clean rebase, got conflicts: {paths:?}")
        }
    };
    let new_main = library.drafts().current_main().await.unwrap();
    assert_eq!(new_base, new_main);

    let at_new_head = SpaceTarget::Draft(DraftHandle::new(name, new_head));
    assert_eq!(
        library
            .spaces()
            .read_file("demo", "changeset.md", &at_new_head)
            .await
            .unwrap(),
        Some(b"from changeset".to_vec()),
        "the changeset's own edit survives the rebase"
    );
    assert_eq!(
        library
            .spaces()
            .read_file("demo", "on-main.md", &at_new_head)
            .await
            .unwrap(),
        Some(b"from main".to_vec()),
        "main's edit is carried onto the rebased branch"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn handle_recreates_a_missing_ref() {
    let (_fixture, library, _pool) = fresh_library("changeset_handle_recreate").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    // Deliberately skip `open` — the ref has never been created (or was
    // lost). `handle` must repair it at `known_head` and still resolve.
    let handle = library
        .drafts()
        .handle(name, &base)
        .await
        .expect("handle recreates the missing ref");
    assert_eq!(handle.name(), name);
    assert_eq!(handle.tip(), base);

    let names = library.drafts().list().await.expect("list");
    assert!(
        names.contains(&name),
        "expected the recreated ref to show up in list()"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn observe_reports_unchanged_when_nothing_moved() {
    let (_fixture, library, _pool) = fresh_library("changeset_observe_unchanged").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    let main_oid = library.drafts().current_main().await.unwrap();

    let observation = library
        .drafts()
        .observe(name, &base, &base, &main_oid)
        .await
        .expect("observe");
    assert!(
        matches!(observation, DraftObservation::Unchanged),
        "expected Unchanged, got {observation:?}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn observe_reports_merged_into_when_head_landed_on_main() {
    let (_fixture, library, _pool) = fresh_library("changeset_observe_merged").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    let draft_target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));
    let tip = library
        .spaces()
        .write_file("demo", "a.md", "a".into(), attr(), &draft_target, None)
        .await
        .unwrap()
        .unwrap();
    let outcome = library
        .drafts()
        .apply(&tip, "changeset: land".into(), attr())
        .await
        .expect("apply");
    let merge_oid = match outcome {
        ApplyOutcome::Merged { merge_oid } => merge_oid,
        ApplyOutcome::Conflicts(paths) => {
            panic!("expected a clean merge, got conflicts: {paths:?}")
        }
    };

    let observation = library
        .drafts()
        .observe(name, &base, &tip, &merge_oid)
        .await
        .expect("observe");
    match observation {
        DraftObservation::MergedInto { main_oid } => assert_eq!(main_oid, merge_oid),
        other => panic!("expected MergedInto, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn observe_reports_advanced_when_the_ref_moved_without_merging() {
    let (_fixture, library, _pool) = fresh_library("changeset_observe_advanced").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    let draft_target = SpaceTarget::Draft(DraftHandle::new(name, base.clone()));
    let recorded_head = library
        .spaces()
        .write_file("demo", "a.md", "a".into(), attr(), &draft_target, None)
        .await
        .unwrap()
        .unwrap();
    // An external commit lands on the branch directly, past what this
    // caller last recorded.
    let advanced_target = SpaceTarget::Draft(DraftHandle::new(name, recorded_head.clone()));
    let advanced_tip = library
        .spaces()
        .write_file("demo", "b.md", "b".into(), attr(), &advanced_target, None)
        .await
        .unwrap()
        .unwrap();
    let main_oid = library.drafts().current_main().await.unwrap();

    let observation = library
        .drafts()
        .observe(name, &base, &recorded_head, &main_oid)
        .await
        .expect("observe");
    match observation {
        DraftObservation::Advanced { tip } => assert_eq!(tip, advanced_tip),
        other => panic!("expected Advanced, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn observe_reports_missing_when_the_ref_is_gone() {
    let (_fixture, library, _pool) = fresh_library("changeset_observe_missing").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    // Never opened — no ref exists locally or on origin.
    let main_oid = library.drafts().current_main().await.unwrap();

    let observation = library
        .drafts()
        .observe(name, &base, &base, &main_oid)
        .await
        .expect("observe");
    assert!(
        matches!(observation, DraftObservation::Missing),
        "expected Missing, got {observation:?}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn list_skips_a_ref_name_that_does_not_parse_as_a_uuid() {
    let (fixture, library, _pool) = fresh_library("changeset_list_skip").await;

    let base = library.drafts().fresh_base().await.unwrap();
    let name = new_draft_name();
    library.drafts().open(name, &base).await.unwrap();
    // `Drafts` only ever creates refs named by a real uuid; a ref that
    // isn't has to be created directly on the upstream — pushed there
    // immediately so it survives `list`'s own fetch-with-prune.
    fixture.create_ref("refs/heads/drua/not-a-uuid", &base);

    let names = library.drafts().list().await.expect("list");
    assert_eq!(names, vec![name]);
}
