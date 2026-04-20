use std::time::Duration;

use super::super::settings::CompactionConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Under threshold, cache hot — do nothing.
    None,
    /// Cache cold, under threshold — prune opportunistically.
    /// Pruning is "free" when the cached prefix has already expired.
    PruneOpportunistic,
    /// Over threshold — prune first, then summarize if still over.
    /// Phase 1: degrades to prune-only (summarization is Phase 2).
    PruneThenSummarize,
}

/// Evaluate whether compaction should run, and which kind.
///
/// The trigger is **cache-aware**: pruning invalidates the provider's cached
/// prefix, so we avoid it while the cache is hot (within `cache_ttl`).
/// Once the cache has expired we prune opportunistically to set up a clean
/// cacheable prefix for the next burst of turns.
pub fn evaluate(
    estimated_tokens: u64,
    time_since_last_turn: Duration,
    config: &CompactionConfig,
) -> CompactionAction {
    if !config.enabled {
        return CompactionAction::None;
    }

    let threshold = (config.context_window_tokens as f64 * config.token_threshold_fraction) as u64;
    let cache_ttl = Duration::from_secs(config.cache_ttl_seconds);
    let cache_cold = time_since_last_turn > cache_ttl;

    match (estimated_tokens > threshold, cache_cold) {
        (false, false) => CompactionAction::None,
        (false, true) => CompactionAction::PruneOpportunistic,
        (true, _) => CompactionAction::PruneThenSummarize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CompactionConfig {
        CompactionConfig {
            enabled: true,
            token_threshold_fraction: 0.6,
            context_window_tokens: 200_000,
            keep_recent_tool_results: 10,
            cache_ttl_seconds: 300,
        }
    }

    #[test]
    fn none_when_disabled() {
        let mut cfg = config();
        cfg.enabled = false;
        assert_eq!(
            evaluate(999_999, Duration::from_secs(9999), &cfg),
            CompactionAction::None
        );
    }

    #[test]
    fn none_when_under_threshold_and_cache_hot() {
        let cfg = config();
        // 60_000 < 120_000 threshold, 60s < 300s cache_ttl
        assert_eq!(
            evaluate(60_000, Duration::from_secs(60), &cfg),
            CompactionAction::None
        );
    }

    #[test]
    fn prune_opportunistic_when_under_threshold_and_cache_cold() {
        let cfg = config();
        // 60_000 < 120_000, but 600s > 300s cache_ttl
        assert_eq!(
            evaluate(60_000, Duration::from_secs(600), &cfg),
            CompactionAction::PruneOpportunistic
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_hot() {
        let cfg = config();
        // 150_000 > 120_000, 60s < 300s
        assert_eq!(
            evaluate(150_000, Duration::from_secs(60), &cfg),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_cold() {
        let cfg = config();
        // 150_000 > 120_000, 600s > 300s
        assert_eq!(
            evaluate(150_000, Duration::from_secs(600), &cfg),
            CompactionAction::PruneThenSummarize
        );
    }
}
