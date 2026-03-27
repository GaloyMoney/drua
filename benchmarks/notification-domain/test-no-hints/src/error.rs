use crate::NotificationId;

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("notification {0} not found")]
    NotFound(NotificationId),
    #[error("notification {0} is already read")]
    AlreadyRead(NotificationId),
    #[error("notification {0} is already dismissed")]
    AlreadyDismissed(NotificationId),
}
