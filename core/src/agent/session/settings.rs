use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::compaction::trigger::CompactionAction;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSettings {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

impl CompactionConfig {
    /// Evaluate whether compaction should run, and which kind.
    ///
    /// The trigger is **cache-aware**: pruning invalidates the provider's cached
    /// prefix, so we avoid it while the cache is hot (within `cache_ttl`).
    /// Once the cache has expired we prune opportunistically to set up a clean
    /// cacheable prefix for the next burst of turns.
    pub fn determine_action(
        &self,
        estimated_tokens: u64,
        time_since_last_turn: Duration,
    ) -> CompactionAction {
        if !self.enabled {
            return CompactionAction::None;
        }

        let threshold =
            (self.context_window_tokens as f64 * self.token_threshold_fraction) as u64;
        let cache_ttl = Duration::from_secs(self.cache_ttl_seconds);
        let cache_cold = time_since_last_turn > cache_ttl;

        match (estimated_tokens > threshold, cache_cold) {
            (false, false) => CompactionAction::None,
            (false, true) => CompactionAction::PruneOpportunistic,
            (true, _) => CompactionAction::PruneThenSummarize,
        }
    }
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
