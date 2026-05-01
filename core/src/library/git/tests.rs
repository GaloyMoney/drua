use std::path::Path;

use super::*;

fn make_commit(
    repo: &git2::Repository,
    parent: Option<git2::Oid>,
    files: &[(&str, Option<&[u8]>)],
    message: &str,
) -> git2::Oid {
    let mut index = git2::Index::new().unwrap();
    if let Some(parent_oid) = parent {
        let parent_commit = repo.find_commit(parent_oid).unwrap();
        index.read_tree(&parent_commit.tree().unwrap()).unwrap();
    }
    for (path, content) in files {
        match content {
            Some(bytes) => {
                let blob_oid = repo.blob(bytes).unwrap();
                let entry = git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: bytes.len() as u32,
                    id: blob_oid,
                    flags: 0,
                    flags_extended: 0,
                    path: path.as_bytes().to_vec(),
                };
                index.add(&entry).unwrap();
            }
            None => {
                index.remove_path(Path::new(path)).unwrap();
            }
        }
    }
    let tree_oid = index.write_tree_to(repo).unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::new("test", "test@example.com", &git2::Time::new(0, 0)).unwrap();
    let parents: Vec<git2::Commit> = parent
        .map(|p| vec![repo.find_commit(p).unwrap()])
        .unwrap_or_default();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        message,
        &tree,
        &parent_refs,
    )
    .unwrap()
}

fn origin_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_origin_init_succeeds_head_none() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    assert!(engine.head().await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_advances_head() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();

    let c1 = make_commit(&origin, None, &[("a.md", Some(b"hello"))], "init");
    engine.fetch().await.unwrap();
    let head = engine.head().await.unwrap().unwrap();
    assert_eq!(head.0, c1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_diff_reports_added_modified_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let c1 = make_commit(
        &origin,
        None,
        &[
            ("keep.md", Some(b"v1")),
            ("modify.md", Some(b"old")),
            ("delete.md", Some(b"bye")),
        ],
        "c1",
    );
    let c2 = make_commit(
        &origin,
        Some(c1),
        &[
            ("modify.md", Some(b"new")),
            ("delete.md", None),
            ("add.md", Some(b"hi")),
        ],
        "c2",
    );

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    engine.fetch().await.unwrap();

    let diff = engine
        .tree_diff(Some(CommitOid(c1)), CommitOid(c2))
        .await
        .unwrap();
    let mut paths: Vec<(String, &str)> = diff
        .deltas
        .iter()
        .map(|d| match d {
            Delta::Upserted { path } => (path.clone(), "U"),
            Delta::Deleted { path } => (path.clone(), "D"),
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("add.md".to_string(), "U"),
            ("delete.md".to_string(), "D"),
            ("modify.md".to_string(), "U"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_diff_from_none_lists_all_as_upserted() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let head = make_commit(
        &origin,
        None,
        &[("a.md", Some(b"a")), ("b.md", Some(b"b"))],
        "init",
    );

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    engine.fetch().await.unwrap();

    let diff = engine.tree_diff(None, CommitOid(head)).await.unwrap();
    let mut paths: Vec<String> = diff
        .deltas
        .iter()
        .map(|d| match d {
            Delta::Upserted { path } => path.clone(),
            Delta::Deleted { path } => path.clone(),
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["a.md".to_string(), "b.md".to_string()]);
    assert!(diff
        .deltas
        .iter()
        .all(|d| matches!(d, Delta::Upserted { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_blob_returns_bytes_and_none() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let head = make_commit(&origin, None, &[("a.md", Some(b"hello"))], "init");

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    engine.fetch().await.unwrap();

    let bytes = engine
        .read_blob_at(CommitOid(head), "a.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"hello");

    let missing = engine
        .read_blob_at(CommitOid(head), "missing.md")
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_directory_paths_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let c1 = make_commit(
        &origin,
        None,
        &[("runtime/projects/foo/skills/bar.md", Some(b"v1"))],
        "init",
    );
    let c2 = make_commit(
        &origin,
        Some(c1),
        &[("runtime/projects/foo/skills/bar.md", Some(b"v2"))],
        "update",
    );

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    engine.fetch().await.unwrap();

    let diff = engine
        .tree_diff(Some(CommitOid(c1)), CommitOid(c2))
        .await
        .unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert!(matches!(
        &diff.deltas[0],
        Delta::Upserted { path } if path == "runtime/projects/foo/skills/bar.md"
    ));

    let bytes = engine
        .read_blob_at(CommitOid(c2), "runtime/projects/foo/skills/bar.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"v2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_fetch_no_change() {
    let tmp = tempfile::tempdir().unwrap();
    let origin_path = tmp.path().join("origin.git");
    let origin = git2::Repository::init_bare(&origin_path).unwrap();
    let local_path = tmp.path().join("local.git");

    let head = make_commit(&origin, None, &[("a.md", Some(b"a"))], "init");

    let engine = GitEngine::init(Some(&origin_url(&origin_path)), local_path, None)
        .await
        .unwrap();
    engine.fetch().await.unwrap();
    let h1 = engine.head().await.unwrap().unwrap();
    engine.fetch().await.unwrap();
    let h2 = engine.head().await.unwrap().unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1.0, head);
}

#[test]
fn commit_oid_hex_round_trip() {
    let zero = git2::Oid::zero();
    let oid = CommitOid(zero);
    let hex = oid.to_hex();
    let parsed = CommitOid::from_hex(&hex).unwrap();
    assert_eq!(oid, parsed);
}
