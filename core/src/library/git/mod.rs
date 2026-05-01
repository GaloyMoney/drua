use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::github_app::GitHubAppTokenProvider;

use super::LibraryError;

mod actor;
mod tree;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOid(pub git2::Oid);

impl CommitOid {
    pub fn to_hex(self) -> String {
        self.0.to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self, git2::Error> {
        git2::Oid::from_str(s).map(Self)
    }
}

#[derive(Debug)]
pub struct TreeDiff {
    pub from: Option<CommitOid>,
    pub to: CommitOid,
    pub deltas: Vec<Delta>,
}

#[derive(Debug)]
pub enum Delta {
    Upserted { path: String },
    Deleted { path: String },
}

impl Delta {
    pub fn path(&self) -> &str {
        match self {
            Delta::Upserted { path } | Delta::Deleted { path } => path,
        }
    }
}

/// Drop signals shutdown by closing the command channel; the blocking
/// actor task can't be aborted, but exits when its receiver returns None.
struct ActorHandle(Option<JoinHandle<()>>);

impl Drop for ActorHandle {
    fn drop(&mut self) {
        // spawn_blocking JoinHandles can't be aborted; channel-close drives
        // shutdown. Keep the handle owned so it doesn't leak the future,
        // then detach.
        let _ = self.0.take();
    }
}

pub struct GitEngine {
    cmds: mpsc::Sender<actor::UpstreamCmd>,
    _handle: ActorHandle,
    repo_path: PathBuf,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
}

impl GitEngine {
    #[tracing::instrument(name = "library.git.init", skip_all)]
    pub async fn init(
        repo_url: Option<&str>,
        repo_path: PathBuf,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        let token = fresh_token(&github_app).await;
        let url = repo_url.map(String::from);
        let path_for_init = repo_path.clone();
        let repo = actor::ensure_repo(path_for_init, url, token).await?;

        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::task::spawn_blocking(move || actor::run(repo, rx));

        Ok(Self {
            cmds: tx,
            _handle: ActorHandle(Some(handle)),
            repo_path,
            github_app,
        })
    }

    #[tracing::instrument(name = "library.git.fetch", skip_all)]
    pub async fn fetch(&self) -> Result<(), LibraryError> {
        let token = fresh_token(&self.github_app).await;
        let (reply, rx) = oneshot::channel();
        self.send(actor::UpstreamCmd::Fetch { token, reply })
            .await?;
        recv(rx).await
    }

    #[tracing::instrument(name = "library.git.head", skip_all)]
    pub async fn head(&self) -> Result<Option<CommitOid>, LibraryError> {
        let (reply, rx) = oneshot::channel();
        self.send(actor::UpstreamCmd::Head { reply }).await?;
        recv(rx).await
    }

    #[tracing::instrument(name = "library.git.tree_diff", skip_all)]
    pub async fn tree_diff(
        &self,
        from: Option<CommitOid>,
        to: CommitOid,
    ) -> Result<TreeDiff, LibraryError> {
        let (reply, rx) = oneshot::channel();
        self.send(actor::UpstreamCmd::TreeDiff {
            from: from.map(|c| c.0),
            to: to.0,
            reply,
        })
        .await?;
        recv(rx).await
    }

    #[tracing::instrument(name = "library.git.read_blob_at", skip_all)]
    pub async fn read_blob_at(
        &self,
        commit: CommitOid,
        path: &str,
    ) -> Result<Option<Vec<u8>>, LibraryError> {
        let (reply, rx) = oneshot::channel();
        self.send(actor::UpstreamCmd::ReadBlob {
            commit: commit.0,
            path: path.to_string(),
            reply,
        })
        .await?;
        recv(rx).await
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    async fn send(&self, cmd: actor::UpstreamCmd) -> Result<(), LibraryError> {
        self.cmds
            .send(cmd)
            .await
            .map_err(|_| LibraryError::Git("git engine actor closed".into()))
    }
}

async fn recv<T>(rx: oneshot::Receiver<Result<T, LibraryError>>) -> Result<T, LibraryError> {
    rx.await
        .map_err(|_| LibraryError::Git("git engine reply lost".into()))?
}

async fn fresh_token(github_app: &Option<Arc<GitHubAppTokenProvider>>) -> Option<String> {
    match github_app.as_ref() {
        Some(provider) => match provider.generate_token().await {
            Ok(t) => Some(t.token),
            Err(e) => {
                tracing::warn!(error = %e, "failed to generate GitHub App token for git engine");
                None
            }
        },
        None => None,
    }
}
