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

    /// Atomically apply a single-file edit and push to origin.
    ///
    /// `update` receives `(input_path, current_content)` (content is `None` if
    /// the file doesn't exist at HEAD) and returns `(rename_to, output_content)`.
    /// `rename_to = None` keeps the same path; `Some(new)` is an atomic move.
    /// `output_content = None` deletes the file.
    ///
    /// On non-fast-forward push, fetches origin, resets local main to the new
    /// remote, re-invokes `update` against the new HEAD, and pushes once more.
    /// A second failure aborts.
    #[tracing::instrument(name = "library.git.update_file", skip_all, fields(%path))]
    pub async fn update_file<F>(
        &self,
        path: String,
        update: F,
        commit_message: String,
    ) -> Result<(), LibraryError>
    where
        F: Fn(&str, Option<&[u8]>) -> (Option<String>, Option<Vec<u8>>) + Send + 'static,
    {
        let _guard = self.write_lock.lock().await;
        let token = Self::fresh_token(self.github_app.as_ref()).await;
        let repo_path = self.repo_path.clone();

        tokio::task::spawn_blocking(move || -> Result<(), LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;

            const MAX_ATTEMPTS: u32 = 2;
            let mut attempt = 0;
            loop {
                attempt += 1;
                match Self::try_update_once(
                    &repo,
                    &path,
                    &update,
                    &commit_message,
                    token.as_deref(),
                ) {
                    Ok(()) => return Ok(()),
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::info!(
                            error = %e,
                            attempt,
                            "update_file failed, refetching and retrying",
                        );
                        Self::fetch_origin(&repo, token.as_deref())?;
                        Self::reset_main_to_origin(&repo)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|e| LibraryError::Git(format!("update_file join: {e}")))?
    }

    fn try_update_once<F>(
        repo: &git2::Repository,
        path: &str,
        update: &F,
        commit_message: &str,
        token: Option<&str>,
    ) -> Result<(), LibraryError>
    where
        F: Fn(&str, Option<&[u8]>) -> (Option<String>, Option<Vec<u8>>),
    {
        let head_commit = repo
            .head()
            .map_err(|e| LibraryError::Git(format!("head: {e}")))?
            .peel_to_commit()
            .map_err(|e| LibraryError::Git(format!("peel head: {e}")))?;
        let head_tree = head_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("head tree: {e}")))?;

        let current_content = Self::read_blob_at(repo, &head_tree, path)?;
        let (rename_to, new_content) = update(path, current_content.as_deref());
        let new_path: &str = rename_to.as_deref().unwrap_or(path);

        // Step 1: remove old path if it's a move
        let intermediate_oid = if new_path == path {
            head_tree.id()
        } else {
            Self::apply_edit(repo, &head_tree, path, None)?
        };
        let intermediate_tree = repo
            .find_tree(intermediate_oid)
            .map_err(|e| LibraryError::Git(format!("find intermediate: {e}")))?;

        // Step 2: insert/delete at new path
        let final_oid = Self::apply_edit(repo, &intermediate_tree, new_path, new_content)?;
        if final_oid == head_tree.id() {
            tracing::debug!("update_file: tree unchanged, skipping commit");
            return Ok(());
        }

        let final_tree = repo
            .find_tree(final_oid)
            .map_err(|e| LibraryError::Git(format!("find final: {e}")))?;
        let sig = git2::Signature::now("drua-library", "drua-library@local")
            .map_err(|e| LibraryError::Git(format!("signature: {e}")))?;
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            commit_message,
            &final_tree,
            &[&head_commit],
        )
        .map_err(|e| LibraryError::Git(format!("commit: {e}")))?;

        Self::push_main(repo, token)
    }

    fn read_blob_at(
        repo: &git2::Repository,
        tree: &git2::Tree,
        path: &str,
    ) -> Result<Option<Vec<u8>>, LibraryError> {
        match tree.get_path(Path::new(path)) {
            Ok(entry) => {
                let blob = repo
                    .find_blob(entry.id())
                    .map_err(|e| LibraryError::Git(format!("find blob: {e}")))?;
                Ok(Some(blob.content().to_vec()))
            }
            Err(_) => Ok(None),
        }
    }

    fn apply_edit(
        repo: &git2::Repository,
        tree: &git2::Tree,
        path: &str,
        content: Option<Vec<u8>>,
    ) -> Result<git2::Oid, LibraryError> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Err(LibraryError::Git(format!("invalid empty path: {path}")));
        }
        Self::apply_edit_recursive(repo, Some(tree), &segments, content)
    }

    fn apply_edit_recursive(
        repo: &git2::Repository,
        dir_tree: Option<&git2::Tree>,
        segments: &[&str],
        content: Option<Vec<u8>>,
    ) -> Result<git2::Oid, LibraryError> {
        const MODE_BLOB: i32 = 0o100644;
        const MODE_TREE: i32 = 0o040000;

        let mut tb = repo
            .treebuilder(dir_tree)
            .map_err(|e| LibraryError::Git(format!("treebuilder: {e}")))?;

        if segments.len() == 1 {
            let name = segments[0];
            match content {
                Some(bytes) => {
                    let blob = repo
                        .blob(&bytes)
                        .map_err(|e| LibraryError::Git(format!("blob: {e}")))?;
                    tb.insert(name, blob, MODE_BLOB)
                        .map_err(|e| LibraryError::Git(format!("insert blob: {e}")))?;
                }
                None => {
                    if dir_tree.and_then(|t| t.get_name(name)).is_some() {
                        tb.remove(name)
                            .map_err(|e| LibraryError::Git(format!("remove blob: {e}")))?;
                    }
                }
            }
        } else {
            let head = segments[0];
            let rest = &segments[1..];

            let sub_tree = dir_tree
                .and_then(|t| t.get_name(head))
                .filter(|e| e.kind() == Some(git2::ObjectType::Tree))
                .and_then(|e| repo.find_tree(e.id()).ok());

            let new_sub_oid = Self::apply_edit_recursive(repo, sub_tree.as_ref(), rest, content)?;
            let new_sub = repo
                .find_tree(new_sub_oid)
                .map_err(|e| LibraryError::Git(format!("find sub: {e}")))?;

            if new_sub.iter().count() == 0 {
                if dir_tree.and_then(|t| t.get_name(head)).is_some() {
                    tb.remove(head)
                        .map_err(|e| LibraryError::Git(format!("remove dir: {e}")))?;
                }
            } else {
                tb.insert(head, new_sub_oid, MODE_TREE)
                    .map_err(|e| LibraryError::Git(format!("insert dir: {e}")))?;
            }
        }

        tb.write()
            .map_err(|e| LibraryError::Git(format!("tree write: {e}")))
    }

    fn push_main(repo: &git2::Repository, token: Option<&str>) -> Result<(), LibraryError> {
        let mut remote = repo
            .find_remote("origin")
            .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;
        let mut po = git2::PushOptions::new();
        po.remote_callbacks(Self::remote_callbacks(token));
        remote
            .push(&["refs/heads/main:refs/heads/main"], Some(&mut po))
            .map_err(|e| LibraryError::Git(format!("push: {e}")))
    }

    fn reset_main_to_origin(repo: &git2::Repository) -> Result<(), LibraryError> {
        let origin_main = repo
            .find_reference("refs/remotes/origin/main")
            .map_err(|e| LibraryError::Git(format!("find origin/main: {e}")))?;
        let oid = origin_main
            .target()
            .ok_or_else(|| LibraryError::Git("origin/main has no target".into()))?;
        repo.reference("refs/heads/main", oid, true, "reset to origin/main")
            .map_err(|e| LibraryError::Git(format!("update refs/heads/main: {e}")))?;
        Ok(())
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

        // Mirror refspec — write directly to local heads so HEAD advances
        // with origin (the default bare-clone refspec only updates the
        // remote-tracking refs).
        remote
            .fetch(&["+refs/heads/*:refs/heads/*"], Some(&mut fo), None)
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
