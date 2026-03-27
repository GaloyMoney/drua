use thiserror::Error;

#[derive(Debug, Error)]
pub enum NotificationError {
    #[error("notification not found")]
    NotFound,
    #[error("notification already dismissed")]
    AlreadyDismissed,
    #[error("hydration error: missing field '{0}'")]
    HydrationError(&'static str),
}
