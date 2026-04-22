use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::compaction::trigger::CompactionAction;

/// Whole-second auto-reset threshold for an `AgentSession`. Wraps a
/// `u32` count of seconds; the `#[serde(transparent)]` derive lets it
/// (de)serialize as a bare integer in YAML / JSONB instead of serde's
/// awkward `{ secs, nanos }` shape for `std::time::Duration`.
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
    /// Enable the compaction system.
    pub enabled: bool,
    /// Token threshold as fraction of context window (e.g. 0.6 = 60%).
    pub token_threshold_fraction: f64,
    /// Number of recent tool results to keep unmasked.
    pub keep_recent_tool_results: usize,
    /// If set, start a fresh thread (orphan) when a user message arrives
    /// more than this many seconds after the previous assistant response.
    #[serde(default)]
    pub reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
}

impl CompactionConfig {
    /// Evaluate whether compaction should run, and which kind.
    ///
    /// The trigger is **cache-aware**: pruning invalidates the provider's cached
    /// prefix, so we avoid it while the cache is hot (within `cache_ttl`).
    /// Once the cache has expired we prune opportunistically to set up a clean
    /// cacheable prefix for the next burst of turns.
    ///
    /// `Orphan` takes priority: if the reset threshold is exceeded the session
    /// starts a brand-new thread regardless of token counts.
    ///
    /// `cache_ttl_seconds` is the provider's prompt-cache TTL sourced from
    /// [`ModelDefaults`](crate::agent::config::ModelDefaults).
    pub fn determine_action(
        &self,
        estimated_tokens: u64,
        context_window_tokens: u64,
        cache_ttl_seconds: u64,
        time_since_last_turn: Duration,
    ) -> CompactionAction {
        if !self.enabled {
            return CompactionAction::None;
        }

        // Orphan check first — idle timeout overrides everything else
        if let Some(reset) = &self.reset_time_delta_seconds {
            if time_since_last_turn > reset.as_duration() {
                return CompactionAction::Orphan;
            }
        }

        let threshold = (context_window_tokens as f64 * self.token_threshold_fraction) as u64;
        let cache_ttl = Duration::from_secs(cache_ttl_seconds);
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
            enabled: true,
            token_threshold_fraction: 0.6,
            keep_recent_tool_results: 10,
            reset_time_delta_seconds: None,
        }
    }
}
