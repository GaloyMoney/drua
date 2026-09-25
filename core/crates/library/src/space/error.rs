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
    /// rev3 D9/D15 rule 3: a `Propose`-only subject's direct
    /// `space:<slug>/` write — no `Update`, so there's nothing to fall
    /// back to but a draft; the message names the exact path form and
    /// command so the model can self-correct.
    #[error(
        "SpaceError - UseDraft: direct edits to space:{slug}/ are not permitted for this subject; write draft:{slug}/<path> instead — it is staged in your draft and published with `spaces publish-draft`"
    )]
    UseDraft { slug: String },
    /// rev3 D15: a subject with an open draft may not write
    /// `space:<slug>/` directly — fail closed rather than silently
    /// landing on `main`, even for a subject that holds `Update`.
    #[error(
        "SpaceError - DraftOpen: you have an open draft {id} \"{title}\"; write draft:{slug}/<path> to keep staging, or run `spaces publish-draft` / `spaces discard-draft` before writing space:{slug}/ directly"
    )]
    DraftOpen {
        id: String,
        title: String,
        slug: String,
    },
}

impl From<derive_builder::UninitializedFieldError> for SpaceError {
    fn from(err: derive_builder::UninitializedFieldError) -> Self {
        Self::MissingField(err.field_name().to_string())
    }
}
