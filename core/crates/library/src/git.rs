use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{GitHubAppTokenProvider, LibraryError};

pub struct GitEngine {
    repo_path: PathBuf,
    write_lock: tokio::sync::Mutex<()>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
}

impl GitEngine {
    #[tracing::instrument(name = "library.git.init", skip_all)]
    pub async fn init(
        repo_url: &str,
        repo_path: PathBuf,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        if repo_url.is_empty() {
            return Err(LibraryError::Config("repo_url is empty".into()));
        }

        let token = Self::fresh_token(github_app.as_ref()).await;
        let path = repo_path.clone();
        let url = repo_url.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), LibraryError> {
            Self::open_or_clone_bare(&url, &path, token.as_deref()).map(|_| ())
        })
        .await
        .map_err(|e| LibraryError::Git(format!("init join: {e}")))??;

        Ok(Self {
            repo_path,
            write_lock: tokio::sync::Mutex::new(()),
            github_app,
        })
    }

    #[tracing::instrument(name = "library.git.fetch_and_head", skip_all)]
    pub async fn fetch_and_head(&self) -> Result<Option<String>, LibraryError> {
        let _guard = self.write_lock.lock().await;
        let token = Self::fresh_token(self.github_app.as_ref()).await;
        let path = self.repo_path.clone();

        tokio::task::spawn_blocking(move || -> Result<Option<String>, LibraryError> {
            let repo = git2::Repository::open_bare(&path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            Self::fetch_origin(&repo, token.as_deref())?;
            let head = match repo.head() {
                Ok(r) => r.target().map(|oid| oid.to_string()),
                Err(_) => None,
            };
            Ok(head)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("fetch_and_head join: {e}")))?
    }

    async fn fresh_token(provider: Option<&Arc<GitHubAppTokenProvider>>) -> Option<String> {
        let provider = provider?;
        match provider.generate_token().await {
            Ok(t) => Some(t.token),
            Err(e) => {
                tracing::warn!(error = %e, "failed to generate GitHub App token for git engine");
                None
            }
        }
    }

    fn open_or_clone_bare(
        url: &str,
        path: &Path,
        token: Option<&str>,
    ) -> Result<git2::Repository, LibraryError> {
        if let Ok(repo) = git2::Repository::open_bare(path) {
            Self::fetch_origin(&repo, token)?;
            return Ok(repo);
        }

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| LibraryError::Io(format!("create_dir_all: {e}")))?;
            }
        }
        if path.exists() {
            std::fs::remove_dir_all(path)
                .map_err(|e| LibraryError::Io(format!("remove stale dir: {e}")))?;
        }

        let mut fo = git2::FetchOptions::new();
        fo.remote_callbacks(Self::remote_callbacks(token));

        git2::build::RepoBuilder::new()
            .bare(true)
            .fetch_options(fo)
            .clone(url, path)
            .map_err(|e| LibraryError::Git(format!("clone bare: {e}")))
    }

    fn fetch_origin(repo: &git2::Repository, token: Option<&str>) -> Result<(), LibraryError> {
        let mut remote = repo
            .find_remote("origin")
            .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;

        let mut fo = git2::FetchOptions::new();
        fo.remote_callbacks(Self::remote_callbacks(token));

        remote
            .fetch::<&str>(&[], Some(&mut fo), None)
            .map_err(|e| LibraryError::Git(format!("fetch: {e}")))
    }

    fn remote_callbacks(token: Option<&str>) -> git2::RemoteCallbacks<'static> {
        let mut cb = git2::RemoteCallbacks::new();
        if let Some(t) = token {
            let t = t.to_string();
            cb.credentials(move |_url, _username, _allowed| {
                git2::Cred::userpass_plaintext("x-access-token", &t)
            });
        }
        cb
    }
}
