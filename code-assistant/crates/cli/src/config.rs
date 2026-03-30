use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_BIND: &str = "127.0.0.1:9222";
const DEFAULT_DATA_DIR: &str = "./data";
const DEFAULT_LABELS_DIR: &str = "./labels";
const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.7;

/// Telegram bot configuration for label review.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    /// Name of the environment variable holding the bot token.
    pub bot_token_env: String,
    /// Only respond to these Telegram user IDs.
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
}

/// Top-level config parsed from TOML.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub services: ServicesConfig,
    pub data_dir: String,
    #[serde(default = "default_labels_dir")]
    pub labels_dir: String,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub mining: MiningConfig,
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServicesConfig {
    /// Minimum human reviews per label before it's considered saturated.
    pub min_reviews_per_label: usize,
    /// Path to the ONNX model directory (default: `<data_dir>/models/onnx`).
    pub classifier_model_dir: Option<String>,
    /// Confidence threshold for using ML labels over heuristic (default: 0.7).
    #[serde(default = "default_confidence_threshold")]
    pub classifier_confidence_threshold: f32,
}

fn default_labels_dir() -> String {
    DEFAULT_LABELS_DIR.to_string()
}

fn default_confidence_threshold() -> f32 {
    DEFAULT_CONFIDENCE_THRESHOLD
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Optional: only index these subdirectories.
    pub paths: Option<Vec<String>>,
    /// Optional: skip files under these directories during indexing.
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    /// Optional: GitHub org/repo identifier for mining (e.g. "galoymoney/lana-bank").
    pub github: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MiningConfig {
    /// GitHub usernames whose review comments to mine.
    pub reviewers: Vec<String>,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            reviewers: vec!["bodymindarts".to_string(), "nicolasburtey".to_string()],
        }
    }
}

fn default_branch() -> String {
    "main".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            services: ServicesConfig::default(),
            data_dir: DEFAULT_DATA_DIR.to_string(),
            labels_dir: DEFAULT_LABELS_DIR.to_string(),
            repos: Vec::new(),
            mining: MiningConfig::default(),
            telegram: None,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_string(),
        }
    }
}

impl Default for ServicesConfig {
    fn default() -> Self {
        Self {
            min_reviews_per_label: 50,
            classifier_model_dir: None,
            classifier_confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
        }
    }
}

impl Config {
    /// Resolve the data directory, expanding `~` and making relative paths absolute.
    pub fn data_dir(&self) -> PathBuf {
        let path = expand_tilde(&self.data_dir);
        if path.is_relative() {
            std::env::current_dir().unwrap_or_default().join(path)
        } else {
            path
        }
    }

    /// Resolve the labels directory (committed label data).
    pub fn labels_dir(&self) -> PathBuf {
        let path = expand_tilde(&self.labels_dir);
        if path.is_relative() {
            std::env::current_dir().unwrap_or_default().join(path)
        } else {
            path
        }
    }

    /// Directory where cloned repos live.
    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir().join("repos")
    }

    /// Path to the SQLite database file.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("code-assistant.db")
    }

    /// Path to the ONNX classifier model directory.
    pub fn model_dir(&self) -> PathBuf {
        match &self.services.classifier_model_dir {
            Some(dir) => {
                let path = expand_tilde(dir);
                if path.is_relative() {
                    self.data_dir().join(path)
                } else {
                    path
                }
            }
            None => std::env::current_dir()
                .unwrap_or_default()
                .join("models")
                .join("onnx"),
        }
    }
}

/// Load config from the default path or `CODE_ASSISTANT_CONFIG` env var.
pub fn load_config() -> anyhow::Result<Config> {
    let path = config_path();
    if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&text)?;
        tracing::info!(path = %path.display(), "Loaded config");
        Ok(config)
    } else {
        tracing::info!("No config file found, using defaults");
        Ok(Config::default())
    }
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODE_ASSISTANT_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("code-assistant")
        .join("config.toml")
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    Path::new(path).to_path_buf()
}
