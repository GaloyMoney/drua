use std::path::PathBuf;

use serde::Deserialize;

/// Core configuration for code assistant storage and models.
#[derive(Debug, Clone, Deserialize)]
pub struct CoreConfig {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
    /// Path to the ONNX classifier model directory.
    pub model_dir: PathBuf,
}
