use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSettings {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSimplificationSettings {
    pub simplify_after_idle_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Enable the compaction system.
    pub enabled: bool,
    /// Token threshold as fraction of context window (e.g. 0.6 = 60%).
    pub token_threshold_fraction: f64,
    /// Context window size for the model in tokens.
    pub context_window_tokens: u64,
    /// Number of recent tool results to keep unmasked.
    pub keep_recent_tool_results: usize,
    /// Provider cache TTL in seconds — prune freely after this inactivity period.
    pub cache_ttl_seconds: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_threshold_fraction: 0.6,
            context_window_tokens: 200_000,
            keep_recent_tool_results: 10,
            cache_ttl_seconds: 300,
        }
    }
}
