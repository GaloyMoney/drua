use crate::git::DeltaKind;

/// Identifies a draft branch by the uuid embedded in its ref name
/// (`refs/heads/drua/<uuid>`). The library knows nothing about what the
/// uuid means to the caller — core's `ChangesetId` and this type share
/// the same uuid by construction, converted at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DraftName(uuid::Uuid);

impl DraftName {
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// `drua/<uuid>` — used as the PR head ref name.
    pub fn branch(&self) -> String {
        format!("drua/{}", self.0)
    }

    pub(crate) fn git_ref(&self) -> String {
        format!("refs/heads/{}", self.branch())
    }
}

impl From<uuid::Uuid> for DraftName {
    fn from(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

/// A draft's name plus its tip at the moment it was resolved. `tip` is
/// intentionally not re-readable from `DraftHandle` alone — callers that
/// need a fresher tip call [`super::Drafts::handle`] again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftHandle {
    pub(super) name: DraftName,
    pub(super) tip: String,
}

impl DraftHandle {
    pub fn name(&self) -> DraftName {
        self.name
    }

    pub fn tip(&self) -> &str {
        &self.tip
    }
}

/// What a space read or write is addressed against: the published repo,
/// or a specific draft's branch at a known tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpaceTarget {
    Main,
    Draft(DraftHandle),
}

#[derive(Debug)]
pub enum Mergeability {
    Clean,
    Conflicts(Vec<String>),
}

#[derive(Debug)]
pub enum ApplyOutcome {
    Merged { merge_oid: String },
    Conflicts(Vec<String>),
}

#[derive(Debug)]
pub enum RebaseOutcome {
    Rebased { base_oid: String, head_oid: String },
    Conflicts(Vec<String>),
}

/// What changed about a draft since the caller last looked, from git's
/// point of view alone — no notion of changeset status.
#[derive(Debug)]
pub enum DraftObservation {
    /// Neither merged into main nor advanced past `head_oid`.
    Unchanged,
    /// `head_oid` is now an ancestor of `main_oid` — the draft landed on
    /// main by a path other than [`super::Drafts::apply`] (a squash or
    /// rebase merge through GitHub, for instance).
    MergedInto { main_oid: String },
    /// The ref moved past `head_oid` without merging into main — an
    /// external commit landed on the branch directly.
    Advanced { tip: String },
    /// The ref doesn't exist, even after a fetch.
    Missing,
}

pub struct TouchedPath {
    pub space_slug: String,
    pub rel_path: String,
    pub kind: DeltaKind,
}
