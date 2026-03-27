use super::{entity::NotificationCommandError, repo::*};

#[derive(thiserror::Error, Debug)]
pub enum NotificationError {
    #[error("NotificationError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("NotificationError - Create: {0}")]
    Create(#[from] NotificationCreateError),
    #[error("NotificationError - Modify: {0}")]
    Modify(#[from] NotificationModifyError),
    #[error("NotificationError - Find: {0}")]
    Find(#[from] NotificationFindError),
    #[error("NotificationError - Query: {0}")]
    Query(#[from] NotificationQueryError),
    #[error("NotificationError - Command: {0}")]
    Command(#[from] NotificationCommandError),
}
