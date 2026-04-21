#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Under threshold, cache hot — do nothing.
    None,
    /// Idle timeout exceeded — start a brand-new thread.
    Orphan,
    /// Cache cold, under threshold — prune opportunistically.
    /// Pruning is "free" when the cached prefix has already expired.
    PruneOpportunistic,
    /// Over threshold — prune first, then summarize if still over.
    /// Phase 1: degrades to prune-only (summarization is Phase 2).
    PruneThenSummarize,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::agent::session::settings::CompactionConfig;

    fn config() -> CompactionConfig {
        CompactionConfig {
            enabled: true,
            token_threshold_fraction: 0.6,
            keep_recent_tool_results: 10,
            cache_ttl_seconds: 300,
            reset_time_delta_seconds: None,
        }
    }

    const CONTEXT_WINDOW: u64 = 200_000;

    #[test]
    fn none_when_disabled() {
        let mut cfg = config();
        cfg.enabled = false;
        assert_eq!(
            cfg.determine_action(999_999, CONTEXT_WINDOW, Duration::from_secs(9999)),
            CompactionAction::None
        );
    }

    #[test]
    fn none_when_under_threshold_and_cache_hot() {
        let cfg = config();
        // 60_000 < 120_000 threshold, 60s < 300s cache_ttl
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(60)),
            CompactionAction::None
        );
    }

    #[test]
    fn prune_opportunistic_when_under_threshold_and_cache_cold() {
        let cfg = config();
        // 60_000 < 120_000, but 600s > 300s cache_ttl
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(600)),
            CompactionAction::PruneOpportunistic
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_hot() {
        let cfg = config();
        // 150_000 > 120_000, 60s < 300s
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(60)),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_cold() {
        let cfg = config();
        // 150_000 > 120_000, 600s > 300s
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(600)),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn orphan_when_reset_threshold_exceeded() {
        use crate::agent::session::settings::ResetTimeDeltaSeconds;

        let cfg = CompactionConfig {
            reset_time_delta_seconds: Some(ResetTimeDeltaSeconds(600)),
            ..config()
        };
        // Under token threshold, but idle > 600s → Orphan
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(700)),
            CompactionAction::Orphan
        );
    }

    #[test]
    fn orphan_overrides_prune_then_summarize() {
        use crate::agent::session::settings::ResetTimeDeltaSeconds;

        let cfg = CompactionConfig {
            reset_time_delta_seconds: Some(ResetTimeDeltaSeconds(600)),
            ..config()
        };
        // Over token threshold AND idle > 600s → Orphan (not PruneThenSummarize)
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(700)),
            CompactionAction::Orphan
        );
    }

    #[test]
    fn no_orphan_when_under_reset_threshold() {
        use crate::agent::session::settings::ResetTimeDeltaSeconds;

        let cfg = CompactionConfig {
            reset_time_delta_seconds: Some(ResetTimeDeltaSeconds(600)),
            ..config()
        };
        // Under token threshold AND idle < 600s → None (not Orphan)
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(60)),
            CompactionAction::None
        );
    }
}
