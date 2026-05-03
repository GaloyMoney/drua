mod common;

use std::path::Path;

use common::TestRepo;

#[test]
fn creates_repo_with_dummy_file() {
    let repo = TestRepo::init(&[("spaces/test/dummy.md", "hello\n")]);

    assert!(repo.path().join("HEAD").is_file());
    assert!(repo.path().join("objects").is_dir());

    let bare = git2::Repository::open_bare(repo.path()).expect("open bare upstream");
    let head = bare
        .head()
        .expect("head ref")
        .peel_to_commit()
        .expect("head commit");
    let entry = head
        .tree()
        .expect("head tree")
        .get_path(Path::new("spaces/test/dummy.md"))
        .expect("dummy.md in tree");
    let blob = bare.find_blob(entry.id()).expect("blob");
    assert_eq!(blob.content(), b"hello\n");
}
