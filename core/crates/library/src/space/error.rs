use thiserror::Error;

use super::repo::{SpaceCreateError, SpaceFindError, SpaceQueryError};

#[derive(Error, Debug)]
pub enum SpaceError {
    #[error("SpaceError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("SpaceError - Create: {0}")]
    Create(#[from] SpaceCreateError),
    #[error("SpaceError - Find: {0}")]
    Find(#[from] SpaceFindError),
    #[error("SpaceError - Query: {0}")]
    Query(#[from] SpaceQueryError),
    #[error(
        "SpaceError - InvalidSlug: {slug:?} (must be [a-z0-9-]+, no leading/trailing or double hyphens)"
    )]
    InvalidSlug { slug: String },
    #[error("SpaceError - MissingField: {0}")]
    MissingField(String),
    #[error("SpaceError - Git: {0}")]
    Git(String),
    #[error("SpaceError - Validation: {0}")]
    Validation(String),
    #[error("SpaceError - NotFound: space {slug:?} does not exist")]
    NotFound { slug: String },
    /// Malformed space URI — distinguishes "fix the path" from auth denial (`Unauthorized`) or unknown slug (`NotFound`).
    #[error("SpaceError - BadRequest: {reason}")]
    BadRequest { reason: String },
    #[error("SpaceError - NotMounted: space {slug:?} is not mounted in project {project_id:?}")]
    NotMounted {
        slug: String,
        project_id: uuid::Uuid,
    },
    #[error("SpaceError - CrossSpaceMove: cannot move {from_slug:?} → {to_slug:?} across spaces")]
    CrossSpaceMove { from_slug: String, to_slug: String },
    #[error("SpaceError - Io: {0}")]
    Io(String),
    #[error("SpaceError - InvalidRelPath: {path:?} ({reason})")]
    InvalidRelPath { path: String, reason: String },
    /// `delete_file` (and `move_file`'s source) target a path that
    /// doesn't exist at HEAD. Replaces the prior silent no-op so
    /// callers learn the operation was a miss — important when the
    /// importer has canonicalised the file and the original path
    /// the caller knows about no longer points anywhere.
    #[error("SpaceError - PathNotFound: {path:?} does not exist at HEAD in space {slug:?}")]
    PathNotFound { slug: String, path: String },
    #[error("SpaceError - ChangesetNotOpen: changeset {id} is {status}; only open changesets accept edits")]
    ChangesetNotOpen { id: String, status: String },
    #[error("SpaceError - ChangesetForeign: changeset {id} belongs to another project")]
    ChangesetForeign { id: String },
    #[error(
        "SpaceError - ReadOnly: space:{slug}/ is read-only for this subject — spaces are edited by workflows (`space_writes:`) or by admins (`drua_admin_spaces` with target: draft)"
    )]
    ReadOnly { slug: String },
    #[error(
        "SpaceError - DraftOpen: you have an open draft {id} \"{title}\"; write draft:{slug}/<path> (or pass target: draft to drua_admin_spaces) to keep staging, or run `drua_admin_spaces merge-draft` / `discard-draft` before writing space:{slug}/ directly"
    )]
    DraftOpen {
        id: String,
        title: String,
        slug: String,
    },
    #[error("SpaceError - RunReadOnly: space:{slug}/ is read-only in this workflow run — set `space_writes.mode` to `merge` or `open_pr` in the workflow definition to let its steps write")]
    RunReadOnly { slug: String },
}

impl From<derive_builder::UninitializedFieldError> for SpaceError {
    fn from(err: derive_builder::UninitializedFieldError) -> Self {
        Self::MissingField(err.field_name().to_string())
    }
}
