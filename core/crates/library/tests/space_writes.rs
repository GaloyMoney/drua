mod common;

use std::path::Path;
use std::sync::Arc;

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{Library, LibraryConfig, SpaceError};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn read_blob(repo_path: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open_bare(repo_path).ok()?;
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let tree = head.tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

fn path_exists(repo_path: &Path, path: &str) -> bool {
    let Ok(repo) = git2::Repository::open_bare(repo_path) else {
        return false;
    };
    let Ok(head) = repo.head().and_then(|h| h.peel_to_commit()) else {
        return false;
    };
    let Ok(tree) = head.tree() else { return false };
    tree.get_path(std::path::Path::new(path)).is_ok()
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

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn write_then_str_replace_happy_path() {
    let (fixture, library, _pool) = fresh_library("write_then_str_replace_happy_path").await;

    let slug = "writers";
    library
        .spaces()
        .create(slug.into(), None)
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "doc.md", "alpha bravo charlie\n".into())
        .await
        .expect("write");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/doc.md")).as_deref(),
        Some(&b"alpha bravo charlie\n"[..]),
    );

    library
        .spaces()
        .str_replace(slug, "doc.md", "bravo".into(), "BRAVO".into())
        .await
        .expect("str_replace");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/doc.md")).as_deref(),
        Some(&b"alpha BRAVO charlie\n"[..]),
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn str_replace_errors_when_old_str_absent_or_ambiguous() {
    let (_fixture, library, _pool) = fresh_library("str_replace_errors").await;
    let slug = "errors";
    library
        .spaces()
        .create(slug.into(), None)
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "doc.md", "alpha bravo bravo\n".into())
        .await
        .expect("write");

    let absent = library
        .spaces()
        .str_replace(slug, "doc.md", "zulu".into(), "ZULU".into())
        .await;
    assert!(
        matches!(absent, Err(SpaceError::Validation(_))),
        "{absent:?}"
    );

    let ambiguous = library
        .spaces()
        .str_replace(slug, "doc.md", "bravo".into(), "BRAVO".into())
        .await;
    assert!(
        matches!(ambiguous, Err(SpaceError::Validation(_))),
        "{ambiguous:?}"
    );
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn move_then_delete() {
    let (fixture, library, _pool) = fresh_library("move_then_delete").await;
    let slug = "moves";
    library
        .spaces()
        .create(slug.into(), None)
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "old.md", "content\n".into())
        .await
        .expect("write");

    library
        .spaces()
        .move_file(slug, "old.md", "new.md")
        .await
        .expect("move");
    assert!(!path_exists(
        fixture.path(),
        &format!("spaces/{slug}/old.md")
    ));
    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/new.md")).as_deref(),
        Some(&b"content\n"[..]),
    );

    let dup = library.spaces().move_file(slug, "new.md", "new.md").await;
    assert!(matches!(dup, Err(SpaceError::Validation(_))), "{dup:?}");

    let missing = library
        .spaces()
        .move_file(slug, "ghost.md", "elsewhere.md")
        .await;
    assert!(
        matches!(missing, Err(SpaceError::Validation(_))),
        "{missing:?}"
    );

    library
        .spaces()
        .delete_file(slug, "new.md")
        .await
        .expect("delete");
    assert!(!path_exists(
        fixture.path(),
        &format!("spaces/{slug}/new.md")
    ));
}

#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn insert_at_line_number() {
    let (fixture, library, _pool) = fresh_library("insert_at_line_number").await;
    let slug = "inserts";
    library
        .spaces()
        .create(slug.into(), None)
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "list.md", "one\ntwo\nthree\n".into())
        .await
        .expect("write");

    library
        .spaces()
        .insert(slug, "list.md", 1, "one-and-a-half".into())
        .await
        .expect("insert");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/list.md")).as_deref(),
        Some(&b"one\none-and-a-half\ntwo\nthree\n"[..]),
    );
}
