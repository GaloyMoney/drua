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
            prune_after_seconds: Some(300),
        }
    }

    const CONTEXT_WINDOW: u64 = 200_000;

    #[test]
    fn none_when_disabled() {
        let mut cfg = config();
        cfg.enabled = false;
        // `enabled` short-circuits before the model TTL is even looked at.
        assert_eq!(
            cfg.determine_action(999_999, CONTEXT_WINDOW, Duration::from_secs(9999), None),
            CompactionAction::None
        );
    }

    #[test]
    fn none_when_under_threshold_and_cache_hot() {
        let cfg = config();
        // No model in the chain yet (`ModelDefaults::cache_ttl_seconds` is
        // `None` until §4.1's registry data is populated) — role window only.
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(60), None),
            CompactionAction::None
        );
    }

    #[test]
    fn prune_opportunistic_when_under_threshold_and_cache_cold() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(600), None),
            CompactionAction::PruneOpportunistic
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_hot() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(60), None),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn prune_then_summarize_when_over_threshold_cache_cold() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(600), None),
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
        // Orphan is checked before the cold-window composition — model TTL
        // is irrelevant here, so `None` is the right value, not a stand-in.
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(700), None),
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
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(700), None),
            CompactionAction::Orphan
        );
    }

    #[test]
    fn no_opportunistic_prune_when_prune_after_seconds_is_none() {
        let cfg = CompactionConfig {
            prune_after_seconds: None,
            ..config()
        };
        // Long idle, under token threshold: would normally prune
        // opportunistically; None disables that path entirely.
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(9999), None),
            CompactionAction::None
        );
        // Token-budget pruning still fires when over threshold.
        assert_eq!(
            cfg.determine_action(150_000, CONTEXT_WINDOW, Duration::from_secs(9999), None),
            CompactionAction::PruneThenSummarize
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
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(60), None),
            CompactionAction::None
        );
    }

    /// D1: the model TTL is a floor-raising extension of the role window,
    /// not a replacement for it — role 300s, model 3600s, idle 600s is
    /// cold by the role alone but still hot once the model's longer-lived
    /// provider cache is accounted for.
    #[test]
    fn model_ttl_extends_role_window() {
        let cfg = config(); // prune_after_seconds: Some(300)
        let model_ttl = Some(Duration::from_secs(3600));

        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(600), model_ttl),
            CompactionAction::None,
            "600s idle: past the role's 300s but under the model's 3600s"
        );
        assert_eq!(
            cfg.determine_action(60_000, CONTEXT_WINDOW, Duration::from_secs(3700), model_ttl),
            CompactionAction::PruneOpportunistic,
            "3700s idle: past both windows"
        );
    }

    /// D1: role `None` disables the time path outright — a model TTL
    /// cannot re-enable what the role explicitly turned off.
    #[test]
    fn role_none_disables_even_with_model_ttl() {
        let cfg = CompactionConfig {
            prune_after_seconds: None,
            ..config()
        };
        assert_eq!(
            cfg.determine_action(
                60_000,
                CONTEXT_WINDOW,
                Duration::from_secs(9999),
                Some(Duration::from_secs(60)),
            ),
            CompactionAction::None
        );
    }

    /// The token-budget path doesn't care how the cold window composed —
    /// over threshold always wins regardless of the model TTL.
    #[test]
    fn over_threshold_ignores_model_ttl() {
        let cfg = config();
        assert_eq!(
            cfg.determine_action(
                150_000,
                CONTEXT_WINDOW,
                Duration::from_secs(60),
                Some(Duration::from_secs(3600)),
            ),
            CompactionAction::PruneThenSummarize
        );
    }
}
