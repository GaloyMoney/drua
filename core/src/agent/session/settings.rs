use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::compaction::trigger::CompactionAction;

/// `serde(transparent)` for bare-integer (de)serialization in YAML / JSONB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResetTimeDeltaSeconds(pub u32);

impl ResetTimeDeltaSeconds {
    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0 as u64)
    }
}

impl From<u32> for ResetTimeDeltaSeconds {
    fn from(s: u32) -> Self {
        Self(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Fraction of context window (e.g. 0.6 = 60%).
    pub token_threshold_fraction: f64,
    pub keep_recent_tool_results: usize,
    /// Idle threshold; exceeding it starts a fresh (orphan) thread.
    #[serde(default)]
    pub reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
    /// Cache-aware prune trigger: prune opportunistically once this many
    /// seconds have passed since the last assistant response (the prompt
    /// cache is already invalidated, so pruning is free). Anthropic's
    /// default cache window is ≈ 300s; providers without prompt caching
    /// should set 0 to disable the opportunistic prune path.
    pub prune_after_seconds: u64,
}

impl CompactionConfig {
    /// Cache-aware: pruning invalidates the provider cache, so avoid it
    /// while hot. `Orphan` (idle reset) takes priority over token checks.
    pub fn determine_action(
        &self,
        estimated_tokens: u64,
        context_window_tokens: u64,
        time_since_last_turn: Duration,
    ) -> CompactionAction {
        if !self.enabled {
            return CompactionAction::None;
        }

        if let Some(reset) = &self.reset_time_delta_seconds {
            if time_since_last_turn > reset.as_duration() {
                return CompactionAction::Orphan;
            }
        }

        let threshold = (context_window_tokens as f64 * self.token_threshold_fraction) as u64;
        let cache_cold = time_since_last_turn > Duration::from_secs(self.prune_after_seconds);

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
            enabled: true,
            token_threshold_fraction: 0.6,
            keep_recent_tool_results: 10,
            reset_time_delta_seconds: None,
            prune_after_seconds: 300,
        }
    }
}
