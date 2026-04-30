use thiserror::Error;

use crate::auth::error::AuthorizationError;

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
    #[error("SpaceError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error(
        "SpaceError - InvalidSlug: {slug:?} (must be [a-z0-9-]+, no leading/trailing or double hyphens)"
    )]
    InvalidSlug { slug: String },
    #[error("SpaceError - MissingField: {0}")]
    MissingField(String),
    #[error("SpaceError - NotFound: slug {slug:?}")]
    NotFound { slug: String },
    #[error(
        "SpaceError - WorkspaceNotAuthorized: workspace {workspace_id} is not in space {slug:?}'s authorized list"
    )]
    WorkspaceNotAuthorized {
        slug: String,
        workspace_id: crate::primitives::WorkspaceId,
    },
}

impl From<derive_builder::UninitializedFieldError> for SpaceError {
    fn from(err: derive_builder::UninitializedFieldError) -> Self {
        Self::MissingField(err.field_name().to_string())
    }
}
