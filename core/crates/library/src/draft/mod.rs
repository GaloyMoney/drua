mod primitives;

use std::sync::Arc;

pub use primitives::{
    ApplyOutcome, DraftHandle, DraftName, DraftObservation, Mergeability, RebaseOutcome,
    SpaceTarget, TouchedPath,
};

use crate::attribution::CommitAttribution;
use crate::error::LibraryError;
use crate::git::GitEngine;

const DRAFT_REF_PREFIX: &str = "refs/heads/drua/";

/// Draft-branch mechanics: ref naming, fetch-and-retry on replica lag,
/// merge and rebase recipes, and repo layout (`spaces/<slug>/`). Knows
/// nothing of changeset status, Postgres, auth, or GitHub — that's
/// core's `Changesets`, layered on top.
#[derive(Clone)]
pub struct Drafts {
    git: Arc<GitEngine>,
}

impl Drafts {
    pub(crate) fn new(git: &Arc<GitEngine>) -> Self {
        Self {
            git: Arc::clone(git),
        }
    }

    /// Main's current tip, to open a new draft against.
    pub async fn fresh_base(&self) -> Result<String, LibraryError> {
        self.git
            .fetch_and_head()
            .await?
            .ok_or(LibraryError::MainUnborn)
    }

    /// Best-effort: creates `name`'s ref at `base_oid`. A failure here is
    /// recoverable — [`Self::handle`] repairs a missing ref on first use.
    pub async fn open(&self, name: DraftName, base_oid: &str) -> Result<(), LibraryError> {
        self.git.create_ref(&name.git_ref(), base_oid).await
    }

    /// Resolves `name` to its current tip, recreating the ref at
    /// `known_head` if it's missing.
    pub async fn handle(
        &self,
        name: DraftName,
        known_head: &str,
    ) -> Result<DraftHandle, LibraryError> {
        let git_ref = name.git_ref();
        if let Some(tip) = self.git.resolve_ref(&git_ref).await? {
            return Ok(DraftHandle { name, tip });
        }
        if let Err(e) = self.git.create_ref(&git_ref, known_head).await {
            // Most likely this replica hasn't fetched known_head's commit
            // object yet (Postgres already advanced past what this clone
            // has). A fetch may resolve the ref outright (a peer already
            // created it) or just bring the object in for one retry.
            tracing::debug!(
                error = %e,
                %git_ref,
                "drafts.handle: create_ref failed; fetching origin and retrying once"
            );
            self.git.fetch_and_head().await?;
            if let Some(tip) = self.git.resolve_ref(&git_ref).await? {
                return Ok(DraftHandle { name, tip });
            }
            self.git.create_ref(&git_ref, known_head).await?;
        }
        // Re-resolve rather than assuming the ref landed at known_head:
        // create_ref's "origin already has this ref" fallback (a push
        // rejected, then a fetch that finds it) can leave the local ref at
        // whatever oid origin actually has, not the oid we asked for.
        let tip = self.git.resolve_ref(&git_ref).await?.ok_or_else(|| {
            LibraryError::Git(format!(
                "drafts.handle: {git_ref} still missing after create_ref reported success"
            ))
        })?;
        Ok(DraftHandle { name, tip })
    }

    pub async fn mergeability(
        &self,
        base_oid: &str,
        head_oid: &str,
    ) -> Result<Mergeability, LibraryError> {
        let main_oid = self
            .git
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(LibraryError::MainUnborn)?;
        match self.git.merge_trees(base_oid, head_oid, &main_oid).await? {
            Ok(_) => Ok(Mergeability::Clean),
            Err(paths) => Ok(Mergeability::Conflicts(paths)),
        }
    }

    /// Space-scoped files touched between `base_oid` and `head_oid`.
    /// Paths outside `spaces/` (there shouldn't be any) are skipped.
    pub async fn touched(
        &self,
        base_oid: &str,
        head_oid: &str,
    ) -> Result<Vec<TouchedPath>, LibraryError> {
        let deltas = self.git.changes_since(Some(base_oid), head_oid).await?;
        let mut out = Vec::with_capacity(deltas.len());
        for delta in deltas {
            let Some((space_slug, rel_path)) = split_space_path(&delta.path) else {
                continue;
            };
            out.push(TouchedPath {
                space_slug,
                rel_path,
                kind: delta.kind,
            });
        }
        Ok(out)
    }

    pub async fn apply(
        &self,
        head_oid: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<ApplyOutcome, LibraryError> {
        match self
            .git
            .merge_into_main(head_oid, message, attribution)
            .await
        {
            Ok(merge_oid) => Ok(ApplyOutcome::Merged { merge_oid }),
            Err(LibraryError::MergeConflicts { paths }) => Ok(ApplyOutcome::Conflicts(paths)),
            Err(e) => Err(e),
        }
    }

    pub async fn rebase(
        &self,
        name: DraftName,
        known_head: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<RebaseOutcome, LibraryError> {
        let main_oid = self
            .git
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(LibraryError::MainUnborn)?;
        match self
            .git
            .rebase_ref(&name.git_ref(), &main_oid, known_head, message, attribution)
            .await?
        {
            Ok((base_oid, head_oid)) => Ok(RebaseOutcome::Rebased { base_oid, head_oid }),
            Err(paths) => Ok(RebaseOutcome::Conflicts(paths)),
        }
    }

    /// What happened to `name`'s draft since the caller last recorded
    /// `base_oid`/`head_oid`, against `main_oid`.
    pub async fn observe(
        &self,
        name: DraftName,
        base_oid: &str,
        head_oid: &str,
        main_oid: &str,
    ) -> Result<DraftObservation, LibraryError> {
        if head_oid != base_oid {
            if let Some(base) = self.git.merge_base(main_oid, head_oid).await? {
                if base == head_oid {
                    return Ok(DraftObservation::MergedInto {
                        main_oid: main_oid.to_string(),
                    });
                }
            }
        }

        let git_ref = name.git_ref();
        let mut ref_oid = self.git.resolve_ref(&git_ref).await?;
        if ref_oid.is_none() {
            // Missing locally can just mean "not fetched yet" — the ref may
            // have been pushed (e.g. by a submit) after this replica's last
            // fetch. Confirm against origin before reporting it missing.
            self.git.fetch_and_head().await?;
            ref_oid = self.git.resolve_ref(&git_ref).await?;
            if ref_oid.is_none() {
                return Ok(DraftObservation::Missing);
            }
        }

        let tip = ref_oid.expect("checked above");
        if tip != head_oid {
            let merge_base = self.git.merge_base(&tip, head_oid).await;
            if let Err(e) = &merge_base {
                tracing::debug!(error = %e, %tip, %head_oid, "drafts.observe: couldn't verify tip ancestry; treating as unchanged this tick");
            }
            if is_genuine_advance(&merge_base, &tip) {
                return Ok(DraftObservation::Advanced { tip });
            }
        }
        Ok(DraftObservation::Unchanged)
    }

    pub async fn close(&self, name: DraftName) -> Result<(), LibraryError> {
        self.git.delete_ref(&name.git_ref(), true).await
    }

    /// Every draft ref that currently exists, fetched first so this
    /// replica's view matches origin. A ref name that doesn't parse as a
    /// uuid is skipped and logged at `debug` rather than failing the
    /// whole list.
    pub async fn list(&self) -> Result<Vec<DraftName>, LibraryError> {
        self.git.fetch_and_head().await?;
        let refs = self.git.list_refs(DRAFT_REF_PREFIX).await?;
        let mut out = Vec::with_capacity(refs.len());
        for (refname, _oid) in refs {
            let Some(id_str) = refname.strip_prefix(DRAFT_REF_PREFIX) else {
                continue;
            };
            match id_str.parse::<uuid::Uuid>() {
                Ok(id) => out.push(DraftName::from(id)),
                Err(_) => {
                    tracing::debug!(
                        %refname,
                        "drafts.list: ref name doesn't parse as a uuid; skipping"
                    );
                }
            }
        }
        Ok(out)
    }
}

/// The decision core of [`Drafts::observe`]'s advance check, isolated so
/// it's unit-testable without a `GitEngine`. `Ok(None)` collapses two
/// distinct `merge_base` outcomes — no common ancestor, and one oid's
/// object missing from this replica's clone — and an `Err` (e.g. the
/// same missing-object case surfacing as a git error instead) are all
/// treated as "can't confirm a genuine advance", which is the safe
/// default: reporting a falsely-advanced `tip` would let a caller move
/// its recorded head backwards.
fn is_genuine_advance(merge_base: &Result<Option<String>, LibraryError>, tip: &str) -> bool {
    matches!(merge_base, Ok(Some(base)) if base != tip)
}

fn split_space_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("spaces/")?;
    let (slug, rel) = rest.split_once('/')?;
    Some((slug.to_string(), rel.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_genuine_advance_true_when_head_oid_is_a_strict_ancestor_of_tip() {
        assert!(is_genuine_advance(&Ok(Some("head".into())), "tip"));
    }

    #[test]
    fn is_genuine_advance_false_when_tip_is_an_ancestor_of_head_oid() {
        assert!(!is_genuine_advance(&Ok(Some("tip".into())), "tip"));
    }

    #[test]
    fn is_genuine_advance_false_when_merge_base_finds_no_common_ancestor() {
        assert!(!is_genuine_advance(&Ok(None), "tip"));
    }

    #[test]
    fn is_genuine_advance_false_when_merge_base_errors() {
        let err = Err(LibraryError::Git("object missing".into()));
        assert!(!is_genuine_advance(&err, "tip"));
    }

    #[test]
    fn split_space_path_extracts_slug_and_rel() {
        assert_eq!(
            split_space_path("spaces/drua-dev/efforts/x/a.md"),
            Some(("drua-dev".to_string(), "efforts/x/a.md".to_string()))
        );
    }

    #[test]
    fn split_space_path_rejects_non_space_paths() {
        assert_eq!(split_space_path("other/thing.md"), None);
        assert_eq!(split_space_path("spaces/only-slug"), None);
        assert_eq!(split_space_path(""), None);
    }

    #[test]
    fn draft_name_branch_and_git_ref() {
        let id = uuid::Uuid::new_v4();
        let name = DraftName::from(id);
        assert_eq!(name.uuid(), id);
        assert_eq!(name.branch(), format!("drua/{id}"));
        assert_eq!(name.git_ref(), format!("refs/heads/drua/{id}"));
    }
}
