#[derive(Debug, thiserror::Error)]
pub enum ToolCachingError {
    #[error("tool-caching sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("tool-caching serde_json error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error(
        "no persisted tool invocation with this id for this session. \
         Invocation ids never expire; a valid one only ever comes verbatim \
         from a `_recovery.invocation_id` (or `sub_invocations[].invocation_id`) \
         in an earlier response in this conversation — do not guess or \
         reconstruct one. If you did not copy it, re-run the original tool \
         call and use the id it returns."
    )]
    InvocationNotFound,
    #[error("invalid fetch path: {0}")]
    InvalidPath(String),
    #[error(
        "fetch response too large: {size} bytes (max {max}); narrow the query \
         with a smaller `len`, a deeper `path`, or by switching mode{hint}"
    )]
    FetchResponseTooLarge {
        size: usize,
        max: usize,
        hint: String,
    },
}
