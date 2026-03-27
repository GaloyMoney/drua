use crate::primitives::NotificationId;

#[derive(thiserror::Error, Debug)]
pub enum NotificationError {
    #[error("NotificationError - NotFound: {0}")]
    NotFound(NotificationId),
    #[error("NotificationError - AlreadyRead")]
    AlreadyRead,
    #[error("NotificationError - AlreadyDismissed")]
    AlreadyDismissed,
    #[error("NotificationError - Hydration: {0}")]
    Hydration(String),
    #[error("NotificationError - Repository: {0}")]
    Repository(String),
}
