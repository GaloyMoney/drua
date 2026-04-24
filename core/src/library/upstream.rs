use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use super::file::GitFileHash;
use super::LibraryError;

#[derive(Clone)]
pub(super) struct Upstream {
    repo_path: PathBuf,
}

impl Upstream {
    pub async fn init(repo_url: Option<&str>, repo_path: PathBuf) -> Result<Self, LibraryError> {
        if let Some(url) = repo_url {
            if !repo_path.join(".git").exists() {
                clone(url, &repo_path).await?;
            }
        }
        Ok(Self { repo_path })
    }

    /// Compute the git blob SHA-1 of the file on disk, or `None` if the file
    /// doesn't exist.  Uses the same `blob <len>\0<content>` format as
    /// `git hash-object`, matching [`RuntimeFile::file_hash`].
    pub async fn file_hash_on_disk(&self, relative_path: &str) -> Option<GitFileHash> {
        let full_path = self.repo_path.join(relative_path);
        let content = tokio::fs::read(&full_path).await.ok()?;
        let header = format!("blob {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(&content);
        Some(GitFileHash::from_sha1(format!("{:x}", hasher.finalize())))
    }

    pub async fn write_file(&self, relative_path: &str, content: &str) -> Result<(), LibraryError> {
        let full_path = self.repo_path.join(relative_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| LibraryError::Io(e.to_string()))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| LibraryError::Io(e.to_string()))?;
        tracing::info!(path = %full_path.display(), "wrote runtime file");
        Ok(())
    }

    pub async fn pull(&self) -> Result<(), LibraryError> {
        if !self.repo_path.join(".git").exists() {
            return Ok(());
        }

        let pull = tokio::process::Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git pull: {e}")))?;
        if !pull.status.success() {
            let stderr = String::from_utf8_lossy(&pull.stderr);
            // Empty remote (no commits yet) — nothing to pull, not an error.
            if stderr.contains("no such ref was fetched") {
                tracing::debug!("pull skipped: remote has no commits yet");
                return Ok(());
            }
            return Err(LibraryError::Git(format!("git pull failed: {stderr}")));
        }

        tracing::info!("pulled latest from remote");
        Ok(())
    }

    pub async fn add_and_commit(
        &self,
        relative_path: &str,
        message: &str,
    ) -> Result<(), LibraryError> {
        if !self.repo_path.join(".git").exists() {
            return Ok(());
        }

        let add = tokio::process::Command::new("git")
            .args(["add", "--", relative_path])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git add: {e}")))?;
        if !add.status.success() {
            return Err(LibraryError::Git(format!(
                "git add failed: {}",
                String::from_utf8_lossy(&add.stderr)
            )));
        }

        let commit = tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=drua",
                "-c",
                "user.email=drua@galoy.io",
                "commit",
                "-m",
                message,
                "--",
                relative_path,
            ])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git commit: {e}")))?;
        if !commit.status.success() {
            return Err(LibraryError::Git(format!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit.stderr)
            )));
        }

        tracing::info!(path = relative_path, "committed runtime file");
        Ok(())
    }

    pub async fn push(&self) -> Result<(), LibraryError> {
        if !self.repo_path.join(".git").exists() {
            return Ok(());
        }

        let push = tokio::process::Command::new("git")
            .args(["push"])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git push: {e}")))?;
        if !push.status.success() {
            return Err(LibraryError::Git(format!(
                "git push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            )));
        }

        tracing::info!("pushed runtime commits");
        Ok(())
    }
}

async fn clone(repo_url: &str, repo_path: &Path) -> Result<(), LibraryError> {
    tracing::info!(url = %repo_url, path = %repo_path.display(), "cloning library repo");
    let output = tokio::process::Command::new("git")
        .args(["clone", repo_url, &repo_path.to_string_lossy()])
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git clone: {e}")))?;
    if !output.status.success() {
        return Err(LibraryError::Git(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    tracing::info!(path = %repo_path.display(), "library repo cloned");
    Ok(())
}
