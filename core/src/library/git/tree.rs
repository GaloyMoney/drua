use std::path::Path;

use super::{Delta, TreeDiff};

pub(super) fn diff_trees(
    repo: &git2::Repository,
    from: Option<git2::Oid>,
    to: git2::Oid,
) -> Result<TreeDiff, git2::Error> {
    let new_tree = repo.find_commit(to)?.tree()?;
    let old_tree = match from {
        Some(oid) => Some(repo.find_commit(oid)?.tree()?),
        None => None,
    };

    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(false);

    let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))?;

    let mut deltas = Vec::new();
    diff.foreach(
        &mut |delta, _progress| {
            let new_path = delta
                .new_file()
                .path()
                .and_then(|p| p.to_str())
                .map(String::from);
            let old_path = delta
                .old_file()
                .path()
                .and_then(|p| p.to_str())
                .map(String::from);
            match delta.status() {
                git2::Delta::Deleted => {
                    if let Some(p) = old_path {
                        deltas.push(Delta::Deleted { path: p });
                    }
                }
                git2::Delta::Added
                | git2::Delta::Modified
                | git2::Delta::Copied
                | git2::Delta::Renamed
                | git2::Delta::Typechange => {
                    if let Some(p) = new_path {
                        deltas.push(Delta::Upserted { path: p });
                    }
                }
                _ => {}
            }
            true
        },
        None,
        None,
        None,
    )?;

    Ok(TreeDiff { deltas })
}

pub(super) fn read_blob_at(
    repo: &git2::Repository,
    commit: git2::Oid,
    path: &str,
) -> Result<Option<Vec<u8>>, git2::Error> {
    let tree = repo.find_commit(commit)?.tree()?;
    let entry = match tree.get_path(Path::new(path)) {
        Ok(e) => e,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let blob = repo.find_blob(entry.id())?;
    Ok(Some(blob.content().to_vec()))
}
