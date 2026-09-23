use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
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

/// PG NOTIFY channel fired after a successful push. Every replica's
/// fetcher LISTENs on it, so a write on one replica is visible
/// cluster-wide in milliseconds instead of after each replica's fetch
/// ticker (`fetch_interval_ms`). Payload is empty; the wake-up is
/// purely a hint, the ticker remains the backstop.
const LIBRARY_HEAD_NOTIFY_CHANNEL: &str = "library_head_changed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Added,
    Modified,
    Deleted,
}

/// `(path, content)` pairs produced by a tree walk.
pub type BlobEntries = Vec<(String, Vec<u8>)>;

/// Result of one [`BatchOp`]: the new commit oid, or `None` if the tree
/// was unchanged.
type WriteResult = Result<Option<String>, LibraryError>;

/// One immediate child of a tree at HEAD. Returned by `list_dir_at_head`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// First/last commit times for one path at HEAD. `created` follows renames
/// (a pure rename keeps the source's `created`); `modified` is the last
/// commit that added, modified or renamed the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathDates {
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
}

/// `path → PathDates` for every blob under one prefix at HEAD, keyed
/// relative to that prefix (the prefix itself is stripped before the
/// map is cached — see [`GitEngine::path_dates_at_head`]).
pub type PathDatesMap = HashMap<String, PathDates>;

/// One path-affecting change from a single commit's diff, in the order
/// `fold_path_dates` needs to apply them.
#[derive(Debug, Clone)]
enum PathEvent {
    Touched { path: String },
    Renamed { from: String, to: String },
    Deleted { path: String },
}

/// A previously computed [`PathDatesMap`] and the HEAD it was computed
/// at, so a later call can fold forward instead of walking from scratch.
struct CachedPathDates {
    head: git2::Oid,
    dates: Arc<PathDatesMap>,
}

/// Applies one commit's path events, in the order libgit2 reported
/// them, to `map`. Pure — no git access — so the four properties below
/// are unit-tested without a repository:
/// - touch-touch keeps the first `created`.
/// - a rename preserves `created` and moves `modified` to `at`.
/// - delete then re-add restarts `created` at the re-add's time.
/// - a rename whose source isn't in `map` (the incremental walk's
///   cached base started after the source was created — never happens
///   in practice, since the source would already be in the cached map)
///   falls back to `created: at`.
fn fold_path_dates(map: &mut PathDatesMap, at: DateTime<Utc>, events: Vec<PathEvent>) {
    for ev in events {
        match ev {
            PathEvent::Touched { path } => {
                let entry = map.entry(path).or_insert(PathDates {
                    created: at,
                    modified: at,
                });
                entry.modified = at;
            }
            PathEvent::Renamed { from, to } => {
                let created = map.remove(&from).map(|d| d.created).unwrap_or(at);
                map.insert(
                    to,
                    PathDates {
                        created,
                        modified: at,
                    },
                );
            }
            PathEvent::Deleted { path } => {
                map.remove(&path);
            }
        }
    }
}

/// Ensures `prefix` ends with `/` (unless empty), so it's both a valid
/// directory pathspec and an unambiguous strip prefix.
fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// Strips `prefix` (already `/`-normalized) off `path`, falling back to
/// the untouched path if it doesn't start with `prefix` (defensive —
/// the diff is already scoped to `prefix` via `pathspec`).
fn strip(prefix: &str, path: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

#[derive(Debug, Clone)]
pub struct CommitDelta {
    pub path: String,
    pub kind: DeltaKind,
    pub file_hash: GitFileHash,
    /// Empty for `Deleted`.
    pub content: Vec<u8>,
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
    /// A pre-built tree committed as-is (no `apply_edit`), with a second
    /// parent alongside the batch's normal parent. Used by
    /// [`GitEngine::merge_into_main`] — `tree_oid` is the already-computed
    /// merge result, `second_parent` the changeset tip being merged in.
    MergeCommit {
        tree_oid: String,
        second_parent: String,
    },
}

pub struct BatchOp {
    pub commit_message: String,
    pub kind: BatchOpKind,
    pub attribution: CommitAttribution,
    /// `refs/heads/<name>` this op commits and pushes to. `None` =
    /// `refs/heads/main` (today's behaviour, unchanged). Ops in the same
    /// batch are grouped by `target_ref` and each group is committed and
    /// pushed independently — see [`GitEngine::commit_each_then_push_blocking`].
    pub target_ref: Option<String>,
}

struct QueuedOp {
    op: BatchOp,
    response: oneshot::Sender<WriteResult>,
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
    /// in-flight commits before `push_ref` lands — the invariant every
    /// new ref-mutating method (`create_ref`, `delete_ref`, `rebase_ref`)
    /// must also hold, since the mirror refspec force-overwrites any
    /// local `refs/heads/*` a concurrent fetch observes.
    repo_mutex: Arc<Mutex<()>>,
    write_tx: mpsc::Sender<QueuedOp>,
    /// Wakes the fetcher. Fired by the local writer after a successful
    /// batch and by the head listener on any peer replica's push.
    commit_notify: Arc<Notify>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    /// Per-prefix `path_dates_at_head` cache. Held only to clone the
    /// `Arc` out and to store a freshly computed result back — never
    /// across the blocking walk. Two concurrent misses for the same
    /// prefix may both compute; the second store wins, and both
    /// results are correct for the HEAD they were computed at, so
    /// there is deliberately no per-prefix lock.
    path_dates: Arc<std::sync::Mutex<HashMap<String, CachedPathDates>>>,
    _writer: OwnedTaskHandle,
    _listener: OwnedTaskHandle,
}

impl GitEngine {
    /// Working-tree root. `<repo_path>/spaces/<slug>/...` is where
    /// SpaceFs reads land.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn commit_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.commit_notify)
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
        let commit_notify = Arc::new(Notify::new());
        let writer = tokio::spawn(Self::run_writer(
            repo_path.clone(),
            github_app.clone(),
            Arc::clone(&repo_mutex),
            Arc::clone(&commit_notify),
            write_rx,
            pool.clone(),
        ));
        let listener = tokio::spawn(Self::run_head_listener(pool, Arc::clone(&commit_notify)));

        Ok(Self {
            repo_path,
            repo_mutex,
            write_tx,
            commit_notify,
            github_app,
            path_dates: Arc::new(std::sync::Mutex::new(HashMap::new())),
            _writer: OwnedTaskHandle::new(writer),
            _listener: OwnedTaskHandle::new(listener),
        })
    }

    /// Cluster-wide counterpart of the writer's local wake-up: any
    /// replica's successful push `pg_notify`s [`LIBRARY_HEAD_NOTIFY_CHANNEL`],
    /// waking this replica's fetcher immediately. While PG is
    /// unreachable, sync degrades to ticker cadence.
    async fn run_head_listener(pool: PgPool, commit_notify: Arc<Notify>) {
        loop {
            let mut listener = match sqlx::postgres::PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "library head listener: connect failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen(LIBRARY_HEAD_NOTIFY_CHANNEL).await {
                tracing::warn!(error = %e, "library head listener: LISTEN failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            loop {
                match listener.recv().await {
                    Ok(_) => commit_notify.notify_one(),
                    Err(e) => {
                        tracing::warn!(error = %e, "library head listener: recv failed; reconnecting");
                        break;
                    }
                }
            }
        }
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

            Self::tree_diff_deltas(&repo, from_tree.as_ref(), Some(&to_tree))
        })
        .await
        .map_err(|e| LibraryError::Git(format!("changes_since join: {e}")))?
    }

    /// Build [`CommitDelta`]s for a tree-to-tree diff. For `Deleted` the
    /// `content` is the removed file's old blob (so importers can make
    /// id-aware delete decisions); add/modify carry the new blob.
    fn tree_diff_deltas(
        repo: &git2::Repository,
        from_tree: Option<&git2::Tree>,
        to_tree: Option<&git2::Tree>,
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
            // For deletes, `oid` is the removed blob (still in the object DB via
            // the parent commit). Empty on read failure → importers fall back
            // to a path lookup / skip.
            let content = match repo.find_blob(oid) {
                Ok(b) => b.content().to_vec(),
                Err(_) if matches!(kind, DeltaKind::Deleted) => Vec::new(),
                Err(_) => continue,
            };
            deltas.push(CommitDelta {
                path: path_str,
                kind,
                file_hash: GitFileHash::new(oid.to_string()),
                content,
            });
        }
        Ok(deltas)
    }

    /// Like [`Self::changes_since`], but ignores commits drua made to merely
    /// project DB state into git — those carrying the `Drua-Projection`
    /// trailer ([`crate::attribution::message_is_projection`]) committed by a
    /// drua bot. Reverse-sync must treat only *authoritative* edits as inputs:
    /// a forward-sync write or prune-orphan delete already reflects persisted
    /// DB state, so re-ingesting it feeds back into spurious deletes — e.g. a
    /// prune-orphan re-read as a `Deleted` delta would soft-delete the live
    /// workflow at that path. Direct authoring writes (`spaces edit`, human
    /// commits) lack the trailer and are imported normally — that's how a
    /// space-authored note/skill lands in the DB.
    ///
    /// Each surviving commit is diffed against its first parent (a collapsed
    /// range diff could mis-attribute a net deletion to the wrong blob). When
    /// `from` is not a strict ancestor of `to` (force-pushed `main`) the walk
    /// is bounded by the merge-base instead of replaying to the root, so the
    /// projection filter still applies. A `from` whose commit is gone (GC'd /
    /// force-pushed away) fails the tick rather than silently resyncing and
    /// dropping deletions.
    #[tracing::instrument(name = "library.git.external_changes_since", skip_all)]
    pub async fn external_changes_since(
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
            Self::external_deltas(&repo, from.as_deref(), &to)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("external_changes_since join: {e}")))?
    }

    /// Blocking core of [`Self::external_changes_since`]; see its docs.
    fn external_deltas(
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

        // Initial sync (no checkpoint): import the whole tree as a snapshot —
        // every present file is desired state (workflows included, fresh clone).
        let Some(from) = from else {
            return Self::tree_diff_deltas(repo, None, Some(&to_tree));
        };
        let from_oid = git2::Oid::from_str(from)
            .map_err(|e| LibraryError::Git(format!("parse from oid: {e}")))?;

        // A missing checkpoint commit (GC'd, or a tip that was force-pushed
        // away) must FAIL the tick — not silently resync. Diffing from an empty
        // tree would drop every deletion since the lost commit yet still advance
        // the checkpoint, diverging DB from git. Erroring keeps the checkpoint
        // so the next tick retries (matches the pre-walk `changes_since`).
        repo.find_commit(from_oid)
            .map_err(|e| LibraryError::Git(format!("checkpoint commit {from} missing: {e}")))?;

        // Bound the walk by the merge-base so a force-updated `main` (where
        // `from` is not a strict ancestor of `to`) doesn't replay history to the
        // root — while still running every surviving commit through the
        // projection filter below (a flat net diff would let drua's own
        // prune/delete net-removals resurface as external `Deleted` deltas and
        // reopen the domino). In the normal case the merge-base IS `from`.
        let base = repo
            .merge_base(from_oid, to_oid)
            .map_err(|e| LibraryError::Git(format!("merge-base of checkpoint and head: {e}")))?;

        let mut walk = repo
            .revwalk()
            .map_err(|e| LibraryError::Git(format!("revwalk: {e}")))?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(|e| LibraryError::Git(format!("revwalk sort: {e}")))?;
        walk.push(to_oid)
            .map_err(|e| LibraryError::Git(format!("revwalk push: {e}")))?;
        walk.hide(base)
            .map_err(|e| LibraryError::Git(format!("revwalk hide: {e}")))?;

        let drua_suffix = format!("@{}", crate::attribution::AGENT_DOMAIN);
        let mut deltas: Vec<CommitDelta> = Vec::new();
        for oid in walk {
            let oid = oid.map_err(|e| LibraryError::Git(format!("revwalk next: {e}")))?;
            let commit = repo
                .find_commit(oid)
                .map_err(|e| LibraryError::Git(format!("find commit: {e}")))?;
            // Skip drua's own projection commits. Gate on the drua committer too
            // so an external actor can't forge the trailer to suppress a commit.
            let by_drua = commit
                .committer()
                .email()
                .map(|e| e.ends_with(&drua_suffix))
                .unwrap_or(false);
            let is_projection = commit
                .message()
                .map(crate::attribution::message_is_projection)
                .unwrap_or(false);
            if by_drua && is_projection {
                continue;
            }
            let commit_tree = commit
                .tree()
                .map_err(|e| LibraryError::Git(format!("commit tree: {e}")))?;
            let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
            deltas.extend(Self::tree_diff_deltas(
                repo,
                parent_tree.as_ref(),
                Some(&commit_tree),
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
            let Some(tree) = Self::head_tree(&repo)? else {
                return Ok(None);
            };
            Self::blob_in_tree(&repo, &tree, &path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("read_blob_at_head join: {e}")))?
    }

    /// Read the blob at `path` from `commit_oid`'s tree. `Ok(None)` when
    /// the path doesn't exist at that commit. The ref-aware counterpart
    /// of [`Self::read_blob_at_head`] — `SpaceFs` uses it once a call
    /// resolves to a changeset target.
    #[tracing::instrument(name = "library.git.read_blob_at", skip_all, fields(%commit_oid, %path))]
    pub async fn read_blob_at(
        &self,
        commit_oid: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let commit_oid = commit_oid.to_string();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let tree = Self::commit_tree(&repo, &commit_oid)?;
            Self::blob_in_tree(&repo, &tree, &path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("read_blob_at join: {e}")))?
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
            let Some(root) = Self::head_tree(&repo)? else {
                return Ok(None);
            };
            Self::dir_entries(&repo, &root, &dir_path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("list_dir_at_head join: {e}")))?
    }

    /// Lists immediate children of `dir_path` at `commit_oid`'s tree.
    /// The ref-aware counterpart of [`Self::list_dir_at_head`].
    #[tracing::instrument(name = "library.git.list_dir_at", skip_all, fields(%commit_oid, %dir_path))]
    pub async fn list_dir_at(
        &self,
        commit_oid: &str,
        dir_path: &str,
    ) -> Result<Option<Vec<DirEntry>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let commit_oid = commit_oid.to_string();
        let dir_path = dir_path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<DirEntry>>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let root = Self::commit_tree(&repo, &commit_oid)?;
            Self::dir_entries(&repo, &root, &dir_path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("list_dir_at join: {e}")))?
    }

    /// Recursively walk every blob under `dir_path` at HEAD's tree.
    /// Returns `(absolute_path, content)` pairs (paths relative to the
    /// repo root). Empty `dir_path` walks the entire tree; a `dir_path`
    /// naming a blob yields just that blob, so callers can scope a walk
    /// to a single file. `Ok(None)` when the path doesn't exist (or HEAD
    /// is unborn) — distinct from `Ok(Some(vec![]))` for an empty tree.
    #[tracing::instrument(name = "library.git.walk_blobs_at_head", skip_all, fields(%dir_path))]
    pub async fn walk_blobs_at_head(
        &self,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let dir_path = dir_path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<BlobEntries>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            Self::blobs_at_head(&repo, &dir_path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("walk_blobs_at_head join: {e}")))?
    }

    /// Recursively walk every blob under `dir_path` at `commit_oid`'s
    /// tree. The ref-aware counterpart of [`Self::walk_blobs_at_head`].
    #[tracing::instrument(name = "library.git.walk_blobs_at", skip_all, fields(%commit_oid, %dir_path))]
    pub async fn walk_blobs_at(
        &self,
        commit_oid: &str,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let commit_oid = commit_oid.to_string();
        let dir_path = dir_path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<BlobEntries>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let tree = Self::commit_tree(&repo, &commit_oid)?;
            Self::blobs_in_tree(&repo, &tree, &dir_path)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("walk_blobs_at join: {e}")))?
    }

    /// `Ok(None)` when HEAD is unborn.
    fn head_tree(repo: &git2::Repository) -> Result<Option<git2::Tree<'_>>, LibraryError> {
        let Ok(head) = repo.head() else {
            return Ok(None);
        };
        let tree = head
            .peel_to_commit()
            .and_then(|c| c.tree())
            .map_err(|e| LibraryError::Git(format!("peel head tree: {e}")))?;
        Ok(Some(tree))
    }

    /// Errors (does not return `Ok(None)`) on a bad or missing oid —
    /// unlike HEAD, a changeset's base/tip oid always names a real
    /// commit, so a lookup failure here is a genuine error, not an
    /// "unborn" case.
    fn commit_tree<'repo>(
        repo: &'repo git2::Repository,
        oid: &str,
    ) -> Result<git2::Tree<'repo>, LibraryError> {
        let oid = git2::Oid::from_str(oid)
            .map_err(|e| LibraryError::Git(format!("parse commit oid: {e}")))?;
        repo.find_commit(oid)
            .and_then(|c| c.tree())
            .map_err(|e| LibraryError::Git(format!("commit tree: {e}")))
    }

    fn blobs_at_head(
        repo: &git2::Repository,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        let Some(tree) = Self::head_tree(repo)? else {
            return Ok(None);
        };
        Self::blobs_in_tree(repo, &tree, dir_path)
    }

    fn dir_entries(
        repo: &git2::Repository,
        root: &git2::Tree,
        dir_path: &str,
    ) -> Result<Option<Vec<DirEntry>>, LibraryError> {
        let owned;
        let tree: &git2::Tree = if dir_path.is_empty() {
            root
        } else {
            let entry = match root.get_path(Path::new(dir_path)) {
                Ok(e) => e,
                Err(_) => return Ok(None),
            };
            if entry.kind() != Some(git2::ObjectType::Tree) {
                return Ok(None);
            }
            owned = repo
                .find_tree(entry.id())
                .map_err(|e| LibraryError::Git(format!("find subtree: {e}")))?;
            &owned
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
    }

    fn blobs_in_tree(
        repo: &git2::Repository,
        root: &git2::Tree,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        let owned;
        let (subtree, prefix): (&git2::Tree, String) = if dir_path.is_empty() {
            (root, String::new())
        } else {
            let Ok(entry) = root.get_path(Path::new(dir_path)) else {
                return Ok(None);
            };
            match entry.kind() {
                Some(git2::ObjectType::Blob) => {
                    let blob = repo
                        .find_blob(entry.id())
                        .map_err(|e| LibraryError::Git(format!("find blob: {e}")))?;
                    return Ok(Some(vec![(dir_path.to_string(), blob.content().to_vec())]));
                }
                Some(git2::ObjectType::Tree) => {
                    owned = repo
                        .find_tree(entry.id())
                        .map_err(|e| LibraryError::Git(format!("find subtree: {e}")))?;
                    (&owned, format!("{dir_path}/"))
                }
                _ => return Ok(None),
            }
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
        Ok(Some(out))
    }

    /// Dates for every blob under `prefix` (repo-relative, trailing
    /// slash optional) at HEAD. `Ok(None)` when HEAD is unborn.
    /// Returned keys — and the cache's — have `prefix` stripped off,
    /// so callers get paths relative to it.
    ///
    /// Served from a per-prefix cache keyed by HEAD; a stale entry is
    /// brought forward by folding only the first-parent commits since
    /// the cached HEAD, rather than re-walking all of history. If the
    /// cached HEAD is no longer an ancestor of the current HEAD (a
    /// force-push to the library, or `main` moved sideways), the
    /// cached entry is discarded and a full walk runs instead —
    /// folding onto a disconnected base would silently attribute
    /// dates to the wrong history.
    #[tracing::instrument(name = "library.git.path_dates_at_head", skip_all, fields(%prefix))]
    pub async fn path_dates_at_head(
        &self,
        prefix: &str,
    ) -> Result<Option<Arc<PathDatesMap>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let prefix = prefix.to_string();
        let cache = Arc::clone(&self.path_dates);
        tokio::task::spawn_blocking(move || -> Result<Option<Arc<PathDatesMap>>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            Self::path_dates_blocking(&repo, &prefix, &cache)
        })
        .await
        .map_err(|e| LibraryError::Git(format!("path_dates_at_head join: {e}")))?
    }

    /// Blocking body of [`Self::path_dates_at_head`], factored out so
    /// unit tests can exercise it against a hand-built `git2::Repository`
    /// without a full `GitEngine` (mirrors `walk_blobs_at_head` /
    /// `walk_blobs_at`).
    fn path_dates_blocking(
        repo: &git2::Repository,
        prefix: &str,
        cache: &std::sync::Mutex<HashMap<String, CachedPathDates>>,
    ) -> Result<Option<Arc<PathDatesMap>>, LibraryError> {
        let Ok(head) = repo.head() else {
            return Ok(None);
        };
        let Some(head_oid) = head.target() else {
            return Ok(None);
        };

        let stale = {
            let guard = cache.lock().expect("path_dates cache lock poisoned");
            guard.get(prefix).map(|c| (c.head, Arc::clone(&c.dates)))
        };
        if let Some((cached_head, dates)) = &stale {
            if *cached_head == head_oid {
                return Ok(Some(Arc::clone(dates)));
            }
        }

        let fold_base = match &stale {
            Some((cached_head, dates))
                if repo
                    .graph_descendant_of(head_oid, *cached_head)
                    .map_err(|e| {
                        LibraryError::Git(format!("path_dates graph_descendant_of: {e}"))
                    })? =>
            {
                Some((*cached_head, (**dates).clone()))
            }
            _ => None,
        };

        let mut walk = repo
            .revwalk()
            .map_err(|e| LibraryError::Git(format!("path_dates revwalk: {e}")))?;
        walk.simplify_first_parent()
            .map_err(|e| LibraryError::Git(format!("path_dates simplify: {e}")))?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
            .map_err(|e| LibraryError::Git(format!("path_dates sort: {e}")))?;
        walk.push(head_oid)
            .map_err(|e| LibraryError::Git(format!("path_dates push: {e}")))?;
        if let Some((cached_head, _)) = &fold_base {
            walk.hide(*cached_head)
                .map_err(|e| LibraryError::Git(format!("path_dates hide: {e}")))?;
        }

        let mut oids = Vec::new();
        for oid in walk {
            oids.push(oid.map_err(|e| LibraryError::Git(format!("path_dates next: {e}")))?);
        }
        oids.reverse();

        let strip_prefix = normalize_prefix(prefix);
        let mut map = fold_base.map(|(_, m)| m).unwrap_or_default();
        for oid in oids {
            let commit = repo
                .find_commit(oid)
                .map_err(|e| LibraryError::Git(format!("path_dates find commit: {e}")))?;
            let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
            let commit_tree = commit
                .tree()
                .map_err(|e| LibraryError::Git(format!("path_dates commit tree: {e}")))?;

            let mut opts = git2::DiffOptions::new();
            if !strip_prefix.is_empty() {
                opts.pathspec(&strip_prefix);
            }
            let mut diff = repo
                .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(&mut opts))
                .map_err(|e| LibraryError::Git(format!("path_dates diff: {e}")))?;
            let mut find_opts = git2::DiffFindOptions::new();
            find_opts.renames(true);
            diff.find_similar(Some(&mut find_opts))
                .map_err(|e| LibraryError::Git(format!("path_dates find_similar: {e}")))?;

            let events: Vec<PathEvent> = diff
                .deltas()
                .filter_map(|delta| match delta.status() {
                    git2::Delta::Added | git2::Delta::Modified => {
                        delta.new_file().path().and_then(|p| p.to_str()).map(|p| {
                            PathEvent::Touched {
                                path: strip(&strip_prefix, p),
                            }
                        })
                    }
                    git2::Delta::Renamed => {
                        let from = delta.old_file().path().and_then(|p| p.to_str());
                        let to = delta.new_file().path().and_then(|p| p.to_str());
                        match (from, to) {
                            (Some(from), Some(to)) => Some(PathEvent::Renamed {
                                from: strip(&strip_prefix, from),
                                to: strip(&strip_prefix, to),
                            }),
                            _ => None,
                        }
                    }
                    git2::Delta::Deleted => {
                        delta.old_file().path().and_then(|p| p.to_str()).map(|p| {
                            PathEvent::Deleted {
                                path: strip(&strip_prefix, p),
                            }
                        })
                    }
                    _ => None,
                })
                .collect();
            if events.is_empty() {
                continue;
            }
            let at = DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0)
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            fold_path_dates(&mut map, at, events);
        }

        let dates = Arc::new(map);
        {
            let mut guard = cache.lock().expect("path_dates cache lock poisoned");
            guard.insert(
                prefix.to_string(),
                CachedPathDates {
                    head: head_oid,
                    dates: Arc::clone(&dates),
                },
            );
        }
        Ok(Some(dates))
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

    /// Current target of `refname` (e.g. `refs/heads/drua/<id>`).
    /// `Ok(None)` when the ref doesn't exist locally.
    #[tracing::instrument(name = "library.git.resolve_ref", skip_all, fields(%refname))]
    pub async fn resolve_ref(&self, refname: &str) -> Result<Option<String>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let refname = refname.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<String>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let result = match repo.find_reference(&refname) {
                Ok(r) => Ok(r.target().map(|oid| oid.to_string())),
                Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
                Err(e) => Err(LibraryError::Git(format!("resolve_ref: {e}"))),
            };
            result
        })
        .await
        .map_err(|e| LibraryError::Git(format!("resolve_ref join: {e}")))?
    }

    /// Points `refname` at `oid` locally (no push) — errors if it
    /// already exists or `oid` doesn't name a commit. Held under
    /// `repo_mutex` like every other ref mutation, matching the
    /// invariant on [`GitEngine::repo_mutex`]. Not pushed: a changeset
    /// branch with no commits yet has nothing for origin to hold, and
    /// gets recreated locally on demand (see `fetch_origin`'s prune
    /// comment).
    #[tracing::instrument(name = "library.git.create_ref", skip_all, fields(%refname, %oid))]
    pub async fn create_ref(&self, refname: &str, oid: &str) -> Result<(), LibraryError> {
        let _guard = self.repo_mutex.lock().await;
        let repo_path = self.repo_path.clone();
        let refname = refname.to_string();
        let oid = oid.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let oid = git2::Oid::from_str(&oid)
                .map_err(|e| LibraryError::Git(format!("parse oid: {e}")))?;
            repo.find_commit(oid)
                .map_err(|e| LibraryError::Git(format!("find commit: {e}")))?;
            repo.reference(&refname, oid, false, "create changeset ref")
                .map_err(|e| LibraryError::Git(format!("create ref: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| LibraryError::Git(format!("create_ref join: {e}")))?
    }

    /// Deletes `refname` locally, and on origin too when `push` is set
    /// (a no-op if it was never pushed). Held under `repo_mutex` like
    /// every other ref mutation.
    #[tracing::instrument(name = "library.git.delete_ref", skip_all, fields(%refname, %push))]
    pub async fn delete_ref(&self, refname: &str, push: bool) -> Result<(), LibraryError> {
        let _guard = self.repo_mutex.lock().await;
        let token = if push {
            Self::fresh_token(self.github_app.as_ref()).await
        } else {
            None
        };
        let repo_path = self.repo_path.clone();
        let refname = refname.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            if let Ok(mut r) = repo.find_reference(&refname) {
                r.delete()
                    .map_err(|e| LibraryError::Git(format!("delete ref: {e}")))?;
            }
            if push {
                let mut remote = repo
                    .find_remote("origin")
                    .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;
                let mut po = git2::PushOptions::new();
                po.remote_callbacks(Self::remote_callbacks(token.as_deref()));
                let spec = format!(":{refname}");
                remote
                    .push(&[spec], Some(&mut po))
                    .map_err(|e| LibraryError::Git(format!("push delete: {e}")))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| LibraryError::Git(format!("delete_ref join: {e}")))?
    }

    /// Best common ancestor of `a` and `b`. `Ok(None)` when they share
    /// no history.
    #[tracing::instrument(name = "library.git.merge_base", skip_all, fields(%a, %b))]
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let a = a.to_string();
        let b = b.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<String>, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let a_oid =
                git2::Oid::from_str(&a).map_err(|e| LibraryError::Git(format!("parse a: {e}")))?;
            let b_oid =
                git2::Oid::from_str(&b).map_err(|e| LibraryError::Git(format!("parse b: {e}")))?;
            match repo.merge_base(a_oid, b_oid) {
                Ok(oid) => Ok(Some(oid.to_string())),
                Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
                Err(e) => Err(LibraryError::Git(format!("merge_base: {e}"))),
            }
        })
        .await
        .map_err(|e| LibraryError::Git(format!("merge_base join: {e}")))?
    }

    /// Tree-level 3-way merge of `ours` onto `theirs` from `base`.
    /// `Ok(Ok(tree_oid))` on a clean merge; `Ok(Err(paths))` lists every
    /// conflicting path and leaves nothing committed. Mergeability is
    /// never stored — this is the "compute it on demand" primitive
    /// `Changesets::status` and `submit` call for their `mergeable` /
    /// `conflicts` fields (see handoff §2.2).
    #[tracing::instrument(name = "library.git.merge_trees", skip_all, fields(%base, %ours, %theirs))]
    pub async fn merge_trees(
        &self,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<Result<String, Vec<String>>, LibraryError> {
        let repo_path = self.repo_path.clone();
        let base = base.to_string();
        let ours = ours.to_string();
        let theirs = theirs.to_string();
        tokio::task::spawn_blocking(
            move || -> Result<Result<String, Vec<String>>, LibraryError> {
                let repo = git2::Repository::open_bare(&repo_path)
                    .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
                Self::merge_trees_blocking(&repo, &base, &ours, &theirs)
            },
        )
        .await
        .map_err(|e| LibraryError::Git(format!("merge_trees join: {e}")))?
    }

    /// Blocking body of [`Self::merge_trees`], factored out so unit
    /// tests can exercise it against a hand-built `git2::Repository`
    /// (mirrors `path_dates_blocking`).
    fn merge_trees_blocking(
        repo: &git2::Repository,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<Result<String, Vec<String>>, LibraryError> {
        let base_tree = Self::commit_tree(repo, base)?;
        let ours_tree = Self::commit_tree(repo, ours)?;
        let theirs_tree = Self::commit_tree(repo, theirs)?;
        let mut index = repo
            .merge_trees(&base_tree, &ours_tree, &theirs_tree, None)
            .map_err(|e| LibraryError::Git(format!("merge_trees: {e}")))?;
        if index.has_conflicts() {
            let mut paths: Vec<String> = Vec::new();
            for conflict in index
                .conflicts()
                .map_err(|e| LibraryError::Git(format!("conflicts: {e}")))?
            {
                let conflict = conflict.map_err(|e| LibraryError::Git(format!("conflict: {e}")))?;
                for entry in [conflict.ancestor, conflict.our, conflict.their]
                    .into_iter()
                    .flatten()
                {
                    let path = String::from_utf8_lossy(&entry.path).into_owned();
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            paths.sort();
            return Ok(Err(paths));
        }
        let tree_oid = index
            .write_tree_to(repo)
            .map_err(|e| LibraryError::Git(format!("write merged tree: {e}")))?;
        Ok(Ok(tree_oid.to_string()))
    }

    /// Merges `changeset_tip` into `main`: a 2-parent commit (`main`,
    /// `changeset_tip`) whose tree is the clean 3-way merge from their
    /// merge-base. Goes through the writer (`BatchOpKind::MergeCommit`)
    /// so it takes the same `repo_mutex`/advisory-lock/retry path as
    /// every other `main` write. `Err(Validation(_))` on conflicts —
    /// callers should `merge_trees` first if they want conflict paths
    /// without attempting the commit.
    #[tracing::instrument(name = "library.git.merge_into_main", skip_all, fields(%changeset_tip))]
    pub async fn merge_into_main(
        &self,
        changeset_tip: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<String, LibraryError> {
        let main_oid = self
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or_else(|| LibraryError::Git("merge_into_main: main has no target".into()))?;
        let base = self
            .merge_base(&main_oid, changeset_tip)
            .await?
            .ok_or_else(|| LibraryError::Git("merge_into_main: no merge base with main".into()))?;
        let tree_oid = match self.merge_trees(&base, changeset_tip, &main_oid).await? {
            Ok(oid) => oid,
            Err(paths) => {
                return Err(LibraryError::Validation(format!(
                    "merge conflicts: {}",
                    paths.join(", ")
                )))
            }
        };
        let oid = self
            .enqueue(BatchOp {
                commit_message: message,
                kind: BatchOpKind::MergeCommit {
                    tree_oid,
                    second_parent: changeset_tip.to_string(),
                },
                attribution,
                target_ref: None,
            })
            .await?;
        oid.ok_or_else(|| LibraryError::Git("merge_into_main: produced no commit".into()))
    }

    /// Force-rewrites `refname` to a single new commit: tree = the clean
    /// 3-way merge of `refname`'s current tip onto `onto` (ancestor =
    /// their merge-base), parent = `onto` alone. A squash: `refname`'s
    /// prior commits are discarded from the branch (not from history —
    /// they remain reachable via the old tip until GC); the PR body
    /// still lists the original ops. `Ok(Err(paths))` on conflict leaves
    /// `refname` untouched.
    #[tracing::instrument(name = "library.git.rebase_ref", skip_all, fields(%refname, %onto))]
    pub async fn rebase_ref(
        &self,
        refname: &str,
        onto: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<Result<(String, String), Vec<String>>, LibraryError> {
        let tip = self
            .resolve_ref(refname)
            .await?
            .ok_or_else(|| LibraryError::Git(format!("rebase_ref: {refname} not found")))?;
        let base = self.merge_base(&tip, onto).await?.ok_or_else(|| {
            LibraryError::Git(format!(
                "rebase_ref: no merge base between {refname} and {onto}"
            ))
        })?;
        let tree_oid = match self.merge_trees(&base, &tip, onto).await? {
            Ok(oid) => oid,
            Err(conflicts) => return Ok(Err(conflicts)),
        };

        let _guard = self.repo_mutex.lock().await;
        let token = Self::fresh_token(self.github_app.as_ref()).await;
        let repo_path = self.repo_path.clone();
        let refname_owned = refname.to_string();
        let onto_owned = onto.to_string();
        let new_head = tokio::task::spawn_blocking(move || -> Result<String, LibraryError> {
            let repo = git2::Repository::open_bare(&repo_path)
                .map_err(|e| LibraryError::Git(format!("open bare: {e}")))?;
            let onto_oid = git2::Oid::from_str(&onto_owned)
                .map_err(|e| LibraryError::Git(format!("parse onto: {e}")))?;
            let onto_commit = repo
                .find_commit(onto_oid)
                .map_err(|e| LibraryError::Git(format!("find onto commit: {e}")))?;
            let tree_oid = git2::Oid::from_str(&tree_oid)
                .map_err(|e| LibraryError::Git(format!("parse tree oid: {e}")))?;
            let tree = repo
                .find_tree(tree_oid)
                .map_err(|e| LibraryError::Git(format!("find tree: {e}")))?;
            let author = git2::Signature::now(&attribution.author_name, &attribution.author_email)
                .map_err(|e| LibraryError::Git(format!("author signature: {e}")))?;
            let committer =
                git2::Signature::now(&attribution.committer_name, &attribution.committer_email)
                    .map_err(|e| LibraryError::Git(format!("committer signature: {e}")))?;
            let mut full_message = message;
            full_message.push_str(&attribution.render_message_suffix());
            let new_oid = repo
                .commit(
                    Some(&refname_owned),
                    &author,
                    &committer,
                    &full_message,
                    &tree,
                    &[&onto_commit],
                )
                .map_err(|e| LibraryError::Git(format!("commit: {e}")))?;
            Self::push_ref_force(&repo, token.as_deref(), &refname_owned)?;
            Ok(new_oid.to_string())
        })
        .await
        .map_err(|e| LibraryError::Git(format!("rebase_ref join: {e}")))??;

        Ok(Ok((onto.to_string(), new_head)))
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
            target_ref: None,
        })
        .await
        .map(|_| ())
    }

    /// Ref-aware counterpart of [`Self::write_file`]: `target_ref`
    /// selects the branch to commit and push to (`None` =
    /// `refs/heads/main`, identical to `write_file`). Returns the
    /// resulting commit oid, or `None` if the tree was unchanged.
    #[tracing::instrument(name = "library.git.write_file_at", skip_all, fields(%path, target_ref = target_ref.as_deref().unwrap_or("refs/heads/main")))]
    pub async fn write_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        content: Vec<u8>,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Write { path, content },
            attribution,
            target_ref,
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
            target_ref: None,
        })
        .await
        .map(|_| ())
    }

    /// Ref-aware counterpart of [`Self::delete_file`]. See
    /// [`Self::write_file_at`] for the `target_ref` contract.
    #[tracing::instrument(name = "library.git.delete_file_at", skip_all, fields(%path, target_ref = target_ref.as_deref().unwrap_or("refs/heads/main")))]
    pub async fn delete_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Delete { path },
            attribution,
            target_ref,
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
            target_ref: None,
        })
        .await
        .map(|_| ())
    }

    /// Ref-aware counterpart of [`Self::update_file`]. See
    /// [`Self::write_file_at`] for the `target_ref` contract.
    #[tracing::instrument(name = "library.git.update_file_at", skip_all, fields(%path, target_ref = target_ref.as_deref().unwrap_or("refs/heads/main")))]
    pub async fn update_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        update: BatchRmwFn,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Rmw { path, update },
            attribution,
            target_ref,
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
            target_ref: None,
        })
        .await
        .map(|_| ())
    }

    /// Ref-aware counterpart of [`Self::move_file`]. See
    /// [`Self::write_file_at`] for the `target_ref` contract.
    #[tracing::instrument(name = "library.git.move_file_at", skip_all, fields(%from, %to, target_ref = target_ref.as_deref().unwrap_or("refs/heads/main")))]
    pub async fn move_file_at(
        &self,
        target_ref: Option<String>,
        from: String,
        to: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.enqueue(BatchOp {
            commit_message,
            kind: BatchOpKind::Move { from, to },
            attribution,
            target_ref,
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
            target_ref: None,
        })
        .await
        .map(|_| ())
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
            target_ref: None,
        })
        .await
        .map(|_| ())
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
            target_ref: None,
        })
        .await
        .map(|_| ())
    }

    /// Push a single op onto the writer queue and await its result: the
    /// resulting commit oid, or `None` if the tree was unchanged.
    /// Failure to enqueue (writer task gone) or to receive the response
    /// (response channel closed) collapses to a `Git(_)` error.
    async fn enqueue(&self, op: BatchOp) -> WriteResult {
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
        commit_notify: Arc<Notify>,
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
                commit_notify.notify_one();
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
        // race the remote into non-ff retries / lost commits.
        //
        // SESSION-scoped (not `xact`): the git fetch/commit/push below runs
        // with no open transaction, so `idle_in_transaction_session_timeout`
        // can't reap it and drop the lock mid-push. Released explicitly after;
        // a crashed/closed connection releases it server-side too. Best effort:
        // a DB hiccup degrades to the prior unlocked behavior rather than
        // wedging all library writes.
        let mut lock_conn = match pool.acquire().await {
            Ok(mut conn) => match sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(LIBRARY_PUSH_LOCK_KEY)
                .execute(&mut *conn)
                .await
            {
                Ok(_) => Some(conn),
                Err(e) => {
                    tracing::warn!(error = %e, "library push lock: acquire failed; proceeding unlocked");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "library push lock: connection failed; proceeding unlocked");
                None
            }
        };
        let token = Self::fresh_token(github_app).await;
        let path = repo_path.to_path_buf();
        let n = batch.len();
        let (ops, responders): (Vec<BatchOp>, Vec<oneshot::Sender<WriteResult>>) =
            batch.into_iter().map(|q| (q.op, q.response)).unzip();

        let results = tokio::task::spawn_blocking(move || -> Vec<WriteResult> {
            Self::commit_each_then_push_blocking(&path, ops, token.as_deref())
        })
        .await
        .unwrap_or_else(|e| {
            let msg = format!("commit_each_then_push join: {e}");
            (0..n)
                .map(|_| Err(LibraryError::Git(msg.clone())))
                .collect()
        });

        let any_ok = results.iter().any(|r| r.is_ok());
        if any_ok {
            Self::notify_cluster_push(pool, lock_conn.as_deref_mut()).await;
        }

        if let Some(mut conn) = lock_conn.take() {
            if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(LIBRARY_PUSH_LOCK_KEY)
                .execute(&mut *conn)
                .await
            {
                tracing::warn!(error = %e, "library push lock: release failed");
            }
        }
        for (resp, res) in responders.into_iter().zip(results) {
            let _ = resp.send(res);
        }
        any_ok
    }

    /// Wake peer replicas' fetchers after a successful push so
    /// cross-replica reads converge in milliseconds instead of after
    /// each replica's fetch ticker. Best effort: failure degrades to
    /// ticker-cadence convergence. Prefers the advisory-lock connection
    /// (already held) over a fresh pool checkout.
    async fn notify_cluster_push(pool: &PgPool, lock_conn: Option<&mut PgConnection>) {
        let res = match lock_conn {
            Some(conn) => sqlx::query("SELECT pg_notify($1, '')")
                .bind(LIBRARY_HEAD_NOTIFY_CHANNEL)
                .execute(conn)
                .await
                .map(|_| ()),
            None => sqlx::query("SELECT pg_notify($1, '')")
                .bind(LIBRARY_HEAD_NOTIFY_CHANNEL)
                .execute(pool)
                .await
                .map(|_| ()),
        };
        if let Err(e) = res {
            tracing::warn!(error = %e, "library head notify failed; peers converge on ticker");
        }
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
    ) -> Vec<WriteResult> {
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

        // Group by target ref, preserving each group's relative order.
        // Groups are committed and pushed independently (a conflict or
        // non-FF retry on one ref never touches another), so grouping
        // order doesn't matter for correctness — only within-group order
        // does, which this preserves.
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, op) in ops.iter().enumerate() {
            let refname = Self::ref_name(op.target_ref.as_deref());
            groups
                .entry(refname.clone())
                .or_insert_with(|| {
                    order.push(refname.clone());
                    Vec::new()
                })
                .push(i);
        }

        let mut results: Vec<Option<WriteResult>> = (0..ops.len()).map(|_| None).collect();
        for refname in order {
            let indices = groups.remove(&refname).expect("just inserted above");
            let group_ops: Vec<&BatchOp> = indices.iter().map(|&i| &ops[i]).collect();
            let group_results =
                Self::commit_group_then_push_blocking(&repo, &refname, &group_ops, token);
            for (idx, res) in indices.into_iter().zip(group_results) {
                results[idx] = Some(res);
            }
        }
        results
            .into_iter()
            .map(|r| r.expect("every op index assigned by its group"))
            .collect()
    }

    /// `refs/heads/<name>` a `target_ref` resolves to. `None` = `main`.
    fn ref_name(target_ref: Option<&str>) -> String {
        target_ref.unwrap_or("refs/heads/main").to_string()
    }

    /// Current tip of `refname`. `refs/heads/main` resolves via `HEAD`
    /// (matching the pre-changesets behaviour exactly); any other ref is
    /// looked up directly, since HEAD never points anywhere else.
    fn ref_oid(repo: &git2::Repository, refname: &str) -> Result<git2::Oid, LibraryError> {
        if refname == "refs/heads/main" {
            repo.head()
                .and_then(|r| r.peel_to_commit())
                .map(|c| c.id())
                .map_err(|e| LibraryError::Git(format!("head: {e}")))
        } else {
            repo.find_reference(refname)
                .and_then(|r| r.peel_to_commit())
                .map(|c| c.id())
                .map_err(|e| LibraryError::Git(format!("resolve {refname}: {e}")))
        }
    }

    /// One group's worth of [`Self::commit_each_then_push_blocking`]:
    /// apply `ops` as N commits on `refname`, then push once. On non-FF
    /// push, refetch and replay against the new tip; a second push
    /// failure rolls `refname` back to its pre-group oid and surfaces
    /// `Git("push failed: ...")` on every op that committed locally.
    /// Per-op `Validation` errors don't advance the parent — the next op
    /// layers on the previous successful commit.
    fn commit_group_then_push_blocking(
        repo: &git2::Repository,
        refname: &str,
        ops: &[&BatchOp],
        token: Option<&str>,
    ) -> Vec<WriteResult> {
        const MAX_ATTEMPTS: u32 = 2;
        let update_ref = if refname == "refs/heads/main" {
            "HEAD"
        } else {
            refname
        };

        let initial_oid = match Self::ref_oid(repo, refname) {
            Ok(oid) => oid,
            Err(e) => {
                let msg = e.to_string();
                return ops
                    .iter()
                    .map(|_| Err(LibraryError::Git(msg.clone())))
                    .collect();
            }
        };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let parent_oid_at_attempt_start = match Self::ref_oid(repo, refname) {
                Ok(oid) => oid,
                Err(e) => {
                    let msg = e.to_string();
                    return ops
                        .iter()
                        .map(|_| Err(LibraryError::Git(msg.clone())))
                        .collect();
                }
            };
            let mut current_parent_oid = parent_oid_at_attempt_start;
            let mut per_op: Vec<WriteResult> = Vec::with_capacity(ops.len());
            for op in ops {
                match Self::commit_one(repo, update_ref, current_parent_oid, op) {
                    Ok(Some(new_oid)) => {
                        per_op.push(Ok(Some(new_oid.to_string())));
                        current_parent_oid = new_oid;
                    }
                    Ok(None) => per_op.push(Ok(None)),
                    Err(e) => per_op.push(Err(e)),
                }
            }

            if current_parent_oid == parent_oid_at_attempt_start {
                return per_op;
            }

            match Self::push_ref(repo, token, refname) {
                Ok(()) => return per_op,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::info!(
                        error = %e, attempt, %refname,
                        "commit_group_then_push: push failed, refetching and retrying"
                    );
                    if let Err(fe) = Self::fetch_origin(repo, token) {
                        let msg = fe.to_string();
                        return ops
                            .iter()
                            .map(|_| Err(LibraryError::Git(msg.clone())))
                            .collect();
                    }
                    // `main` needs an explicit reset from
                    // `refs/remotes/origin/main` (see `reset_main_to_origin`).
                    // Every other ref is written directly by
                    // `fetch_origin`'s mirror refspec, so it already
                    // reflects origin — nothing further to reset.
                    if refname == "refs/heads/main" {
                        if let Err(re) = Self::reset_main_to_origin(repo) {
                            let msg = re.to_string();
                            return ops
                                .iter()
                                .map(|_| Err(LibraryError::Git(msg.clone())))
                                .collect();
                        }
                    }
                }
                Err(e) => {
                    let _ =
                        repo.reference(refname, initial_oid, true, "rollback after push failure");
                    let msg = format!("push failed: {e}");
                    return per_op
                        .into_iter()
                        .map(|r| match r {
                            Ok(_) => Err(LibraryError::Git(msg.clone())),
                            Err(e) => Err(e),
                        })
                        .collect();
                }
            }
        }
    }

    /// One commit step inside [`Self::commit_group_then_push_blocking`].
    /// `update_ref` is passed straight to `git2::Repository::commit` —
    /// `"HEAD"` for the `main` group (matching pre-changesets behaviour
    /// exactly), the literal `refs/heads/drua/<id>` for any other group,
    /// since `HEAD` never points anywhere but `main`.
    /// `Ok(Some(oid))` = real commit; `Ok(None)` = tree unchanged;
    /// `Err(_)` = per-op validation or git failure (skip, don't advance).
    fn commit_one(
        repo: &git2::Repository,
        update_ref: &str,
        parent_oid: git2::Oid,
        op: &BatchOp,
    ) -> Result<Option<git2::Oid>, LibraryError> {
        let parent_commit = repo
            .find_commit(parent_oid)
            .map_err(|e| LibraryError::Git(format!("find parent commit: {e}")))?;
        let parent_tree = parent_commit
            .tree()
            .map_err(|e| LibraryError::Git(format!("parent tree: {e}")))?;

        let mut second_parent: Option<git2::Commit> = None;

        let new_tree_oid = match &op.kind {
            BatchOpKind::Write { path, content } => {
                Self::apply_edit(repo, &parent_tree, path, Some(content.clone()))?
            }
            BatchOpKind::Delete { path } => match Self::blob_in_tree(repo, &parent_tree, path)? {
                Some(_) => Self::apply_edit(repo, &parent_tree, path, None)?,
                None => parent_tree.id(),
            },
            BatchOpKind::Rmw { path, update } => {
                let current = Self::blob_in_tree(repo, &parent_tree, path)?;
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
                    Self::blob_in_tree(repo, &parent_tree, from)?.ok_or_else(|| {
                        LibraryError::Validation(format!("move: src does not exist: {from}"))
                    })?;
                if Self::blob_in_tree(repo, &parent_tree, to)?.is_some() {
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
            BatchOpKind::MergeCommit {
                tree_oid,
                second_parent: sp,
            } => {
                let sp_oid = git2::Oid::from_str(sp)
                    .map_err(|e| LibraryError::Git(format!("parse second parent: {e}")))?;
                let sp_commit = repo
                    .find_commit(sp_oid)
                    .map_err(|e| LibraryError::Git(format!("find second parent: {e}")))?;
                second_parent = Some(sp_commit);
                git2::Oid::from_str(tree_oid)
                    .map_err(|e| LibraryError::Git(format!("parse tree oid: {e}")))?
            }
        };

        // A merge commit is real even when its tree matches the parent's
        // (an "already contains everything" merge) — it still records
        // the second parent, so it never no-ops.
        if second_parent.is_none() && new_tree_oid == parent_tree.id() {
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
        let parents: Vec<&git2::Commit> = match &second_parent {
            Some(sp) => vec![&parent_commit, sp],
            None => vec![&parent_commit],
        };
        let commit_oid = repo
            .commit(
                Some(update_ref),
                &author,
                &committer,
                &message,
                &new_tree,
                &parents,
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

    fn blob_in_tree(
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

    /// Fast-forward push of `refname` (used for every normal write,
    /// `main` included — `refs/heads/main:refs/heads/main` is exactly
    /// what the pre-changesets `push_main` sent).
    fn push_ref(
        repo: &git2::Repository,
        token: Option<&str>,
        refname: &str,
    ) -> Result<(), LibraryError> {
        let mut remote = repo
            .find_remote("origin")
            .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;
        let mut po = git2::PushOptions::new();
        po.remote_callbacks(Self::remote_callbacks(token));
        let spec = format!("{refname}:{refname}");
        remote
            .push(&[spec], Some(&mut po))
            .map_err(|e| LibraryError::Git(format!("push: {e}")))
    }

    /// Force push of `refname` — used only by [`Self::rebase_ref`],
    /// whose squash commit deliberately isn't a fast-forward of the
    /// branch's prior tip.
    fn push_ref_force(
        repo: &git2::Repository,
        token: Option<&str>,
        refname: &str,
    ) -> Result<(), LibraryError> {
        let mut remote = repo
            .find_remote("origin")
            .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;
        let mut po = git2::PushOptions::new();
        po.remote_callbacks(Self::remote_callbacks(token));
        let spec = format!("+{refname}:{refname}");
        remote
            .push(&[spec], Some(&mut po))
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
        // Prune local `refs/heads/*` absent from origin's fetched set —
        // needed so a changeset branch's local ref actually disappears
        // once its PR closes and GitHub deletes the branch (the sync job
        // reads that disappearance as `resolve_ref == None` to mark the
        // changeset `Abandoned`; see `job::sync`). This mirror refspec's
        // destination is `refs/heads/*` (not the usual
        // `refs/remotes/origin/*`), so prune here operates over local
        // branches, not remote-tracking ones — a real behaviour change
        // from "off" (the prior default), not a no-op tweak.
        //
        // Safe for a *local-only, not-yet-pushed* `drua/<id>` branch
        // (created by `create_ref` before its first write): pruning it
        // loses nothing, because it carries no commits origin doesn't
        // already have (it's sitting at `base_oid`), and `Changesets`
        // already treats a missing local ref as recoverable —
        // `ensure_ref` recreates it on next use, the same repair path a
        // pod restart (ephemeral clone) requires anyway. Once a
        // changeset receives its first write, the writer pushes before
        // releasing `repo_mutex` (see the field doc on `GitEngine`), so
        // from that point its ref always exists on origin and prune
        // cannot remove real content.
        fo.prune(git2::FetchPrune::On);

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
        message: &str,
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
        repo.commit(None, &sig, &sig, message, &tree, &parent_refs)
            .unwrap()
    }

    fn projection_msg() -> String {
        format!(
            "msg\n\n{}: true",
            crate::attribution::PROJECTION_TRAILER_KEY
        )
    }

    const LIB_BOT: &str = crate::attribution::LIBRARY_BOT_EMAIL;

    #[test]
    fn external_deltas_skips_projections_keeps_authoring() {
        let dir = unique_dir("echo");
        let repo = git2::Repository::init_bare(&dir).unwrap();

        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com", "init");
        // drua forward-sync projection (committer drua + trailer) → dropped.
        let c1 = commit_file(&repo, Some(c0), "a.yml", b"v1", LIB_BOT, &projection_msg());
        // drua authoring write (e.g. `spaces edit`, committer drua, NO trailer)
        // → kept, so space-authored docs still import.
        let c2 = commit_file(&repo, Some(c1), "note.md", b"hi", LIB_BOT, "spaces: edit");

        let deltas =
            GitEngine::external_deltas(&repo, Some(&c0.to_string()), &c2.to_string()).unwrap();

        let paths: std::collections::BTreeSet<&str> =
            deltas.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            ["note.md"].into_iter().collect(),
            "projection dropped, authoring kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_deltas_ignores_forged_projection_trailer() {
        let dir = unique_dir("forge");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com", "init");
        // Non-drua committer carrying the trailer must NOT be suppressed.
        let c1 = commit_file(
            &repo,
            Some(c0),
            "b.yml",
            b"b",
            "attacker@example.com",
            &projection_msg(),
        );

        let deltas =
            GitEngine::external_deltas(&repo, Some(&c0.to_string()), &c1.to_string()).unwrap();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].path, "b.yml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_deltas_snapshots_without_checkpoint() {
        let dir = unique_dir("snapshot");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        // Even a projection commit is desired state on initial snapshot.
        let c0 = commit_file(&repo, None, "a.yml", b"v0", LIB_BOT, &projection_msg());

        let deltas = GitEngine::external_deltas(&repo, None, &c0.to_string()).unwrap();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].path, "a.yml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_deltas_force_push_bounds_by_merge_base_and_filters_projections() {
        let dir = unique_dir("forcepush");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com", "init");
        // New `main` since divergence: a drua projection then an external edit.
        let c1 = commit_file(
            &repo,
            Some(c0),
            "proj.yml",
            b"p",
            LIB_BOT,
            &projection_msg(),
        );
        let to = commit_file(
            &repo,
            Some(c1),
            "ext.yml",
            b"e",
            "human@example.com",
            "edit",
        );
        // Stale checkpoint on an abandoned branch — not an ancestor of `to`.
        let stale_from = commit_file(&repo, Some(c0), "gone.yml", b"g", "human@example.com", "y");

        let deltas =
            GitEngine::external_deltas(&repo, Some(&stale_from.to_string()), &to.to_string())
                .unwrap();

        // Walk is bounded by the merge-base (c0), so only the new-main commits
        // are considered; the projection is still filtered and the abandoned
        // branch's file is not replayed.
        let paths: std::collections::BTreeSet<&str> =
            deltas.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["ext.yml"].into_iter().collect());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_delta_carries_removed_blob() {
        let dir = unique_dir("del");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let c0 = commit_file(
            &repo,
            None,
            "a.yml",
            b"removed-bytes",
            "human@example.com",
            "init",
        );
        // c1 removes a.yml (external human commit → empty tree).
        let c1 = {
            let tb = repo.treebuilder(None).unwrap();
            let tree = repo.find_tree(tb.write().unwrap()).unwrap();
            let sig = git2::Signature::now("tester", "human@example.com").unwrap();
            let parent = repo.find_commit(c0).unwrap();
            repo.commit(None, &sig, &sig, "rm", &tree, &[&parent])
                .unwrap()
        };

        let deltas =
            GitEngine::external_deltas(&repo, Some(&c0.to_string()), &c1.to_string()).unwrap();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].path, "a.yml");
        assert!(matches!(deltas[0].kind, DeltaKind::Deleted));
        // The removed blob is carried so the importer can do an id-aware delete.
        assert_eq!(deltas[0].content, b"removed-bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_deltas_fails_on_missing_checkpoint() {
        let dir = unique_dir("missing");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let c0 = commit_file(&repo, None, "a.yml", b"v0", "human@example.com", "init");
        // Checkpoint OID that doesn't exist in the repo (GC'd / force-pushed
        // away). Must error so the tick keeps the checkpoint instead of
        // silently resyncing from an empty tree and dropping deletions.
        let missing = "1111111111111111111111111111111111111111";
        let res = GitEngine::external_deltas(&repo, Some(missing), &c0.to_string());
        assert!(res.is_err(), "missing checkpoint must fail the tick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bare repo with `spaces/s/README.md` + `spaces/s/research/a.md`,
    /// committed on HEAD so the `*_at_head` readers can see it.
    fn repo_with_space_tree(tag: &str) -> (PathBuf, git2::Repository) {
        let dir = unique_dir(tag);
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let mut idx = repo.index().unwrap();
        for (path, content) in [
            ("spaces/s/README.md", &b"# s\nobix here\n"[..]),
            ("spaces/s/research/a.md", &b"deeper obix\n"[..]),
        ] {
            let oid = repo.blob(content).unwrap();
            let mut entry = index_entry(path);
            entry.id = oid;
            entry.file_size = content.len() as u32;
            idx.add(&entry).unwrap();
        }
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        (dir, repo)
    }

    fn index_entry(path: &str) -> git2::IndexEntry {
        git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: git2::Oid::zero(),
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn walk_blobs_at_dir_walks_recursively() {
        let (dir, repo) = repo_with_space_tree("walk-dir");
        let blobs = GitEngine::blobs_at_head(&repo, "spaces/s")
            .unwrap()
            .unwrap();
        let paths: std::collections::BTreeSet<&str> =
            blobs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["spaces/s/README.md", "spaces/s/research/a.md"]
                .into_iter()
                .collect()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_blobs_at_file_yields_that_file() {
        let (dir, repo) = repo_with_space_tree("walk-file");
        let blobs = GitEngine::blobs_at_head(&repo, "spaces/s/README.md")
            .unwrap()
            .unwrap();
        assert_eq!(blobs.len(), 1, "a file path scopes the walk to that file");
        assert_eq!(blobs[0].0, "spaces/s/README.md");
        assert_eq!(blobs[0].1, b"# s\nobix here\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_blobs_at_missing_path_is_none() {
        let (dir, repo) = repo_with_space_tree("walk-missing");
        // `None`, not an empty Vec: callers surface it as an error instead
        // of a silent "no matches".
        assert!(GitEngine::blobs_at_head(&repo, "spaces/s/nope.md")
            .unwrap()
            .is_none());
        assert!(GitEngine::blobs_at_head(&repo, "spaces/other")
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_blobs_at_unborn_head_is_none() {
        let dir = unique_dir("walk-unborn");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        assert!(GitEngine::blobs_at_head(&repo, "").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn fold_touch_touch_keeps_first_created() {
        let mut map = PathDatesMap::new();
        fold_path_dates(
            &mut map,
            t(100),
            vec![PathEvent::Touched {
                path: "a.md".into(),
            }],
        );
        fold_path_dates(
            &mut map,
            t(200),
            vec![PathEvent::Touched {
                path: "a.md".into(),
            }],
        );
        let dates = map["a.md"];
        assert_eq!(dates.created, t(100));
        assert_eq!(dates.modified, t(200));
    }

    #[test]
    fn fold_rename_preserves_created_moves_modified() {
        let mut map = PathDatesMap::new();
        fold_path_dates(
            &mut map,
            t(100),
            vec![PathEvent::Touched {
                path: "old.md".into(),
            }],
        );
        fold_path_dates(
            &mut map,
            t(300),
            vec![PathEvent::Renamed {
                from: "old.md".into(),
                to: "new.md".into(),
            }],
        );
        assert!(!map.contains_key("old.md"));
        let dates = map["new.md"];
        assert_eq!(dates.created, t(100));
        assert_eq!(dates.modified, t(300));
    }

    #[test]
    fn fold_delete_then_readd_restarts_created() {
        let mut map = PathDatesMap::new();
        fold_path_dates(
            &mut map,
            t(100),
            vec![PathEvent::Touched {
                path: "a.md".into(),
            }],
        );
        fold_path_dates(
            &mut map,
            t(200),
            vec![PathEvent::Deleted {
                path: "a.md".into(),
            }],
        );
        assert!(!map.contains_key("a.md"));
        fold_path_dates(
            &mut map,
            t(300),
            vec![PathEvent::Touched {
                path: "a.md".into(),
            }],
        );
        let dates = map["a.md"];
        assert_eq!(dates.created, t(300));
        assert_eq!(dates.modified, t(300));
    }

    #[test]
    fn fold_rename_of_unseen_source_uses_commit_time() {
        let mut map = PathDatesMap::new();
        fold_path_dates(
            &mut map,
            t(400),
            vec![PathEvent::Renamed {
                from: "ghost.md".into(),
                to: "new.md".into(),
            }],
        );
        let dates = map["new.md"];
        assert_eq!(dates.created, t(400));
        assert_eq!(dates.modified, t(400));
    }

    #[test]
    fn normalize_prefix_adds_trailing_slash() {
        assert_eq!(normalize_prefix("spaces/s"), "spaces/s/");
        assert_eq!(normalize_prefix("spaces/s/"), "spaces/s/");
        assert_eq!(normalize_prefix(""), "");
    }

    #[test]
    fn strip_removes_normalized_prefix() {
        assert_eq!(strip("spaces/s/", "spaces/s/a.md"), "a.md");
        assert_eq!(
            strip("spaces/s/", "spaces/s/research/a.md"),
            "research/a.md"
        );
        // Defensive fallback when the path doesn't carry the prefix.
        assert_eq!(strip("spaces/s/", "elsewhere/a.md"), "elsewhere/a.md");
    }

    /// Rename tracking itself is exercised end-to-end by the
    /// `move_keeps_created` integration test (`space_path_dates.rs`),
    /// against the production `Spaces::move_file` commit shape. This
    /// covers the plainer diff/revwalk mechanics: `created` pins to
    /// the first commit that touches a path, `modified` follows the
    /// last.
    #[test]
    fn path_dates_blocking_pins_created_follows_modified() {
        let dir = unique_dir("pd-happy");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let cache: std::sync::Mutex<HashMap<String, CachedPathDates>> =
            std::sync::Mutex::new(HashMap::new());

        // `commit_file`'s treebuilder only supports single-level names,
        // so this exercises the fold/revwalk mechanics at the repo
        // root (prefix ""); `normalize_prefix`/`strip` above cover the
        // `spaces/<slug>/` scoping in isolation, and the DB-backed
        // `space_path_dates.rs` integration tests exercise the real
        // `spaces/<slug>/...` layout end to end.
        let c0 = commit_file(&repo, None, "a.md", b"v0", "human@example.com", "add a.md");
        let c1 = commit_file(
            &repo,
            Some(c0),
            "a.md",
            b"v1",
            "human@example.com",
            "edit a.md",
        );
        repo.set_head_detached(c1).unwrap();

        let dates = GitEngine::path_dates_blocking(&repo, "", &cache)
            .unwrap()
            .unwrap();
        let a = dates["a.md"];
        assert_eq!(a.created, a_time(&repo, c0));
        assert_eq!(a.modified, a_time(&repo, c1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Simulates the hidden-commit caveat (§2.2 of the handoff): a
    /// cached entry whose HEAD is no longer an ancestor of the current
    /// HEAD — e.g. a force-push, or `main` reset sideways — must be
    /// discarded rather than folded onto, which would silently
    /// attribute dates to a disconnected base.
    #[test]
    fn path_dates_blocking_discards_cache_when_stale_head_not_ancestor() {
        let dir = unique_dir("pd-nonancestor");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let cache: std::sync::Mutex<HashMap<String, CachedPathDates>> =
            std::sync::Mutex::new(HashMap::new());

        // History A: c0 -> c1. Populate the cache at HEAD = c1.
        let c0 = commit_file(&repo, None, "a.md", b"v0", "human@example.com", "add a.md");
        let c1 = commit_file(
            &repo,
            Some(c0),
            "b.md",
            b"v0",
            "human@example.com",
            "add b.md",
        );
        repo.set_head_detached(c1).unwrap();
        let first = GitEngine::path_dates_blocking(&repo, "", &cache)
            .unwrap()
            .unwrap();
        assert!(first.contains_key("a.md"));
        assert!(first.contains_key("b.md"));

        // History B: an unrelated root commit. HEAD moves sideways —
        // c1 is not an ancestor of it (equivalent to an abandoned
        // branch after a force-push / reset).
        let d0 = commit_file(
            &repo,
            None,
            "c.md",
            b"v0",
            "human@example.com",
            "unrelated root",
        );
        repo.set_head_detached(d0).unwrap();
        assert!(!repo.graph_descendant_of(d0, c1).unwrap());

        let second = GitEngine::path_dates_blocking(&repo, "", &cache)
            .unwrap()
            .unwrap();
        assert!(second.contains_key("c.md"));
        assert!(
            !second.contains_key("a.md") && !second.contains_key("b.md"),
            "folding onto the disconnected stale base must not resurrect history A: {second:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn a_time(repo: &git2::Repository, oid: git2::Oid) -> DateTime<Utc> {
        let commit = repo.find_commit(oid).unwrap();
        DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0).unwrap()
    }

    // --- changesets: target_ref, grouped batches, merges, prune ---

    /// A bare "origin" with one commit on `main`, plus a bare clone of it
    /// (mirrors production: `GitEngine` only ever operates on a bare
    /// clone with a remote named `origin`). No credentials are needed —
    /// both sides are local filesystem paths.
    fn origin_and_clone(tag: &str) -> (PathBuf, PathBuf, git2::Repository) {
        let origin_dir = unique_dir(&format!("{tag}-origin"));
        {
            let origin = git2::Repository::init_bare(&origin_dir).unwrap();
            let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
            let tree = origin
                .find_tree(origin.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            origin
                .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
                .unwrap();
            origin.set_head("refs/heads/main").unwrap();
        }

        let local_dir = unique_dir(&format!("{tag}-local"));
        let local_repo = git2::build::RepoBuilder::new()
            .bare(true)
            .clone(&origin_dir.to_string_lossy(), &local_dir)
            .unwrap();
        (origin_dir, local_dir, local_repo)
    }

    #[test]
    fn write_to_drua_ref_leaves_main_untouched_and_visible_at_tip() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("write-ref");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        local_repo
            .reference("refs/heads/drua/x", main_oid, false, "test create")
            .unwrap();

        let op = BatchOp {
            commit_message: "changeset: add file".into(),
            kind: BatchOpKind::Write {
                path: "note.md".into(),
                content: b"hi".to_vec(),
            },
            attribution: CommitAttribution::library_default(),
            target_ref: Some("refs/heads/drua/x".into()),
        };
        let mut results = GitEngine::commit_each_then_push_blocking(&local_dir, vec![op], None);
        let new_oid = results.remove(0).unwrap().expect("real commit");

        let origin_repo = git2::Repository::open_bare(&origin_dir).unwrap();
        let origin_main = origin_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(
            origin_main, main_oid,
            "main on origin must be untouched by a drua/* write"
        );

        let origin_branch = origin_repo
            .find_reference("refs/heads/drua/x")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(origin_branch.to_string(), new_oid);

        // read at the changeset tip sees the new file; read at main (HEAD)
        // does not.
        let tip_tree = origin_repo
            .find_commit(origin_branch)
            .unwrap()
            .tree()
            .unwrap();
        assert_eq!(
            GitEngine::blob_in_tree(&origin_repo, &tip_tree, "note.md").unwrap(),
            Some(b"hi".to_vec())
        );
        let main_tree = origin_repo
            .find_commit(origin_main)
            .unwrap()
            .tree()
            .unwrap();
        assert!(GitEngine::blob_in_tree(&origin_repo, &main_tree, "note.md")
            .unwrap()
            .is_none());

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    #[test]
    fn two_refs_in_one_batch_push_independently() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("two-refs");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        local_repo
            .reference("refs/heads/drua/y", main_oid, false, "test create")
            .unwrap();

        let op_main = BatchOp {
            commit_message: "main: add a".into(),
            kind: BatchOpKind::Write {
                path: "a.md".into(),
                content: b"a".to_vec(),
            },
            attribution: CommitAttribution::library_default(),
            target_ref: None,
        };
        let op_branch = BatchOp {
            commit_message: "changeset: add b".into(),
            kind: BatchOpKind::Write {
                path: "b.md".into(),
                content: b"b".to_vec(),
            },
            attribution: CommitAttribution::library_default(),
            target_ref: Some("refs/heads/drua/y".into()),
        };
        let results =
            GitEngine::commit_each_then_push_blocking(&local_dir, vec![op_main, op_branch], None);
        assert_eq!(results.len(), 2);
        assert!(results[0].as_ref().unwrap().is_some(), "main op committed");
        assert!(
            results[1].as_ref().unwrap().is_some(),
            "branch op committed"
        );

        let origin_repo = git2::Repository::open_bare(&origin_dir).unwrap();
        let main_tree = origin_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_tree()
            .unwrap();
        assert!(
            main_tree.get_path(Path::new("a.md")).is_ok(),
            "main got its own op"
        );
        assert!(
            main_tree.get_path(Path::new("b.md")).is_err(),
            "main must not see the branch's op"
        );

        let branch_tree = origin_repo
            .find_reference("refs/heads/drua/y")
            .unwrap()
            .peel_to_tree()
            .unwrap();
        assert!(
            branch_tree.get_path(Path::new("b.md")).is_ok(),
            "branch got its own op"
        );
        assert!(
            branch_tree.get_path(Path::new("a.md")).is_err(),
            "branch must not see main's op"
        );

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    #[test]
    fn non_ff_push_to_drua_ref_replays_after_fetch() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("non-ff");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        local_repo
            .reference("refs/heads/drua/z", main_oid, false, "test create")
            .unwrap();
        GitEngine::push_ref(&local_repo, None, "refs/heads/drua/z").unwrap();

        // Simulate a human pushing directly to origin: a second commit on
        // `drua/z` the local clone has never seen. The local clone's
        // `drua/z` ref is now stale at `main_oid`.
        let origin_repo = git2::Repository::open_bare(&origin_dir).unwrap();
        let external_oid = commit_file(
            &origin_repo,
            Some(main_oid),
            "external.md",
            b"human",
            "human@example.com",
            "human push",
        );
        origin_repo
            .reference(
                "refs/heads/drua/z",
                external_oid,
                true,
                "simulate human push",
            )
            .unwrap();

        let op = BatchOp {
            commit_message: "changeset: add x".into(),
            kind: BatchOpKind::Write {
                path: "x.md".into(),
                content: b"x".to_vec(),
            },
            attribution: CommitAttribution::library_default(),
            target_ref: Some("refs/heads/drua/z".into()),
        };
        let mut results = GitEngine::commit_each_then_push_blocking(&local_dir, vec![op], None);
        let new_oid = results
            .remove(0)
            .unwrap()
            .expect("real commit after replay");

        let tip = origin_repo
            .find_reference("refs/heads/drua/z")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(tip.to_string(), new_oid);
        let tip_commit = origin_repo.find_commit(tip).unwrap();
        // The replayed commit's parent is the external human commit, not
        // the stale oid the write started from — proving the retry
        // re-fetched and replayed on the new tip instead of force-pushing
        // over it.
        assert_eq!(tip_commit.parent_id(0).unwrap(), external_oid);
        let tree = tip_commit.tree().unwrap();
        assert!(
            tree.get_path(Path::new("external.md")).is_ok(),
            "human's file survives the replay"
        );
        assert!(
            tree.get_path(Path::new("x.md")).is_ok(),
            "replayed write lands too"
        );

        let main_after = origin_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(main_after, main_oid, "main untouched throughout");

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    #[test]
    fn merge_trees_blocking_reports_conflict_paths() {
        let dir = unique_dir("merge-conflict");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let base = commit_file(&repo, None, "a.md", b"base\n", "human@example.com", "base");
        let ours = commit_file(
            &repo,
            Some(base),
            "a.md",
            b"ours\n",
            "human@example.com",
            "ours edits a",
        );
        let theirs = commit_file(
            &repo,
            Some(base),
            "a.md",
            b"theirs\n",
            "human@example.com",
            "theirs edits a",
        );

        let result = GitEngine::merge_trees_blocking(
            &repo,
            &base.to_string(),
            &ours.to_string(),
            &theirs.to_string(),
        )
        .unwrap();
        let conflicts = result.expect_err("both sides edited a.md");
        assert_eq!(conflicts, vec!["a.md".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_trees_blocking_clean_merge_combines_disjoint_edits() {
        let dir = unique_dir("merge-clean");
        let repo = git2::Repository::init_bare(&dir).unwrap();
        let base = commit_file(&repo, None, "a.md", b"base\n", "human@example.com", "base");
        let ours = commit_file(
            &repo,
            Some(base),
            "b.md",
            b"ours-new-file\n",
            "human@example.com",
            "ours adds b",
        );
        let theirs = commit_file(
            &repo,
            Some(base),
            "c.md",
            b"theirs-new-file\n",
            "human@example.com",
            "theirs adds c",
        );

        let tree_oid = GitEngine::merge_trees_blocking(
            &repo,
            &base.to_string(),
            &ours.to_string(),
            &theirs.to_string(),
        )
        .unwrap()
        .expect("disjoint edits merge cleanly");
        let tree = repo
            .find_tree(git2::Oid::from_str(&tree_oid).unwrap())
            .unwrap();
        assert!(tree.get_path(Path::new("a.md")).is_ok());
        assert!(tree.get_path(Path::new("b.md")).is_ok());
        assert!(tree.get_path(Path::new("c.md")).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_commit_op_produces_two_parent_commit_with_merged_tree() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("merge-into-main");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        let changeset_tip = commit_file(
            &local_repo,
            Some(main_oid),
            "feature.md",
            b"feature\n",
            "agent@example.com",
            "changeset edit",
        );

        let tree_oid = GitEngine::merge_trees_blocking(
            &local_repo,
            &main_oid.to_string(),
            &changeset_tip.to_string(),
            &main_oid.to_string(),
        )
        .unwrap()
        .expect("clean merge onto unmoved main");

        let op = BatchOp {
            commit_message: "changeset: land feature".into(),
            kind: BatchOpKind::MergeCommit {
                tree_oid: tree_oid.clone(),
                second_parent: changeset_tip.to_string(),
            },
            attribution: CommitAttribution::library_default(),
            target_ref: None,
        };
        let mut results = GitEngine::commit_each_then_push_blocking(&local_dir, vec![op], None);
        let merge_oid = results.remove(0).unwrap().expect("merge produced a commit");

        let origin_repo = git2::Repository::open_bare(&origin_dir).unwrap();
        let merge_commit = origin_repo
            .find_commit(git2::Oid::from_str(&merge_oid).unwrap())
            .unwrap();
        assert_eq!(merge_commit.parent_count(), 2);
        assert_eq!(merge_commit.parent_id(0).unwrap(), main_oid);
        assert_eq!(merge_commit.parent_id(1).unwrap(), changeset_tip);
        assert_eq!(merge_commit.tree_id().to_string(), tree_oid);

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    #[test]
    fn fetch_prune_removes_local_branch_absent_from_origin() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("prune-remove");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        // A branch that exists locally (as `create_ref` would leave it)
        // but was never pushed to origin.
        local_repo
            .reference("refs/heads/drua/gone", main_oid, false, "local only")
            .unwrap();
        assert!(local_repo.find_reference("refs/heads/drua/gone").is_ok());

        GitEngine::fetch_origin(&local_repo, None).unwrap();

        assert!(
            local_repo.find_reference("refs/heads/drua/gone").is_err(),
            "prune removes a local branch absent from origin"
        );
        assert!(
            local_repo.find_reference("refs/heads/main").is_ok(),
            "main, which IS on origin, survives"
        );

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    #[test]
    fn fetch_prune_keeps_branches_still_present_on_origin() {
        let (origin_dir, local_dir, local_repo) = origin_and_clone("prune-keep");
        let main_oid = local_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        local_repo
            .reference("refs/heads/drua/kept", main_oid, false, "local")
            .unwrap();
        GitEngine::push_ref(&local_repo, None, "refs/heads/drua/kept").unwrap();

        GitEngine::fetch_origin(&local_repo, None).unwrap();

        assert!(
            local_repo.find_reference("refs/heads/drua/kept").is_ok(),
            "a branch that IS on origin must survive prune"
        );

        let _ = std::fs::remove_dir_all(&origin_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }
}
