use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha1::{Digest, Sha1};

use crate::github_app::GitHubAppTokenProvider;

use super::file::GitFileHash;
use super::LibraryError;

/// Build a `git` command that never prompts for credentials.
/// When a token is provided, injects it via a one-shot credential helper.
fn git_cmd(token: Option<&str>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = token {
        cmd.args([
            "-c",
            &format!(
                "credential.helper=!f() {{ echo username=x-access-token; echo password={token}; }}; f"
            ),
        ]);
    }
    cmd
}

#[derive(Clone)]
pub(super) struct Upstream {
    repo_path: PathBuf,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
}

impl Upstream {
    pub async fn init(
        repo_url: Option<&str>,
        repo_path: PathBuf,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        if let Some(url) = repo_url {
            if !repo_path.join(".git").exists() {
                // Clean up any leftover files from a previous failed clone
                // so `git clone` doesn't fail with "directory not empty".
                if repo_path.exists() {
                    if let Err(e) = tokio::fs::remove_dir_all(&repo_path).await {
                        tracing::warn!(error = %e, "failed to clean stale library dir");
                    }
                }
                let token = Self::fresh_token(&github_app).await;
                if let Err(e) = clone(url, &repo_path, token.as_deref()).await {
                    tracing::warn!(error = %e, url, "library git clone failed — upstream sync disabled");
                }
            }
        }
        Ok(Self {
            repo_path,
            github_app,
        })
    }

    /// Generate a fresh GitHub App installation token, or `None` if no app is configured.
    async fn fresh_token(github_app: &Option<Arc<GitHubAppTokenProvider>>) -> Option<String> {
        match github_app.as_ref() {
            Some(provider) => match provider.generate_token().await {
                Ok(t) => Some(t.token),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to generate GitHub App token for library git ops");
                    None
                }
            },
            None => None,
        }
    }

    fn require_git_repo(&self) -> Result<(), LibraryError> {
        if !self.repo_path.join(".git").exists() {
            return Err(LibraryError::Git(format!(
                "no .git directory at {} — clone may have failed on startup",
                self.repo_path.display()
            )));
        }
        Ok(())
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
        self.require_git_repo()?;

        let token = Self::fresh_token(&self.github_app).await;
        let pull = git_cmd(token.as_deref())
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
        self.require_git_repo()?;

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

    /// Return the HEAD commit hash, or `None` if the repo has no commits.
    pub async fn head_commit_hash(&self) -> Result<Option<String>, LibraryError> {
        self.require_git_repo()?;

        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git rev-parse HEAD: {e}")))?;

        if !output.status.success() {
            // No commits yet — HEAD doesn't exist.
            return Ok(None);
        }

        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            return Ok(None);
        }
        Ok(Some(hash))
    }

    /// Find skill files that changed since `last_commit`.
    ///
    /// Returns `(relative_path, content)` tuples for each changed `.md` file
    /// under the skill directories.
    ///
    /// When `last_commit` is `None` (first sync), lists all tracked skill
    /// files at HEAD.
    pub async fn changed_skill_files(
        &self,
        last_commit: Option<&str>,
    ) -> Result<Vec<(String, String)>, LibraryError> {
        self.require_git_repo()?;

        let paths = match last_commit {
            Some(commit) => {
                let output = tokio::process::Command::new("git")
                    .args([
                        "diff",
                        "--name-only",
                        &format!("{commit}..HEAD"),
                        "--",
                        "runtime/skills/",
                        "runtime/workspaces/",
                    ])
                    .current_dir(&self.repo_path)
                    .output()
                    .await
                    .map_err(|e| LibraryError::Git(format!("git diff: {e}")))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(LibraryError::Git(format!("git diff failed: {stderr}")));
                }

                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|p| p.ends_with(".md") && p.contains("/skills/"))
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            }
            None => {
                // First run: list all tracked skill files.
                let output = tokio::process::Command::new("git")
                    .args([
                        "ls-files",
                        "--",
                        "runtime/skills/*.md",
                        "runtime/workspaces/*/skills/*.md",
                    ])
                    .current_dir(&self.repo_path)
                    .output()
                    .await
                    .map_err(|e| LibraryError::Git(format!("git ls-files: {e}")))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(LibraryError::Git(format!("git ls-files failed: {stderr}")));
                }

                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|p| !p.is_empty())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            }
        };

        let mut results = Vec::with_capacity(paths.len());
        for path in paths {
            let full_path = self.repo_path.join(&path);
            match tokio::fs::read_to_string(&full_path).await {
                Ok(content) => results.push((path, content)),
                Err(e) => {
                    // File may have been deleted in a later commit.
                    tracing::debug!(path = %path, error = %e, "skipping unreadable skill file");
                }
            }
        }
        Ok(results)
    }

    pub async fn push(&self) -> Result<(), LibraryError> {
        self.require_git_repo()?;

        let token = Self::fresh_token(&self.github_app).await;
        let push = git_cmd(token.as_deref())
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

async fn clone(repo_url: &str, repo_path: &Path, token: Option<&str>) -> Result<(), LibraryError> {
    tracing::info!(url = %repo_url, path = %repo_path.display(), "cloning library repo");
    let output = git_cmd(token)
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
