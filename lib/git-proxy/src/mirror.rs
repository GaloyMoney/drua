//! Bare mirrors at `<root>/<owner>/<repo>.git`.
//!
//! Pull traffic flows through these mirrors so the proxy can:
//!   * serve `git http-backend` from a stable on-disk repo (memo §2.2 Pattern A)
//!   * dedupe concurrent fetches per-mirror via in-process locks
//!   * apply per-mirror TTL gating so repeat clones in the same window
//!     don't hammer github.com
//!
//! Allow-list is global per project pivot — every project that
//! authenticates can address the same mirror set, so mirrors are
//! NOT namespaced per project. Push-side writes land in `git
//! http-backend` against the same paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, instrument};

use crate::primitives::RepoCoord;

/// `(github.com, ephemeral-token) -> credentials` callback. Implementations
/// mint fresh tokens via the existing `GitHubAppTokenProvider` plumbing —
/// the proxy never holds long-lived credentials.
#[async_trait::async_trait]
pub trait UpstreamCredentialProvider: Send + Sync {
    /// Returns a short-lived token usable as the password for HTTPS
    /// basic auth against github.com. Username convention: `x-access-token`.
    async fn fresh_token(&self) -> Result<String, MirrorError>;
}

#[derive(Debug, Error)]
pub enum MirrorError {
    #[error("MirrorError - Io: {0}")]
    Io(#[from] std::io::Error),
    #[error("MirrorError - Git: {0}")]
    Git(#[from] git2::Error),
    #[error("MirrorError - CredentialProvider: {0}")]
    CredentialProvider(String),
}

#[derive(Clone, Debug)]
pub struct MirrorConfig {
    pub root: PathBuf,
    /// Pulls within this window reuse the existing mirror without
    /// re-fetching from upstream. Defaults to 5 minutes (memo §7.1).
    pub fetch_ttl: Duration,
}

impl MirrorConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            fetch_ttl: Duration::from_secs(300),
        }
    }
}

/// Lock + last-fetched timestamp for one mirror. Held in a process-wide
/// map so concurrent requests for the same `(owner, repo)` serialize
/// on `fetch`.
#[derive(Default)]
struct MirrorState {
    last_fetched_at: Option<Instant>,
}

#[derive(Clone)]
pub struct MirrorManager {
    config: MirrorConfig,
    locks: Arc<Mutex<HashMap<MirrorKey, Arc<Mutex<MirrorState>>>>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct MirrorKey {
    owner: String,
    repo: String,
}

impl MirrorManager {
    pub fn new(config: MirrorConfig) -> Self {
        Self {
            config,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Returns the on-disk path for a mirror, opening / cloning + fetching
    /// from upstream if the mirror is missing or stale. Bare repo at
    /// `<root>/<owner>/<repo>.git`.
    ///
    /// Concurrent calls for the same mirror serialize on a per-mirror
    /// mutex so we don't kick off two fetches in parallel.
    #[instrument(name = "git_proxy.mirror.ensure", skip_all, fields(owner = %coord.owner, repo = %coord.repo))]
    pub async fn ensure(
        &self,
        coord: &RepoCoord,
        upstream_url: &str,
        creds: &dyn UpstreamCredentialProvider,
    ) -> Result<PathBuf, MirrorError> {
        let key = MirrorKey {
            owner: coord.owner.clone(),
            repo: coord.repo.clone(),
        };
        let mirror_lock = {
            let mut locks = self.locks.lock().await;
            locks.entry(key).or_default().clone()
        };
        let mut state = mirror_lock.lock().await;

        let path = self.mirror_path(coord);
        let exists = path.join("HEAD").exists();
        let stale = state
            .last_fetched_at
            .map(|t| t.elapsed() > self.config.fetch_ttl)
            .unwrap_or(true);

        if !exists {
            tokio::fs::create_dir_all(path.parent().expect("mirror_path has parent")).await?;
            self.clone_bare(&path, upstream_url, creds).await?;
            state.last_fetched_at = Some(Instant::now());
        } else if stale {
            if let Err(e) = self.fetch_into(&path, upstream_url, creds).await {
                tracing::warn!(error = %e, "upstream fetch failed; serving existing mirror");
            } else {
                state.last_fetched_at = Some(Instant::now());
            }
        } else {
            debug!("mirror within TTL; reusing");
        }

        Ok(path)
    }

    pub fn mirror_path(&self, coord: &RepoCoord) -> PathBuf {
        self.config
            .root
            .join(&coord.owner)
            .join(format!("{}.git", coord.repo))
    }

    async fn clone_bare(
        &self,
        path: &Path,
        upstream_url: &str,
        creds: &dyn UpstreamCredentialProvider,
    ) -> Result<(), MirrorError> {
        let token = creds.fresh_token().await?;
        let url = upstream_url.to_string();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<(), MirrorError> {
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.credentials(move |_url, _username, _allowed| {
                git2::Cred::userpass_plaintext("x-access-token", &token)
            });
            let mut fetch_opts = git2::FetchOptions::new();
            fetch_opts.remote_callbacks(callbacks);
            let mut builder = git2::build::RepoBuilder::new();
            builder.bare(true).fetch_options(fetch_opts);
            builder.clone(&url, &path)?;
            Ok(())
        })
        .await
        .map_err(|e| MirrorError::CredentialProvider(format!("join: {e}")))?
    }

    async fn fetch_into(
        &self,
        path: &Path,
        upstream_url: &str,
        creds: &dyn UpstreamCredentialProvider,
    ) -> Result<(), MirrorError> {
        let token = creds.fresh_token().await?;
        let url = upstream_url.to_string();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<(), MirrorError> {
            let repo = git2::Repository::open_bare(&path)?;
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.credentials(move |_url, _username, _allowed| {
                git2::Cred::userpass_plaintext("x-access-token", &token)
            });
            let mut fetch_opts = git2::FetchOptions::new();
            fetch_opts.remote_callbacks(callbacks);
            let mut remote = repo
                .find_remote("origin")
                .or_else(|_| repo.remote("origin", &url))?;
            // Mirror-fetch refspec: pull every ref under refs/* into the
            // matching local ref, overwriting non-ff updates. Same shape
            // `git clone --mirror` configures.
            remote.fetch(&["+refs/*:refs/*"], Some(&mut fetch_opts), None)?;
            Ok(())
        })
        .await
        .map_err(|e| MirrorError::CredentialProvider(format!("join: {e}")))?
    }
}

/// Test-only credential provider that hands back a static string. Used
/// in unit tests + bats fixtures where the "upstream" is a local file:// URL.
pub struct StaticCredential(pub String);

#[async_trait::async_trait]
impl UpstreamCredentialProvider for StaticCredential {
    async fn fresh_token(&self) -> Result<String, MirrorError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mirror_path_is_owner_repo() {
        let tmp = TempDir::new().unwrap();
        let mgr = MirrorManager::new(MirrorConfig::new(tmp.path()));
        let coord = RepoCoord {
            owner: "GaloyMoney".into(),
            repo: "drua".into(),
        };
        assert!(mgr.mirror_path(&coord).ends_with("GaloyMoney/drua.git"));
    }
}
