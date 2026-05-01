use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use super::{tree, CommitOid, TreeDiff};
use crate::library::LibraryError;

pub(super) enum UpstreamCmd {
    Fetch {
        token: Option<String>,
        reply: oneshot::Sender<Result<(), LibraryError>>,
    },
    Head {
        reply: oneshot::Sender<Result<Option<CommitOid>, LibraryError>>,
    },
    TreeDiff {
        from: Option<git2::Oid>,
        to: git2::Oid,
        reply: oneshot::Sender<Result<TreeDiff, LibraryError>>,
    },
    ReadBlob {
        commit: git2::Oid,
        path: String,
        reply: oneshot::Sender<Result<Option<Vec<u8>>, LibraryError>>,
    },
}

/// Open a bare repo at `repo_path`, cloning from `repo_url` if not present.
/// When `repo_url` is `None` and the repo doesn't exist, init an empty bare.
pub(super) fn open_or_clone_bare(
    repo_path: &Path,
    repo_url: Option<&str>,
    token: Option<&str>,
) -> Result<git2::Repository, LibraryError> {
    if let Ok(repo) = git2::Repository::open_bare(repo_path) {
        return Ok(repo);
    }

    if let Some(parent) = repo_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LibraryError::Io(format!("create_dir_all: {e}")))?;
        }
    }
    if repo_path.exists() {
        // Stale dir from a half-finished clone; remove so libgit2 won't fail.
        std::fs::remove_dir_all(repo_path)
            .map_err(|e| LibraryError::Io(format!("remove stale dir: {e}")))?;
    }

    match repo_url {
        Some(url) => {
            let mut cb = git2::RemoteCallbacks::new();
            if let Some(t) = token {
                let t = t.to_string();
                cb.credentials(move |_url, _username, _allowed| {
                    git2::Cred::userpass_plaintext("x-access-token", &t)
                });
            }
            let mut fo = git2::FetchOptions::new();
            fo.remote_callbacks(cb);
            let mut builder = git2::build::RepoBuilder::new();
            builder.bare(true).fetch_options(fo);
            builder
                .clone(url, repo_path)
                .map_err(|e| LibraryError::Git(format!("clone bare: {e}")))
        }
        None => git2::Repository::init_bare(repo_path)
            .map_err(|e| LibraryError::Git(format!("init bare: {e}"))),
    }
}

pub(super) fn run(repo: git2::Repository, mut rx: mpsc::Receiver<UpstreamCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            UpstreamCmd::Fetch { token, reply } => {
                let _ = reply.send(do_fetch(&repo, token.as_deref()));
            }
            UpstreamCmd::Head { reply } => {
                let _ = reply.send(do_head(&repo));
            }
            UpstreamCmd::TreeDiff { from, to, reply } => {
                let r = tree::diff_trees(&repo, from, to)
                    .map_err(|e| LibraryError::Git(format!("tree_diff: {e}")));
                let _ = reply.send(r);
            }
            UpstreamCmd::ReadBlob {
                commit,
                path,
                reply,
            } => {
                let r = tree::read_blob_at(&repo, commit, &path)
                    .map_err(|e| LibraryError::Git(format!("read_blob: {e}")));
                let _ = reply.send(r);
            }
        }
    }
}

fn do_fetch(repo: &git2::Repository, token: Option<&str>) -> Result<(), LibraryError> {
    let mut remote = repo
        .find_remote("origin")
        .map_err(|e| LibraryError::Git(format!("find origin: {e}")))?;

    let mut cb = git2::RemoteCallbacks::new();
    if let Some(t) = token {
        let t = t.to_string();
        cb.credentials(move |_url, _username, _allowed| {
            git2::Cred::userpass_plaintext("x-access-token", &t)
        });
    }
    let mut fo = git2::FetchOptions::new();
    fo.remote_callbacks(cb);

    remote
        .fetch(&["main"], Some(&mut fo), None)
        .map_err(|e| LibraryError::Git(format!("fetch: {e}")))?;

    let fetch_head_ref = match repo.find_reference("FETCH_HEAD") {
        Ok(r) => r,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(()),
        Err(e) => return Err(LibraryError::Git(format!("FETCH_HEAD: {e}"))),
    };
    let fetch_commit = repo
        .reference_to_annotated_commit(&fetch_head_ref)
        .map_err(|e| LibraryError::Git(format!("annotated FETCH_HEAD: {e}")))?;

    let refname = "refs/heads/main";
    match repo.find_reference(refname) {
        Ok(mut reference) => {
            let analysis = repo
                .merge_analysis(&[&fetch_commit])
                .map_err(|e| LibraryError::Git(format!("merge_analysis: {e}")))?;
            if analysis.0.is_up_to_date() {
                return Ok(());
            }
            if analysis.0.is_fast_forward() {
                reference
                    .set_target(fetch_commit.id(), "fast-forward")
                    .map_err(|e| LibraryError::Git(format!("set_target: {e}")))?;
                repo.set_head(refname)
                    .map_err(|e| LibraryError::Git(format!("set_head: {e}")))?;
                return Ok(());
            }
            Err(LibraryError::Git(
                "non-fast-forward fetch in bare engine".into(),
            ))
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            // First fetch into an empty bare repo — manually create main.
            repo.reference(refname, fetch_commit.id(), true, "initial fetch")
                .map_err(|e| LibraryError::Git(format!("create main: {e}")))?;
            repo.set_head(refname)
                .map_err(|e| LibraryError::Git(format!("set_head: {e}")))?;
            Ok(())
        }
        Err(e) => Err(LibraryError::Git(format!("find main: {e}"))),
    }
}

fn do_head(repo: &git2::Repository) -> Result<Option<CommitOid>, LibraryError> {
    match repo.head() {
        Ok(reference) => {
            let commit = reference
                .peel_to_commit()
                .map_err(|e| LibraryError::Git(format!("peel head: {e}")))?;
            Ok(Some(CommitOid(commit.id())))
        }
        Err(e)
            if matches!(
                e.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(LibraryError::Git(format!("head: {e}"))),
    }
}

pub(super) async fn ensure_repo(
    repo_path: PathBuf,
    repo_url: Option<String>,
    token: Option<String>,
) -> Result<git2::Repository, LibraryError> {
    tokio::task::spawn_blocking(move || {
        open_or_clone_bare(&repo_path, repo_url.as_deref(), token.as_deref())
    })
    .await
    .map_err(|e| LibraryError::Git(format!("init join: {e}")))?
}
