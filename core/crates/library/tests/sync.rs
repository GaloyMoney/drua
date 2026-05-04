mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn space_file_imports_into_search_store() {
    let fixture = TestRepo::init(&[("README.md", "initial\n")]);
    let data_dir = library_data_dir("space_file_imports_into_search_store");

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

    // 1. Create the space (DB row + .gitkeep pushed upstream).
    let slug = format!("oncall-{}", uuid::Uuid::new_v4().simple());
    library
        .spaces()
        .create(
            slug.clone(),
            Some("on-call rotation".into()),
            CommitAttribution::library_default(),
        )
        .await
        .expect("create space");

    // 2. Push a file into the space upstream (simulating an out-of-band author).
    fixture.commit(
        &[(
            &format!("spaces/{slug}/runbook.md"),
            "alpha bravo charlie delta\n",
        )],
        "add oncall runbook",
    );

    // 3. Wait for the importer to dispatch the file into library_documents.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let hits = library
            .search()
            .search("bravo", None, &[], &[], &[], 10)
            .await
            .expect("search");
        if let Some(hit) = hits.iter().find(|h| {
            h.fields.scope_slug.as_deref() == Some(slug.as_str())
                && h.fields.path.as_deref() == Some("runbook.md")
        }) {
            assert_eq!(hit.fields.name, "runbook.md");
            assert!(
                hit.fields.content.contains("bravo"),
                "content was {:?}",
                hit.fields.content
            );
            assert_eq!(hit.fields.doc_type.as_str(), "space_file");
            return;
        }
        if Instant::now() >= deadline {
            panic!("runbook.md not found in search store within timeout");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
