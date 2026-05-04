mod common;

use std::path::Path;
use std::sync::Arc;

use common::{library_data_dir, reset_library_db_state, TestRepo};
use drua_library::{CommitAttribution, Library, LibraryConfig, SpaceError};

fn attr() -> CommitAttribution {
    CommitAttribution::library_default()
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";
const FETCH_INTERVAL_MS: u64 = 100;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn read_blob(repo_path: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let tree = head.tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

/// Borrow-free snapshot of a commit's author/committer + message.
fn head_commit_meta(repo_path: &Path) -> (String, String, String, String, String) {
    let repo = git2::Repository::open(repo_path).expect("open upstream");
    let commit = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .expect("peel head");
    let author = commit.author();
    let committer = commit.committer();
    (
        author.name().unwrap_or("").to_owned(),
        author.email().unwrap_or("").to_owned(),
        committer.name().unwrap_or("").to_owned(),
        committer.email().unwrap_or("").to_owned(),
        commit.message().unwrap_or("").to_owned(),
    )
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
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "doc.md", "alpha bravo charlie\n".into(), attr())
        .await
        .expect("write");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/doc.md")).as_deref(),
        Some(&b"alpha bravo charlie\n"[..]),
    );

    library
        .spaces()
        .str_replace(slug, "doc.md", "bravo".into(), "BRAVO".into(), attr())
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
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "doc.md", "alpha bravo bravo\n".into(), attr())
        .await
        .expect("write");

    let absent = library
        .spaces()
        .str_replace(slug, "doc.md", "zulu".into(), "ZULU".into(), attr())
        .await;
    assert!(
        matches!(absent, Err(SpaceError::Validation(_))),
        "{absent:?}"
    );

    let ambiguous = library
        .spaces()
        .str_replace(slug, "doc.md", "bravo".into(), "BRAVO".into(), attr())
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
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "old.md", "content\n".into(), attr())
        .await
        .expect("write");

    library
        .spaces()
        .move_file(slug, "old.md", "new.md", attr())
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

    let dup = library
        .spaces()
        .move_file(slug, "new.md", "new.md", attr())
        .await;
    assert!(matches!(dup, Err(SpaceError::Validation(_))), "{dup:?}");

    let missing = library
        .spaces()
        .move_file(slug, "ghost.md", "elsewhere.md", attr())
        .await;
    assert!(
        matches!(missing, Err(SpaceError::Validation(_))),
        "{missing:?}"
    );

    library
        .spaces()
        .delete_file(slug, "new.md", attr())
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
        .create(slug.into(), None, attr())
        .await
        .expect("create space");
    library
        .spaces()
        .write_file(slug, "list.md", "one\ntwo\nthree\n".into(), attr())
        .await
        .expect("write");

    library
        .spaces()
        .insert(slug, "list.md", 1, "one-and-a-half".into(), attr())
        .await
        .expect("insert");

    assert_eq!(
        read_blob(fixture.path(), &format!("spaces/{slug}/list.md")).as_deref(),
        Some(&b"one\none-and-a-half\ntwo\nthree\n"[..]),
    );
}

/// End-to-end: a rich `CommitAttribution` (agent-on-behalf-of-human)
/// is rendered into the upstream commit's author, committer, and
/// trailer block.
#[tokio::test]
#[ignore = "requires postgres + writes to tests/.library; run with --ignored"]
async fn write_renders_rich_attribution_into_commit() {
    let (fixture, library, _pool) = fresh_library("write_renders_rich_attribution").await;
    let slug = "attribution";
    library
        .spaces()
        .create(slug.into(), None, attr())
        .await
        .expect("create space");

    let mut rich = CommitAttribution {
        author_name: "Alice (via drua)".to_owned(),
        author_email: "12345+alice@users.noreply.github.com".to_owned(),
        committer_name: "drua-agent[bot]".to_owned(),
        committer_email: "agent@agent.galoy.io".to_owned(),
        kind: drua_library::CommitSubjectKind::AgentOnBehalfOfUser,
        trailers: Vec::new(),
        co_authored_by: Vec::new(),
    };
    rich.add_trailer("Drua-Subject-Type", "agent_on_behalf_of_user");
    rich.add_trailer("Drua-Acting-User", "5f1c8b3a");
    rich.add_trailer("Drua-Project", "a1b2c3d4");
    rich.add_trailer("Drua-Agent", "019df3aa");
    rich.add_trailer("Drua-Action", "space.write_file");
    rich.add_co_authored_by("Alice", "12345+alice@users.noreply.github.com");

    library
        .spaces()
        .write_file(slug, "doc.md", "hello\n".into(), rich)
        .await
        .expect("write doc.md");

    let (a_name, a_email, c_name, c_email, message) = head_commit_meta(fixture.path());
    assert_eq!(a_name, "Alice (via drua)");
    assert_eq!(a_email, "12345+alice@users.noreply.github.com");
    assert_eq!(c_name, "drua-agent[bot]");
    assert_eq!(c_email, "agent@agent.galoy.io");

    assert!(
        message.starts_with(&format!("space:{slug}: write doc.md")),
        "summary missing: {message:?}"
    );
    assert!(
        message.contains("\nCo-Authored-By: Alice <12345+alice@users.noreply.github.com>\n"),
        "co-authored-by missing: {message:?}"
    );
    assert!(
        message.contains("\nDrua-Acting-User: 5f1c8b3a\n"),
        "acting-user missing: {message:?}"
    );
    assert!(
        message.contains("\nDrua-Subject-Type: agent_on_behalf_of_user"),
        "subject-type missing: {message:?}"
    );
    assert!(
        message.contains("\nDrua-Action: space.write_file"),
        "action missing: {message:?}"
    );
    let acting = message.find("Drua-Acting-User").expect("acting present");
    let agent = message.find("Drua-Agent").expect("agent present");
    let project = message.find("Drua-Project").expect("project present");
    let subject = message.find("Drua-Subject-Type").expect("subject present");
    assert!(acting < agent && agent < project && project < subject);
}
