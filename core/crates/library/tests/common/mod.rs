use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

static WIPE_ARTIFACTS: Once = Once::new();

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn ensure_artifacts_wiped() {
    WIPE_ARTIFACTS.call_once(|| {
        let tests = tests_dir();
        let _ = std::fs::remove_dir_all(tests.join("fixtures"));
        let _ = std::fs::remove_dir_all(tests.join(".library"));
    });
}

pub fn library_data_dir(test_name: &str) -> PathBuf {
    ensure_artifacts_wiped();
    tests_dir().join(".library").join(test_name)
}

fn fixtures_root() -> PathBuf {
    ensure_artifacts_wiped();
    let root = tests_dir().join("fixtures");
    std::fs::create_dir_all(&root).expect("create fixtures root");
    root
}

pub struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    pub fn init(files: &[(&str, &str)]) -> Self {
        let path = fixtures_root().join(format!(
            "repo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&path).expect("create repo dir");

        git(&path, &["init", "--quiet", "--initial-branch=main"]);
        git(&path, &["config", "user.email", "test@example.com"]);
        git(&path, &["config", "user.name", "Test"]);

        for (rel, content) in files {
            let full = path.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir -p");
            }
            std::fs::write(&full, content).expect("write file");
        }

        git(&path, &["add", "."]);
        git(&path, &["commit", "--quiet", "-m", "initial commit"]);

        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn commit(&self, files: &[(&str, &str)], message: &str) {
        for (rel, content) in files {
            let full = self.path.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir -p");
            }
            std::fs::write(&full, content).expect("write file");
        }
        git(&self.path, &["add", "."]);
        git(&self.path, &["commit", "--quiet", "-m", message]);
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}
