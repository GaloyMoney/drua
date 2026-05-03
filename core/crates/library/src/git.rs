use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::importer::GitFileHash;
use crate::{GitHubAppTokenProvider, LibraryError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Added,
    Modified,
    Deleted,
}

/// One immediate child of a tree at HEAD. Returned by `list_dir_at_head`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct CommitDelta {
    pub path: String,
    pub kind: DeltaKind,
    pub file_hash: GitFileHash,
    /// Empty for `Deleted`.
    pub content: Vec<u8>,
}

pub struct GitEngine {
    repo_path: PathBuf,
    write_lock: tokio::sync::Mutex<()>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
}

impl GitEngine {
    /// Working-tree root. `<repo_path>/spaces/<slug>/...` is where
    /// SpaceFs reads land.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

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
            Self::open_or_clone(&url, &path, token.as_deref()).map(|_| ())
        })
        .await
        .map_err(|e| LibraryError::Git(format!("init join: {e}")))??;

        Ok(Self {
            repo_path,
            write_lock: tokio::sync::Mutex::new(()),
            github_app,
        })
    }

    /// Diff between two commits (None `from` = walk all of `to`'s tree as Added)
    /// returning each changed path with its blob content at `to` (or empty for
    /// deletes).
    #[tracing::instrument(name = "library.git.changes_since", skip_all)]
    pub async fn changes_since(
        &self,
        from: Option<&str>,
        to: &str,
    ) -> Result<Vec<CommitDelta>, LibraryError> {
        let path = self.repo_path.clone();
        let from = from.map(String::from);
        let to = to.to_string();

        tokio::task::spawn_blocking(move || -> Result<Vec<CommitDelta>, LibraryError> {
            let repo = git2::Repository::open_bare(&path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;

            let to_oid = git2::Oid::from_str(&to)
                .map_err(|e| LibraryError::Git(format!("parse to oid: {e}")))?;
            let to_tree = repo
                .find_commit(to_oid)
                .and_then(|c| c.tree())
                .map_err(|e| LibraryError::Git(format!("to tree: {e}")))?;

            let from_tree = match from.as_deref() {
                Some(s) => {
                    let oid = git2::Oid::from_str(s)
                        .map_err(|e| LibraryError::Git(format!("parse from oid: {e}")))?;
                    Some(
                        repo.find_commit(oid)
                            .and_then(|c| c.tree())
                            .map_err(|e| LibraryError::Git(format!("from tree: {e}")))?,
                    )
                }
                None => None,
            };

            let diff = repo
                .diff_tree_to_tree(from_tree.as_ref(), Some(&to_tree), None)
                .map_err(|e| LibraryError::Git(format!("diff: {e}")))?;

            let mut deltas: Vec<CommitDelta> = Vec::new();
            for delta in diff.deltas() {
                let kind = match delta.status() {
                    git2::Delta::Added => DeltaKind::Added,
                    git2::Delta::Modified => DeltaKind::Modified,
                    git2::Delta::Deleted => DeltaKind::Deleted,
                    _ => continue,
                };
                let entry = match kind {
                    DeltaKind::Deleted => delta.old_file(),
                    _ => delta.new_file(),
                };
                let path_str = match entry.path() {
                    Some(p) => p.to_string_lossy().into_owned(),
                    None => continue,
                };
                let oid = entry.id();
                let content = match kind {
                    DeltaKind::Deleted => Vec::new(),
                    _ => match repo.find_blob(oid) {
                        Ok(b) => b.content().to_vec(),
                        Err(_) => continue,
                    },
                };
                deltas.push(CommitDelta {
                    path: path_str,
                    kind,
                    file_hash: GitFileHash::new(oid.to_string()),
                    content,
                });
            }
            Ok(deltas)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("changes_since join: {e}")))?
    }

    /// Read the blob at `path` from HEAD's tree. `Ok(None)` when the
    /// path doesn't exist (or HEAD is unborn).
    #[tracing::instrument(name = "library.git.read_blob_at_head", skip_all, fields(%path))]
    pub async fn read_blob_at_head(&self, path: &str) -> Result<Option<Vec<u8>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let Ok(head) = repo.head() else {
                return Ok(None);
            };
            let tree = head
                .peel_to_commit()
                .and_then(|c| c.tree())
                .map_err(|e| LibraryError::Git(format!("peel head tree: {e}")))?;
            Self::read_blob_at(&repo, &tree, &path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("read_blob_at_head join: {e}")))?
    }

    /// Lists immediate children of `dir_path` at HEAD's tree.
    /// `Ok(None)` when the directory doesn't exist (or HEAD is unborn).
    /// Empty `dir_path` lists the repo root.
    #[tracing::instrument(name = "library.git.list_dir_at_head", skip_all, fields(%dir_path))]
    pub async fn list_dir_at_head(
        &self,
        dir_path: &str,
    ) -> Result<Option<Vec<DirEntry>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let dir_path = dir_path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<DirEntry>>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let Ok(head) = repo.head() else {
                return Ok(None);
            };
            let root = head
                .peel_to_commit()
                .and_then(|c| c.tree())
                .map_err(|e| LibraryError::Git(format!("peel head tree: {e}")))?;
            let tree = if dir_path.is_empty() {
                root
            } else {
                let entry = match root.get_path(Path::new(&dir_path)) {
                    Ok(e) => e,
                    Err(_) => return Ok(None),
                };
                if entry.kind() != Some(git2::ObjectType::Tree) {
                    return Ok(None);
                }
                repo.find_tree(entry.id())
                    .map_err(|e| LibraryError::Git(format!("find subtree: {e}")))?
            };
            let mut out = Vec::with_capacity(tree.iter().count());
            for entry in tree.iter() {
                let Some(name) = entry.name() else { continue };
                out.push(DirEntry {
                    name: name.to_string(),
                    is_dir: entry.kind() == Some(git2::ObjectType::Tree),
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Some(out))
        })
        .await
        .map_err(|e| LibraryError::Git(format!("list_dir_at_head join: {e}")))?
    }

    /// Recursively walk every blob under `dir_path` at HEAD's tree.
    /// Returns `(absolute_path, content)` pairs (paths relative to the
    /// repo root). Empty `dir_path` walks the entire tree.
    #[tracing::instrument(name = "library.git.walk_blobs_at_head", skip_all, fields(%dir_path))]
    pub async fn walk_blobs_at_head(
        &self,
        dir_path: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let dir_path = dir_path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<(String, Vec<u8>)>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let Ok(head) = repo.head() else {
                return Ok(Vec::new());
            };
            let root = head
                .peel_to_commit()
                .and_then(|c| c.tree())
                .map_err(|e| LibraryError::Git(format!("peel head tree: {e}")))?;
            let (subtree, prefix) = if dir_path.is_empty() {
                (root, String::new())
            } else {
                let entry = match root.get_path(Path::new(&dir_path)) {
                    Ok(e) => e,
                    Err(_) => return Ok(Vec::new()),
                };
                if entry.kind() != Some(git2::ObjectType::Tree) {
                    return Ok(Vec::new());
                }
                let t = repo
                    .find_tree(entry.id())
                    .map_err(|e| LibraryError::Git(format!("find subtree: {e}")))?;
                (t, format!("{dir_path}/"))
            };
            let mut out = Vec::new();
            subtree
                .walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                    if entry.kind() != Some(git2::ObjectType::Blob) {
                        return git2::TreeWalkResult::Ok;
                    }
                    let Some(name) = entry.name() else {
                        return git2::TreeWalkResult::Ok;
                    };
                    let rel = format!("{prefix}{dir}{name}");
                    if let Ok(blob) = repo.find_blob(entry.id()) {
                        out.push((rel, blob.content().to_vec()));
                    }
                    git2::TreeWalkResult::Ok
                })
                .map_err(|e| LibraryError::Git(format!("tree walk: {e}")))?;
            Ok(out)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("walk_blobs_at_head join: {e}")))?
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
    /// `output_content = None` deletes the file. The closure may return
    /// `Err(LibraryError::Validation(_))` to abort with a typed error
    /// (e.g. `str_replace` sentinel-not-unique).
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
        F: Fn(&str, Option<&[u8]>) -> Result<(Option<String>, Option<Vec<u8>>), LibraryError>
            + Send
            + 'static,
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
                    // Validation errors are deterministic (closure-side), so
                    // refetch+retry would just produce the same failure.
                    Err(e @ LibraryError::Validation(_)) => return Err(e),
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
        F: Fn(&str, Option<&[u8]>) -> Result<(Option<String>, Option<Vec<u8>>), LibraryError>,
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
        let (rename_to, new_content) = update(path, current_content.as_deref())?;
        let new_path: &str = rename_to.as_deref().unwrap_or(path);

        let intermediate_oid = if new_path == path {
            head_tree.id()
        } else {
            Self::apply_edit(repo, &head_tree, path, None)?
        };
        let intermediate_tree = repo
            .find_tree(intermediate_oid)
            .map_err(|e| LibraryError::Git(format!("find intermediate: {e}")))?;

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

    /// Apply multiple file changes in a single commit + push.
    /// `changes` is a list of `(path, content_opt)` — `None` deletes
    /// (no-op if absent). No-op overall if the resulting tree matches
    /// HEAD. Same retry-on-non-ff semantics as `update_file`.
    #[tracing::instrument(name = "library.git.commit_changes", skip_all, fields(count = changes.len()))]
    pub async fn commit_changes(
        &self,
        changes: Vec<(String, Option<Vec<u8>>)>,
        commit_message: String,
    ) -> Result<(), LibraryError> {
        if changes.is_empty() {
            return Ok(());
        }
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
                match Self::try_commit_changes_once(&repo, &changes, &commit_message, token.as_deref()) {
                    Ok(()) => return Ok(()),
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::info!(error = %e, attempt, "commit_changes failed, refetching and retrying");
                        Self::fetch_origin(&repo, token.as_deref())?;
                        Self::reset_main_to_origin(&repo)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|e| LibraryError::Git(format!("commit_changes join: {e}")))?
    }

    fn try_commit_changes_once(
        repo: &git2::Repository,
        changes: &[(String, Option<Vec<u8>>)],
        commit_message: &str,
        token: Option<&str>,
    ) -> Result<(), LibraryError> {
        let head_commit = repo
            .head()
            .map_err(|e| LibraryError::Git(format!("head: {e}")))?
            .peel_to_commit()
            .map_err(|e| LibraryError::Git(format!("peel head: {e}")))?;
        let head_tree = head_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("head tree: {e}")))?;

        let mut current_oid = head_tree.id();
        for (path, content) in changes {
            let current_tree = repo
                .find_tree(current_oid)
                .map_err(|e| LibraryError::Git(format!("find tree: {e}")))?;
            current_oid = Self::apply_edit(repo, &current_tree, path, content.clone())?;
        }

        if current_oid == head_tree.id() {
            tracing::debug!("commit_changes: tree unchanged, skipping commit");
            return Ok(());
        }

        let final_tree = repo
            .find_tree(current_oid)
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

    /// Atomically move `from` → `to` and push. Errors with
    /// `LibraryError::Validation` if `from` is missing or `to` already
    /// exists at HEAD. Same retry-on-non-ff semantics as `update_file`.
    #[tracing::instrument(name = "library.git.move_file", skip_all, fields(%from, %to))]
    pub async fn move_file(
        &self,
        from: String,
        to: String,
        commit_message: String,
    ) -> Result<(), LibraryError> {
        if from == to {
            return Err(LibraryError::Validation(format!(
                "move: src and dest are the same: {from}"
            )));
        }
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
                match Self::try_move_once(&repo, &from, &to, &commit_message, token.as_deref()) {
                    Ok(()) => return Ok(()),
                    Err(e @ LibraryError::Validation(_)) => return Err(e),
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::info!(error = %e, attempt, "move_file failed, refetching and retrying");
                        Self::fetch_origin(&repo, token.as_deref())?;
                        Self::reset_main_to_origin(&repo)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|e| LibraryError::Git(format!("move_file join: {e}")))?
    }

    fn try_move_once(
        repo: &git2::Repository,
        from: &str,
        to: &str,
        commit_message: &str,
        token: Option<&str>,
    ) -> Result<(), LibraryError> {
        let head_commit = repo
            .head()
            .map_err(|e| LibraryError::Git(format!("head: {e}")))?
            .peel_to_commit()
            .map_err(|e| LibraryError::Git(format!("peel head: {e}")))?;
        let head_tree = head_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("head tree: {e}")))?;

        let from_content = Self::read_blob_at(repo, &head_tree, from)?
            .ok_or_else(|| LibraryError::Validation(format!("move: src does not exist: {from}")))?;
        if Self::read_blob_at(repo, &head_tree, to)?.is_some() {
            return Err(LibraryError::Validation(format!(
                "move: dest already exists: {to}"
            )));
        }

        let intermediate_oid = Self::apply_edit(repo, &head_tree, from, None)?;
        let intermediate_tree = repo
            .find_tree(intermediate_oid)
            .map_err(|e| LibraryError::Git(format!("find intermediate: {e}")))?;
        let final_oid = Self::apply_edit(repo, &intermediate_tree, to, Some(from_content))?;

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

    /// Recursively remove every blob under `dir_path` and push.
    /// No-op if the directory doesn't exist at HEAD. Same retry-on-non-ff
    /// semantics as `update_file`.
    #[tracing::instrument(name = "library.git.delete_dir", skip_all, fields(%dir_path))]
    pub async fn delete_dir(
        &self,
        dir_path: String,
        commit_message: String,
    ) -> Result<(), LibraryError> {
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
                match Self::try_delete_dir_once(&repo, &dir_path, &commit_message, token.as_deref())
                {
                    Ok(()) => return Ok(()),
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::info!(error = %e, attempt, "delete_dir failed, refetching and retrying");
                        Self::fetch_origin(&repo, token.as_deref())?;
                        Self::reset_main_to_origin(&repo)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|e| LibraryError::Git(format!("delete_dir join: {e}")))?
    }

    fn try_delete_dir_once(
        repo: &git2::Repository,
        dir_path: &str,
        commit_message: &str,
        token: Option<&str>,
    ) -> Result<(), LibraryError> {
        let head_commit = repo
            .head()
            .map_err(|e| LibraryError::Git(format!("head: {e}")))?
            .peel_to_commit()
            .map_err(|e| LibraryError::Git(format!("peel head: {e}")))?;
        let head_tree = head_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("head tree: {e}")))?;

        let segments: Vec<&str> = dir_path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Err(LibraryError::Git("empty dir_path".into()));
        }

        let new_oid = Self::remove_dir_recursive(repo, Some(&head_tree), &segments)?;
        if new_oid == head_tree.id() {
            tracing::debug!("delete_dir: tree unchanged, skipping commit");
            return Ok(());
        }

        let new_tree = repo
            .find_tree(new_oid)
            .map_err(|e| LibraryError::Git(format!("find new tree: {e}")))?;
        let sig = git2::Signature::now("drua-library", "drua-library@local")
            .map_err(|e| LibraryError::Git(format!("signature: {e}")))?;
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            commit_message,
            &new_tree,
            &[&head_commit],
        )
        .map_err(|e| LibraryError::Git(format!("commit: {e}")))?;

        Self::push_main(repo, token)
    }

    fn remove_dir_recursive(
        repo: &git2::Repository,
        dir_tree: Option<&git2::Tree>,
        segments: &[&str],
    ) -> Result<git2::Oid, LibraryError> {
        let mut tb = repo
            .treebuilder(dir_tree)
            .map_err(|e| LibraryError::Git(format!("treebuilder: {e}")))?;

        let head = segments[0];
        if segments.len() == 1 {
            if dir_tree.and_then(|t| t.get_name(head)).is_some() {
                tb.remove(head)
                    .map_err(|e| LibraryError::Git(format!("remove dir: {e}")))?;
            }
        } else {
            let sub = dir_tree
                .and_then(|t| t.get_name(head))
                .filter(|e| e.kind() == Some(git2::ObjectType::Tree))
                .and_then(|e| repo.find_tree(e.id()).ok());
            let Some(sub_tree) = sub else {
                return Ok(dir_tree.map(|t| t.id()).unwrap_or_else(git2::Oid::zero));
            };
            let new_sub_oid = Self::remove_dir_recursive(repo, Some(&sub_tree), &segments[1..])?;
            let new_sub = repo
                .find_tree(new_sub_oid)
                .map_err(|e| LibraryError::Git(format!("find sub: {e}")))?;
            const MODE_TREE: i32 = 0o040000;
            if new_sub.iter().count() == 0 {
                tb.remove(head)
                    .map_err(|e| LibraryError::Git(format!("remove dir: {e}")))?;
            } else {
                tb.insert(head, new_sub_oid, MODE_TREE)
                    .map_err(|e| LibraryError::Git(format!("insert dir: {e}")))?;
            }
        }

        tb.write()
            .map_err(|e| LibraryError::Git(format!("tree write: {e}")))
    }

    fn read_blob_at(
        repo: &git2::Repository,
        tree: &git2::Tree,
        path: &str,
    ) -> Result<Option<Vec<u8>>, LibraryError> {
        match tree.get_path(Path::new(path)) {
            Ok(entry) => {
                if entry.kind() != Some(git2::ObjectType::Blob) {
                    return Err(LibraryError::Validation(format!(
                        "path is a directory; create a file inside it: {path}"
                    )));
                }
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
                    if let Some(existing) = dir_tree.and_then(|t| t.get_name(name)) {
                        if existing.kind() == Some(git2::ObjectType::Tree) {
                            return Err(LibraryError::Validation(format!(
                                "path is a directory; create a file inside it: {name}"
                            )));
                        }
                    }
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

    fn open_or_clone(
        url: &str,
        path: &Path,
        token: Option<&str>,
    ) -> Result<git2::Repository, LibraryError> {
        if let Ok(repo) = git2::Repository::open_bare(path) {
            Self::fetch_origin(&repo, token)?;
            Self::reset_main_to_origin(&repo)?;
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
        let token = token.map(str::to_string);
        // Auth strategy:
        // - HTTPS with a token (GitHub App in prod): send as basic auth.
        // - SSH (gitconfig insteadOf rewrites https→ssh in dev): try
        //   ssh-agent first, then fall back to common `~/.ssh/id_*`
        //   files. The agent is often empty on macOS even when SSH
        //   itself works (lazy-loaded from Keychain), so the file
        //   fallback is what makes "it just works" for dev without
        //   requiring `ssh-add` first.
        // libgit2 calls credentials() repeatedly until one succeeds or
        // we return Err; we step through the strategies in order and
        // bail when exhausted.
        let mut ssh_attempt: usize = 0;
        let mut tried_userpass = false;
        cb.credentials(move |_url, username_from_url, allowed| {
            if allowed.contains(git2::CredentialType::USERNAME) {
                return git2::Cred::username(username_from_url.unwrap_or("git"));
            }
            if allowed.contains(git2::CredentialType::SSH_KEY) {
                let username = username_from_url.unwrap_or("git");
                if ssh_attempt == 0 {
                    ssh_attempt = 1;
                    return git2::Cred::ssh_key_from_agent(username);
                }
                // Agent failed (no keys, or no matching key). Walk the
                // common `~/.ssh/id_*` paths and return the first one
                // that exists. Skipping happens inline so libgit2 only
                // sees credentials it can actually try.
                let home = dirs::home_dir()
                    .ok_or_else(|| git2::Error::from_str("could not determine home directory"))?;
                let candidates = ["id_ed25519", "id_ecdsa", "id_rsa"];
                while let Some(name) = candidates.get(ssh_attempt - 1) {
                    ssh_attempt += 1;
                    let key = home.join(".ssh").join(name);
                    if key.exists() {
                        return git2::Cred::ssh_key(username, None, &key, None);
                    }
                }
                return Err(git2::Error::from_str(
                    "ssh authentication failed (agent empty and no usable ~/.ssh/id_* keys; \
                     run `ssh-add <your-key>` or configure a GitHub App)",
                ));
            }
            if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
                if tried_userpass {
                    return Err(git2::Error::from_str(
                        "userpass authentication failed (token rejected)",
                    ));
                }
                tried_userpass = true;
                if let Some(t) = token.as_deref() {
                    return git2::Cred::userpass_plaintext("x-access-token", t);
                }
            }
            git2::Cred::default()
        });
        cb
    }
}
