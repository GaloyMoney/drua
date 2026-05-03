// Each integration test file is its own binary; helpers used by some but not
// all of them otherwise trigger dead_code warnings per-binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

static WIPE_ARTIFACTS: Once = Once::new();

/// Per-test scratch root. Prefers `CARGO_TARGET_TMPDIR` (Cargo's writable
/// integration-test scratch dir) so tests work inside the Nix flake-check
/// sandbox where the source tree is read-only. Falls back to
/// `tests/` next to the crate when running outside cargo (e.g. some IDEs).
fn tests_dir() -> PathBuf {
    if let Some(dir) = option_env!("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir);
    }
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

/// `spawn_unique` is a no-op while a row exists, so a stuck "running" job
/// from a crashed previous run would prevent the poller from ever invoking
/// our runner. Tests should call this before constructing `Library`.
pub async fn reset_library_db_state(pool: &sqlx::PgPool) {
    sqlx::query("DELETE FROM job_executions WHERE job_type = 'library.sync'")
        .execute(pool)
        .await
        .expect("delete job_executions");
    sqlx::query(
        "DELETE FROM job_events WHERE id IN (SELECT id FROM jobs WHERE job_type = 'library.sync')",
    )
    .execute(pool)
    .await
    .expect("delete job_events");
    sqlx::query("DELETE FROM jobs WHERE job_type = 'library.sync'")
        .execute(pool)
        .await
        .expect("delete jobs");
    sqlx::query("DELETE FROM library_documents")
        .execute(pool)
        .await
        .expect("delete library_documents");
    sqlx::query("DELETE FROM space_events")
        .execute(pool)
        .await
        .expect("delete space_events");
    sqlx::query("DELETE FROM spaces")
        .execute(pool)
        .await
        .expect("delete spaces");
}

fn fixtures_root() -> PathBuf {
    ensure_artifacts_wiped();
    let root = tests_dir().join("fixtures");
    std::fs::create_dir_all(&root).expect("create fixtures root");
    root
}

/// A bare upstream + a working clone. `path()` returns the bare upstream
/// (libgit2 only pushes to bare); commits go through the work clone and are
/// pushed to upstream by `commit`.
pub struct TestRepo {
    upstream: PathBuf,
    work: PathBuf,
}

impl TestRepo {
    pub fn init(files: &[(&str, &str)]) -> Self {
        let root = fixtures_root();
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        let upstream = root.join(format!("upstream-{stamp}.git"));
        let work = root.join(format!("work-{stamp}"));

        std::fs::create_dir_all(&upstream).expect("create upstream");
        std::fs::create_dir_all(&work).expect("create work");

        git(
            &upstream,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );

        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        git(&work, &["config", "user.email", "test@example.com"]);
        git(&work, &["config", "user.name", "Test"]);
        git(
            &work,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );

        write_files(&work, files);
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "initial commit"]);
        git(&work, &["push", "--quiet", "-u", "origin", "main"]);

        Self { upstream, work }
    }

    /// Path to the bare upstream — pass to `LibraryConfig::repo_url`.
    pub fn path(&self) -> &Path {
        &self.upstream
    }

    /// Add a new commit upstream. Pulls any external pushes (e.g. Library's
    /// `.gitkeep`) into the work clone first so the new commit lands on top.
    pub fn commit(&self, files: &[(&str, &str)], message: &str) {
        git(&self.work, &["fetch", "--quiet", "origin"]);
        git(&self.work, &["reset", "--hard", "origin/main"]);

        write_files(&self.work, files);
        git(&self.work, &["add", "."]);
        git(&self.work, &["commit", "--quiet", "-m", message]);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
    }
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&full, content).expect("write file");
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
