#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// Under threshold, cache hot — do nothing.
    None,
    /// Idle timeout exceeded — start a brand-new thread.
    Orphan,
    /// Cache cold, under threshold — pruning is free.
    PruneOpportunistic,
    /// Over threshold; phase 1 degrades to prune-only (summarization TBD).
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
            reset_time_delta_seconds: None,
        }
    }

    const CONTEXT_WINDOW: u64 = 200_000;
    const CACHE_TTL: u64 = 300;

    #[test]
    fn none_when_disabled() {
        let mut cfg = config();
        cfg.enabled = false;
        assert_eq!(
            cfg.determine_action(
                999_999,
                CONTEXT_WINDOW,
                CACHE_TTL,
                Duration::from_secs(9999)
            ),
            CompactionAction::None
        );
    }

    #[test]
    fn none_when_under_threshold_and_cache_hot() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(60)),
            CompactionAction::None
        );
    }

    #[test]
    fn prune_opportunistic_when_under_threshold_and_cache_cold() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(600)),
            CompactionAction::PruneOpportunistic
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_hot() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(60)),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_cold() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(600)),
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
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(700)),
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
        // Orphan wins over PruneThenSummarize.
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(700)),
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
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, CACHE_TTL, Duration::from_secs(60)),
            CompactionAction::None
        );
    }
}
