use code_assistant_core::request_log::RequestLogEntry;

/// Boxed error type used by [`RequestLogger`] trait methods.
pub type LoggerError = Box<dyn std::error::Error + Send + Sync>;

/// Trait for logging code assistant requests.
#[async_trait::async_trait]
pub trait RequestLogger: Send + Sync + 'static {
    /// Record a completed request.
    async fn log_request(&self, entry: &RequestLogEntry) -> Result<(), LoggerError>;
}
