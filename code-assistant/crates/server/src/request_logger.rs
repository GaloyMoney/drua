use code_assistant_core::request_log::RequestLogEntry;

/// Boxed error type used by [`RequestLogger`] trait methods.
pub type LoggerError = Box<dyn std::error::Error + Send + Sync>;

/// Trait for logging code assistant requests.
///
/// Implementations may write to Postgres (gateway) or silently discard (standalone).
#[async_trait::async_trait]
pub trait RequestLogger: Send + Sync + 'static {
    /// Record a completed request.
    async fn log_request(&self, entry: &RequestLogEntry) -> Result<(), LoggerError>;
}

/// No-op [`RequestLogger`] that silently discards all entries.
///
/// Used as the default when no external logger (e.g. Postgres) is configured.
#[derive(Clone)]
pub struct NoopRequestLogger;

#[async_trait::async_trait]
impl RequestLogger for NoopRequestLogger {
    async fn log_request(&self, _entry: &RequestLogEntry) -> Result<(), LoggerError> {
        Ok(())
    }
}
