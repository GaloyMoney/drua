#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("CoreError - Reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("CoreError - SerdeJson: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("CoreError - Sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("CoreError - Classifier: {0}")]
    Classifier(String),
}
