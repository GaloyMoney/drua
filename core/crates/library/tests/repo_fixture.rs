mod common;

use common::TestRepo;

#[test]
fn creates_repo_with_dummy_file() {
    let repo = TestRepo::init(&[("spaces/test/dummy.md", "hello\n")]);

    assert!(repo.path().join(".git").is_dir());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("spaces/test/dummy.md")).unwrap(),
        "hello\n",
    );
}
