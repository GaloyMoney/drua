use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::attribution::CommitAttribution;
use crate::importer::GitFileHash;
use crate::{GitHubAppTokenProvider, LibraryError};

/// How long the writer waits for additional ops after the first one
/// arrives before processing the batch. Sized to absorb the jitter of
/// parallel tool calls in a single agent turn without delaying the
/// upstream push noticeably.
const BATCH_WINDOW: Duration = Duration::from_millis(25);
const MAX_BATCH: usize = 32;
const QUEUE_CAPACITY: usize = 256;

/// Cluster-wide Postgres advisory-lock key for serializing pushes to the
/// library repo's `main`. Fixed (one library repo per deployment); `0x647275616c6962` = "drualib".
const LIBRARY_PUSH_LOCK_KEY: i64 = 0x647275616c6962;

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
    /// True when the originating commit was authored by drua itself
    /// (committer in the `@{AGENT_DOMAIN}` namespace). DB-authoritative
    /// importers use this to drop echoes of their own writes/prunes;
    /// git-authoritative importers (spaces/notes/skills) ignore it.
    pub from_drua: bool,
}

/// Closure used by [`BatchOpKind::Rmw`]. Receives the current bytes at
/// the op's path (`None` if absent at HEAD) and returns the new content
/// (`None` deletes the path, `Some(_)` writes/overwrites). Returning
/// `Err(LibraryError::Validation(_))` aborts only this op; siblings in
/// the batch keep going.
pub type BatchRmwFn =
    Box<dyn Fn(Option<&[u8]>) -> Result<Option<Vec<u8>>, LibraryError> + Send + Sync>;

pub enum BatchOpKind {
    Write {
        path: String,
        content: Vec<u8>,
    },
    Delete {
        path: String,
    },
    Rmw {
        path: String,
        update: BatchRmwFn,
    },
    /// Same-tree rename. Errors with `Validation` if `from` is missing
    /// or `to` already exists in the parent tree.
    Move {
        from: String,
        to: String,
    },
    /// Atomic delete-then-write in one commit (importer canonical
    /// rewrites). Removes `from_path` and writes `content` at `to_path`.
    WriteWithRename {
        from_path: String,
        to_path: String,
        content: Vec<u8>,
    },
    /// Recursive directory removal. No-op if the directory is absent.
    DeleteDir {
        path: String,
    },
    /// Multi-file write/delete in a single commit (e.g. project-init
    /// scaffolding). Each `(path, content)` pair is applied to an
    /// evolving tree before commit.
    MultiFile {
        changes: Vec<(String, Option<Vec<u8>>)>,
    },
}

pub struct BatchOp {
    pub commit_message: String,
    pub kind: BatchOpKind,
    pub attribution: CommitAttribution,
}

struct QueuedOp {
    op: BatchOp,
    response: oneshot::Sender<Result<(), LibraryError>>,
}

/// Drop aborts the worker; lets it live as long as its owning `GitEngine`.
struct OwnedTaskHandle(Option<JoinHandle<()>>);

impl OwnedTaskHandle {
    fn new(inner: JoinHandle<()>) -> Self {
        Self(Some(inner))
    }
}

impl Drop for OwnedTaskHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub struct GitEngine {
    repo_path: PathBuf,
    /// Held by the writer for each batch and by `fetch_and_head`.
    /// Prevents the periodic fetch's mirror refspec from racing
    /// in-flight commits before `push_main` lands.
    repo_mutex: Arc<Mutex<()>>,
    write_tx: mpsc::Sender<QueuedOp>,
    local_commit_notify: Arc<Notify>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    _writer: OwnedTaskHandle,
}

impl GitEngine {
    /// Working-tree root. `<repo_path>/spaces/<slug>/...` is where
    /// SpaceFs reads land.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn local_commit_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.local_commit_notify)
    }

    #[tracing::instrument(name = "library.git.init", skip_all)]
    pub async fn init(
        repo_url: &str,
        repo_path: PathBuf,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
        pool: PgPool,
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

        let repo_mutex = Arc::new(Mutex::new(()));
        let (write_tx, write_rx) = mpsc::channel(QUEUE_CAPACITY);
        let local_commit_notify = Arc::new(Notify::new());
        let writer = tokio::spawn(Self::run_writer(
            repo_path.clone(),
            github_app.clone(),
            Arc::clone(&repo_mutex),
            Arc::clone(&local_commit_notify),
            write_rx,
            pool,
        ));

        Ok(Self {
            repo_path,
            repo_mutex,
            write_tx,
            local_commit_notify,
            github_app,
            _writer: OwnedTaskHandle::new(writer),
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

            Self::tree_diff_deltas(&repo, from_tree.as_ref(), Some(&to_tree), false)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("changes_since join: {e}")))?
    }

    /// Build [`CommitDelta`]s for a tree-to-tree diff. `Deleted` deltas carry
    /// empty content (the new side is gone); add/modify carry the new blob.
    /// `from_drua` tags every produced delta with the commit's provenance.
    fn tree_diff_deltas(
        repo: &git2::Repository,
        from_tree: Option<&git2::Tree>,
        to_tree: Option<&git2::Tree>,
        from_drua: bool,
    ) -> Result<Vec<CommitDelta>, LibraryError> {
        let diff = repo
            .diff_tree_to_tree(from_tree, to_tree, None)
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
                from_drua,
            });
        }
        Ok(deltas)
    }

    /// Like [`Self::changes_since`], but every delta is tagged with whether
    /// its originating commit was authored by drua (`from_drua`). The sync
    /// dispatcher uses that tag so DB-authoritative importers (workflows) can
    /// drop echoes of their own forward-sync writes / prune-orphan deletes —
    /// e.g. a prune-orphan re-read as a `Deleted` delta would otherwise
    /// soft-delete the live workflow at that path. Git-authoritative importers
    /// (spaces / notes / skills) are written *to git first* by drua and then
    /// reverse-synced into the DB, so they must still process drua commits and
    /// ignore the tag.
    ///
    /// Each commit is diffed against its first parent (a collapsed range diff
    /// could mis-attribute a net deletion to the wrong blob). When `from` is
    /// missing or not an ancestor of `to` (force-pushed `main`, GC'd
    /// checkpoint, rebuilt clone) the revwalk's `hide` wouldn't trim history,
    /// so we fall back to a bounded net tree diff tagged external — matching
    /// the pre-tagging behavior instead of replaying to the root.
    #[tracing::instrument(name = "library.git.changes_with_provenance", skip_all)]
    pub async fn changes_with_provenance(
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
            Self::provenance_deltas(&repo, from.as_deref(), &to)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("changes_with_provenance join: {e}")))?
    }

    /// Blocking core of [`Self::changes_with_provenance`]; see its docs.
    fn provenance_deltas(
        repo: &git2::Repository,
        from: Option<&str>,
        to: &str,
    ) -> Result<Vec<CommitDelta>, LibraryError> {
        let to_oid =
            git2::Oid::from_str(to).map_err(|e| LibraryError::Git(format!("parse to oid: {e}")))?;
        let to_tree = repo
            .find_commit(to_oid)
            .and_then(|c| c.tree())
            .map_err(|e| LibraryError::Git(format!("to tree: {e}")))?;

        // Initial sync (no checkpoint): import the whole tree as a snapshot.
        // Every present file is desired state, tagged external so all
        // importers ingest it (workflows included, on a fresh clone).
        let Some(from) = from else {
            return Self::tree_diff_deltas(repo, None, Some(&to_tree), false);
        };
        let from_oid = git2::Oid::from_str(from)
            .map_err(|e| LibraryError::Git(format!("parse from oid: {e}")))?;

        // Fall back to a bounded net diff (tagged external) when `from` is not
        // a strict ancestor of `to` — otherwise the walk below would re-emit
        // every commit back to the root.
        let from_is_ancestor = repo.find_commit(from_oid).is_ok()
            && repo.graph_descendant_of(to_oid, from_oid).unwrap_or(false);
        if !from_is_ancestor {
            let from_tree = repo.find_commit(from_oid).ok().and_then(|c| c.tree().ok());
            return Self::tree_diff_deltas(repo, from_tree.as_ref(), Some(&to_tree), false);
        }

        let mut walk = repo
            .revwalk()
            .map_err(|e| LibraryError::Git(format!("revwalk: {e}")))?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(|e| LibraryError::Git(format!("revwalk sort: {e}")))?;
        walk.push(to_oid)
            .map_err(|e| LibraryError::Git(format!("revwalk push: {e}")))?;
        walk.hide(from_oid)
            .map_err(|e| LibraryError::Git(format!("revwalk hide: {e}")))?;

        let drua_suffix = format!("@{}", crate::attribution::AGENT_DOMAIN);
        let mut deltas: Vec<CommitDelta> = Vec::new();
        for oid in walk {
            let oid = oid.map_err(|e| LibraryError::Git(format!("revwalk next: {e}")))?;
            let commit = repo
                .find_commit(oid)
                .map_err(|e| LibraryError::Git(format!("find commit: {e}")))?;
            let from_drua = commit
                .committer()
                .email()
                .map(|e| e.ends_with(&drua_suffix))
                .unwrap_or(false);
            let commit_tree = commit
                .tree()
                .map_err(|e| LibraryError::Git(format!("commit tree: {e}")))?;
            let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
            deltas.extend(Self::tree_diff_deltas(
                repo,
                parent_tree.as_ref(),
                Some(&commit_tree),
                from_drua,
            )?);
        }
        Ok(deltas)
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
        let _guard = self.repo_mutex.lock().await;
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

    /// Blind overwrite (or create) of `path`.
    #[tracing::instrument(name = "library.git.write_file", skip_all, fields(%path))]
    pub async fn write_file(
        &self,
        path: String,
        content: Vec<u8>,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Write { path, content },
            attribution,
        })
        .await
    }

    /// Remove `path`. No-op if absent at HEAD.
    #[tracing::instrument(name = "library.git.delete_file", skip_all, fields(%path))]
    pub async fn delete_file(
        &self,
        path: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Delete { path },
            attribution,
        })
        .await
    }

    /// Read–modify–write at `path`. The closure runs against the
    /// freshest content (parent-commit's tree) and may return
    /// `Err(LibraryError::Validation(_))` to abort just this op.
    #[tracing::instrument(name = "library.git.update_file", skip_all, fields(%path))]
    pub async fn update_file(
        &self,
        path: String,
        update: BatchRmwFn,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Rmw { path, update },
            attribution,
        })
        .await
    }

    /// Atomically rename `from` → `to`. Errors with `Validation` if
    /// `from` is missing or `to` already exists at the parent commit.
    #[tracing::instrument(name = "library.git.move_file", skip_all, fields(%from, %to))]
    pub async fn move_file(
        &self,
        from: String,
        to: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Move { from, to },
            attribution,
        })
        .await
    }

    /// Delete `from_path` and write `content` at `to_path` in a single
    /// commit. Used by the importer when rewriting an imported file to
    /// canonical form at a new path.
    #[tracing::instrument(name = "library.git.write_with_rename", skip_all, fields(%from_path, %to_path))]
    pub async fn write_with_rename(
        &self,
        from_path: String,
        to_path: String,
        content: Vec<u8>,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::WriteWithRename {
                from_path,
                to_path,
                content,
            },
            attribution,
        })
        .await
    }

    /// Recursively remove every blob under `dir_path` in one commit.
    /// No-op if the directory doesn't exist at HEAD.
    #[tracing::instrument(name = "library.git.delete_dir", skip_all, fields(%dir_path))]
    pub async fn delete_dir(
        &self,
        dir_path: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::DeleteDir { path: dir_path },
            attribution,
        })
        .await
    }

    /// Apply multiple `(path, content_opt)` changes in a single commit.
    /// `None` deletes (no-op if absent). No-op overall if the resulting
    /// tree matches HEAD.
    #[tracing::instrument(name = "library.git.commit_changes", skip_all, fields(count = changes.len()))]
    pub async fn commit_changes(
        &self,
        changes: Vec<(String, Option<Vec<u8>>)>,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        if changes.is_empty() {
            return Ok(());
        }
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::MultiFile { changes },
            attribution,
        })
        .await
    }

    /// Push a single op onto the writer queue and await its result.
    /// Failure to enqueue (writer task gone) or to receive the response
    /// (response channel closed) collapses to a `Git(_)` error.
    async fn enqueue(&self, op: BatchOp) -> Result<(), LibraryError> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(QueuedOp { op, response: tx })
            .await
            .map_err(|_| LibraryError::Git("git writer task is gone".into()))?;
        rx.await
            .map_err(|_| LibraryError::Git("git writer dropped response".into()))?
    }

    /// Drains the queue forever: takes the first op, waits up to
    /// [`BATCH_WINDOW`] for siblings, then runs the whole batch as
    /// N commits + 1 push under the [`Self::repo_mutex`].
    async fn run_writer(
        repo_path: PathBuf,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
        repo_mutex: Arc<Mutex<()>>,
        local_commit_notify: Arc<Notify>,
        mut rx: mpsc::Receiver<QueuedOp>,
        pool: PgPool,
    ) {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            let deadline = Instant::now() + BATCH_WINDOW;
            while batch.len() < MAX_BATCH {
                let now = Instant::now();
                let timeout = deadline.saturating_duration_since(now);
                if timeout.is_zero() {
                    break;
                }
                match tokio::time::timeout(timeout, rx.recv()).await {
                    Ok(Some(op)) => batch.push(op),
                    Ok(None) => return, // channel closed
                    Err(_) => break,    // window elapsed
                }
            }
            let any_ok =
                Self::process_batch(&repo_path, github_app.as_ref(), &repo_mutex, &pool, batch)
                    .await;
            if any_ok {
                local_commit_notify.notify_waiters();
            }
        }
    }

    #[tracing::instrument(name = "library.git.process_batch", skip_all, fields(n = batch.len()))]
    async fn process_batch(
        repo_path: &Path,
        github_app: Option<&Arc<GitHubAppTokenProvider>>,
        repo_mutex: &Mutex<()>,
        pool: &PgPool,
        batch: Vec<QueuedOp>,
    ) -> bool {
        let _guard = repo_mutex.lock().await;
        // Cluster-wide push serialization (HA): the per-pod `repo_mutex` only
        // orders writes within a pod; this advisory lock ensures at most one
        // pod mutates `main` at a time, so divergent ephemeral clones can't
        // race the remote into non-ff retries / lost commits. `xact`-scoped so
        // it always releases (commit, rollback, or connection drop). Best
        // effort: a DB hiccup degrades to the prior unlocked behavior rather
        // than wedging all library writes.
        let mut push_lock = match pool.begin().await {
            Ok(mut tx) => match sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(LIBRARY_PUSH_LOCK_KEY)
                .execute(&mut *tx)
                .await
            {
                Ok(_) => Some(tx),
                Err(e) => {
                    tracing::warn!(error = %e, "library push lock: acquire failed; proceeding unlocked");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "library push lock: begin failed; proceeding unlocked");
                None
            }
        };
        let token = Self::fresh_token(github_app).await;
        let path = repo_path.to_path_buf();
        let n = batch.len();
        let (ops, responders): (Vec<BatchOp>, Vec<oneshot::Sender<Result<(), LibraryError>>>) =
            batch.into_iter().map(|q| (q.op, q.response)).unzip();

        let results = tokio::task::spawn_blocking(move || -> Vec<Result<(), LibraryError>> {
            Self::commit_each_then_push_blocking(&path, ops, token.as_deref())
        })
        .await
        .unwrap_or_else(|e| {
            let msg = format!("commit_each_then_push join: {e}");
            (0..n)
                .map(|_| Err(LibraryError::Git(msg.clone())))
                .collect()
        });

        if let Some(tx) = push_lock.take() {
            if let Err(e) = tx.commit().await {
                tracing::warn!(error = %e, "library push lock: release failed");
            }
        }

        let any_ok = results.iter().any(|r| r.is_ok());
        for (resp, res) in responders.into_iter().zip(results) {
            let _ = resp.send(res);
        }
        any_ok
    }

    /// Apply N ops as N commits, then push once. On non-FF push, fetch
    /// origin, reset local main, and replay every op against the new
    /// HEAD; second push failure rolls local main back to the
    /// pre-batch HEAD and surfaces `Git("push failed: ...")` on every
    /// op that committed locally so the caller can mark them as
    /// cascaded. Per-op `Validation` errors don't advance HEAD — the
    /// next op layers on the previous successful commit.
    fn commit_each_then_push_blocking(
        repo_path: &Path,
        ops: Vec<BatchOp>,
        token: Option<&str>,
    ) -> Vec<Result<(), LibraryError>> {
        if ops.is_empty() {
            return Vec::new();
        }
        let repo = match git2::Repository::open_bare(repo_path) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("open bare: {e}");
                return ops
                    .iter()
                    .map(|_| Err(LibraryError::Git(msg.clone())))
                    .collect();
            }
        };

        const MAX_ATTEMPTS: u32 = 2;
        let initial_head_oid = match repo.head().and_then(|r| r.peel_to_commit()) {
            Ok(c) => c.id(),
            Err(e) => {
                let msg = format!("head: {e}");
                return ops
                    .iter()
                    .map(|_| Err(LibraryError::Git(msg.clone())))
                    .collect();
            }
        };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let parent_oid_at_attempt_start = match repo.head().and_then(|r| r.peel_to_commit()) {
                Ok(c) => c.id(),
                Err(e) => {
                    let msg = format!("head: {e}");
                    return ops
                        .iter()
                        .map(|_| Err(LibraryError::Git(msg.clone())))
                        .collect();
                }
            };
            let mut current_parent_oid = parent_oid_at_attempt_start;
            let mut per_op: Vec<Result<(), LibraryError>> = Vec::with_capacity(ops.len());
            for op in &ops {
                match Self::commit_one(&repo, current_parent_oid, op) {
                    Ok(Some(new_oid)) => {
                        per_op.push(Ok(()));
                        current_parent_oid = new_oid;
                    }
                    Ok(None) => per_op.push(Ok(())),
                    Err(e) => per_op.push(Err(e)),
                }
            }

            if current_parent_oid == parent_oid_at_attempt_start {
                return per_op;
            }

            match Self::push_main(&repo, token) {
                Ok(()) => return per_op,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::info!(
                        error = %e, attempt,
                        "commit_each_then_push: push failed, refetching and retrying"
                    );
                    if let Err(fe) = Self::fetch_origin(&repo, token) {
                        let msg = fe.to_string();
                        return ops
                            .iter()
                            .map(|_| Err(LibraryError::Git(msg.clone())))
                            .collect();
                    }
                    if let Err(re) = Self::reset_main_to_origin(&repo) {
                        let msg = re.to_string();
                        return ops
                            .iter()
                            .map(|_| Err(LibraryError::Git(msg.clone())))
                            .collect();
                    }
                }
                Err(e) => {
                    let _ = repo.reference(
                        "refs/heads/main",
                        initial_head_oid,
                        true,
                        "rollback after push failure",
                    );
                    let msg = format!("push failed: {e}");
                    return per_op
                        .into_iter()
                        .map(|r| match r {
                            Ok(()) => Err(LibraryError::Git(msg.clone())),
                            Err(e) => Err(e),
                        })
                        .collect();
                }
            }
        }
    }

    /// One commit step inside [`Self::commit_each_then_push_blocking`].
    /// `Ok(Some(oid))` = real commit; `Ok(None)` = tree unchanged;
    /// `Err(_)` = per-op validation or git failure (skip, don't advance).
    fn commit_one(
        repo: &git2::Repository,
        parent_oid: git2::Oid,
        op: &BatchOp,
    ) -> Result<Option<git2::Oid>, LibraryError> {
        let parent_commit = repo
            .find_commit(parent_oid)
            .map_err(|e| LibraryError::Git(format!("find parent commit: {e}")))?;
        let parent_tree = parent_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("parent tree: {e}")))?;

        let new_tree_oid = match &op.kind {
            BatchOpKind::Write { path, content } => {
                Self::apply_edit(repo, &parent_tree, path, Some(content.clone()))?
            }
            BatchOpKind::Delete { path } => match Self::read_blob_at(repo, &parent_tree, path)? {
                Some(_) => Self::apply_edit(repo, &parent_tree, path, None)?,
                None => parent_tree.id(),
            },
            BatchOpKind::Rmw { path, update } => {
                let current = Self::read_blob_at(repo, &parent_tree, path)?;
                let new_content = update(current.as_deref())?;
                Self::apply_edit(repo, &parent_tree, path, new_content)?
            }
            BatchOpKind::Move { from, to } => {
                if from == to {
                    return Err(LibraryError::Validation(format!(
                        "move: src and dest are the same: {from}"
                    )));
                }
                let from_content =
                    Self::read_blob_at(repo, &parent_tree, from)?.ok_or_else(|| {
                        LibraryError::Validation(format!("move: src does not exist: {from}"))
                    })?;
                if Self::read_blob_at(repo, &parent_tree, to)?.is_some() {
                    return Err(LibraryError::Validation(format!(
                        "move: dest already exists: {to}"
                    )));
                }
                let intermediate_oid = Self::apply_edit(repo, &parent_tree, from, None)?;
                let intermediate_tree = repo
                    .find_tree(intermediate_oid)
                    .map_err(|e| LibraryError::Git(format!("find intermediate: {e}")))?;
                Self::apply_edit(repo, &intermediate_tree, to, Some(from_content))?
            }
            BatchOpKind::WriteWithRename {
                from_path,
                to_path,
                content,
            } => {
                let intermediate_oid = if from_path == to_path {
                    parent_tree.id()
                } else {
                    Self::apply_edit(repo, &parent_tree, from_path, None)?
                };
                let intermediate_tree = repo
                    .find_tree(intermediate_oid)
                    .map_err(|e| LibraryError::Git(format!("find intermediate: {e}")))?;
                Self::apply_edit(repo, &intermediate_tree, to_path, Some(content.clone()))?
            }
            BatchOpKind::DeleteDir { path } => {
                let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                if segments.is_empty() {
                    return Err(LibraryError::Git("empty dir_path".into()));
                }
                Self::remove_dir_recursive(repo, Some(&parent_tree), &segments)?
            }
            BatchOpKind::MultiFile { changes } => {
                let mut current_oid = parent_tree.id();
                for (path, content) in changes {
                    let current_tree = repo
                        .find_tree(current_oid)
                        .map_err(|e| LibraryError::Git(format!("find tree: {e}")))?;
                    current_oid = Self::apply_edit(repo, &current_tree, path, content.clone())?;
                }
                current_oid
            }
        };

        if new_tree_oid == parent_tree.id() {
            return Ok(None);
        }

        let new_tree = repo
            .find_tree(new_tree_oid)
            .map_err(|e| LibraryError::Git(format!("find tree: {e}")))?;
        let author =
            git2::Signature::now(&op.attribution.author_name, &op.attribution.author_email)
                .map_err(|e| LibraryError::Git(format!("author signature: {e}")))?;
        let committer = git2::Signature::now(
            &op.attribution.committer_name,
            &op.attribution.committer_email,
        )
        .map_err(|e| LibraryError::Git(format!("committer signature: {e}")))?;
        let mut message = op.commit_message.clone();
        message.push_str(&op.attribution.render_message_suffix());
        let commit_oid = repo
            .commit(
                Some("HEAD"),
                &author,
                &committer,
                &message,
                &new_tree,
                &[&parent_commit],
            )
            .map_err(|e| LibraryError::Git(format!("commit: {e}")))?;
        Ok(Some(commit_oid))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "drua-git-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn commit_file(
        repo: &git2::Repository,
        parent: Option<git2::Oid>,
        path: &str,
        content: &[u8],
        committer_email: &str,
    ) -> git2::Oid {
        let base = parent.map(|p| repo.find_commit(p).unwrap().tree().unwrap());
        let mut tb = repo.treebuilder(base.as_ref()).unwrap();
        let blob = repo.blob(content).unwrap();
        tb.insert(path, blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", committer_email).unwrap();
        let parents: Vec<git2::Commit> = parent
            .into_iter()
            .map(|p| repo.find_commit(p).unwrap())
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "msg", &tree, &parent_refs)
            .unwrap()
    }

    #[test]
    fn provenance_deltas_tags_drua_commits() {
        let dir = unique_dir("echo");
        let repo = git2::Repository::init_bare(&dir).unwrap();

        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com");
        // drua forward-sync style: tagged from_drua = true.
        let c1 = commit_file(
            &repo,
            Some(c0),
            "a.yml",
            b"v1",
            crate::attribution::LIBRARY_BOT_EMAIL,
        );
        // external human edit: tagged from_drua = false.
        let c2 = commit_file(&repo, Some(c1), "b.yml", b"hi", "human@example.com");

        let deltas =
            GitEngine::provenance_deltas(&repo, Some(&c0.to_string()), &c2.to_string()).unwrap();

        let a = deltas
            .iter()
            .find(|d| d.path == "a.yml")
            .expect("a.yml delta present");
        assert!(a.from_drua, "drua commit's delta must be tagged from_drua");
        let b = deltas
            .iter()
            .find(|d| d.path == "b.yml")
            .expect("b.yml delta present");
        assert!(!b.from_drua, "external commit's delta must not be tagged");
        assert!(matches!(b.kind, DeltaKind::Added));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provenance_deltas_snapshots_without_checkpoint() {
        let dir = unique_dir("snapshot");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        // Even a drua-authored commit is desired state on initial snapshot.
        let c0 = commit_file(
            &repo,
            None,
            "a.yml",
            b"v0",
            crate::attribution::LIBRARY_BOT_EMAIL,
        );

        let deltas = GitEngine::provenance_deltas(&repo, None, &c0.to_string()).unwrap();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].path, "a.yml");
        // Snapshot is authoritative — workflows must import on a fresh clone.
        assert!(!deltas[0].from_drua);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provenance_deltas_net_diff_when_from_not_ancestor() {
        let dir = unique_dir("forcepush");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com");
        // Two divergent children of c0 — neither is an ancestor of the other,
        // as if `main` was force-updated under a stale checkpoint.
        let to = commit_file(&repo, Some(c0), "b.yml", b"b", "human@example.com");
        let stale_from = commit_file(&repo, Some(c0), "c.yml", b"c", "human@example.com");

        let deltas =
            GitEngine::provenance_deltas(&repo, Some(&stale_from.to_string()), &to.to_string())
                .unwrap();

        // Bounded net diff between the two trees (b.yml added, c.yml removed) —
        // NOT a replay of history back to the root.
        let paths: std::collections::BTreeSet<&str> =
            deltas.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            ["b.yml", "c.yml"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(deltas.iter().all(|d| !d.from_drua));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
