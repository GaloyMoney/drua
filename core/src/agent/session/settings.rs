use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use super::compaction::trigger::CompactionAction;

/// `serde(transparent)` for bare-integer (de)serialization in YAML / JSONB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Cache-aware opportunistic prune trigger: once this many seconds
    /// have passed since the last assistant response the provider's
    /// prompt cache is cold and pruning is free. `None` disables the
    /// time-based path entirely — only token-budget pruning remains.
    /// Anthropic's default cache window is ≈ 300s.
    #[serde(default)]
    pub prune_after_seconds: Option<u64>,
}

impl CompactionConfig {
    /// Cache-aware: pruning invalidates the provider cache, so avoid it
    /// while hot. `Orphan` (idle reset) takes priority over token checks.
    ///
    /// The cold window is `max(prune_after_seconds, model_cache_ttl)` — the
    /// role setting is a floor, never a ceiling: a model whose provider
    /// cache outlives the role's window keeps the thread hot for longer,
    /// but the role window can only be *extended*, never shortened, by the
    /// model. `prune_after_seconds: None` disables the time-based path
    /// entirely regardless of `model_cache_ttl`.
    pub fn determine_action(
        &self,
        estimated_tokens: u64,
        context_window_tokens: u64,
        time_since_last_turn: Duration,
        model_cache_ttl: Option<Duration>,
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
        let cold_after = match (self.prune_after_seconds, model_cache_ttl) {
            (None, _) => None,
            (Some(role), None) => Some(Duration::from_secs(role)),
            (Some(role), Some(model)) => Some(Duration::from_secs(role).max(model)),
        };
        let cache_cold = cold_after.is_some_and(|window| time_since_last_turn > window);

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
            prune_after_seconds: Some(300),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BreakerConfig {
    pub enabled: bool,
    pub consecutive_error_turns: usize,
    pub identical_failing_calls: usize,
    pub consecutive_max_tokens: usize,
    /// Consecutive empty turns (D5) on one thread that advance the model
    /// chain (D7). `0` disables it.
    pub consecutive_empty_turns: usize,
    pub max_turns_per_prompt: usize,
    /// Consecutive discarded responses (un-executable tool arguments) on
    /// one thread, since the last `PromptSent`, before a response is
    /// dispatched instead of re-prompted. `0` disables discarding —
    /// every such response is dispatched (and errors) immediately.
    pub max_tool_call_discards: usize,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            consecutive_error_turns: 15,
            identical_failing_calls: 3,
            consecutive_max_tokens: 3,
            consecutive_empty_turns: 2,
            max_turns_per_prompt: 250,
            max_tool_call_discards: 2,
        }
    }
}

/// Partial override of a role's [`CompactionConfig`], applied once at
/// agent creation — step beats workflow beats role config, the same
/// precedence as `model_chain`. Every field is optional; an unset
/// field inherits the role's value unchanged.
///
/// `prune_after_seconds` and `reset_time_delta_seconds` are doubly
/// optional: the outer `Option` distinguishes "not set in this
/// override" (inherit) from "set", and the inner `Option` is the
/// field's own value — so `Some(None)` explicitly clears it (disables
/// the time path / the idle-reset path) rather than inheriting.
/// Plain serde cannot tell an explicit `null` apart from an absent key
/// here without `deserialize_double_option`, for the same reason the
/// chart's `with` -> `hasKey` fix exists: a naive `Option<Option<T>>`
/// collapses `null` to the outer `None` (inherit), silently discarding
/// the author's intent to disable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompactionOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_threshold_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tool_results: Option<usize>,
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub prune_after_seconds: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub reset_time_delta_seconds: Option<Option<ResetTimeDeltaSeconds>>,
}

/// `#[serde(default)]` on the field handles an absent key (-> outer
/// `None`); this handles a *present* key, wrapping whatever `Option<T>`
/// deserializes to (including `None` from an explicit `null`) in an
/// outer `Some` so the two cases stay distinguishable.
fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

impl CompactionConfig {
    /// `enabled: Some(false)` disables *all* pruning, including the
    /// token-budget path — not just the time path. A step that opts
    /// out via `{enabled: false}` and then genuinely outgrows the
    /// context window fails loudly (a provider context-length error)
    /// rather than silently losing inputs to a prune it asked not to
    /// have; the fix is a smaller workload, not re-enabling pruning.
    pub fn with_override(&self, o: &CompactionOverride) -> Self {
        Self {
            enabled: o.enabled.unwrap_or(self.enabled),
            token_threshold_fraction: o
                .token_threshold_fraction
                .unwrap_or(self.token_threshold_fraction),
            keep_recent_tool_results: o
                .keep_recent_tool_results
                .unwrap_or(self.keep_recent_tool_results),
            prune_after_seconds: o.prune_after_seconds.unwrap_or(self.prune_after_seconds),
            reset_time_delta_seconds: o
                .reset_time_delta_seconds
                .unwrap_or(self.reset_time_delta_seconds),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This is the behaviour the chart's `with` -> `hasKey` fix relies on:
    /// an explicit `null` must reach here as `None` (disabling the
    /// time-based path).
    #[test]
    fn prune_after_seconds_null_deserialises_to_none() {
        let cfg: CompactionConfig = serde_yaml::from_str("prune_after_seconds: null").unwrap();
        assert_eq!(cfg.prune_after_seconds, None);
    }

    /// Contrary to the field doc's `#[serde(default)]` reading in
    /// isolation: `prune_after_seconds` carries its own field-level
    /// `#[serde(default)]`, which takes precedence over
    /// `CompactionConfig`'s struct-level one *for this field* — a key
    /// missing from an otherwise-present `compaction` map resolves via
    /// `Option::default()` (`None`), not the struct's `Some(300)`.
    /// Verified against serde's actual behaviour here, not assumed.
    #[test]
    fn prune_after_seconds_omitted_from_a_present_map_also_deserialises_to_none() {
        let cfg: CompactionConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.prune_after_seconds, None);
    }

    /// The actual chart-level failure mode is one level up: `RoleConfig`'s
    /// `compaction: CompactionConfig` field is *also* a bare
    /// `#[serde(default)]`, but there the missing-field fallback uses
    /// `CompactionConfig::default()` as a whole (`Some(300)`) because the
    /// container, not a leaf field, is what's missing. Helm's override
    /// merge deletes a key set to `null`; for a role whose `compaction`
    /// block has only `pruneAfterSeconds` configured (`workflow_step_agent`
    /// today), nulling that one key collapses the entire `compaction` map,
    /// which is indistinguishable here from "role never set compaction".
    #[test]
    fn compaction_key_entirely_absent_from_role_falls_back_to_the_300s_default() {
        use crate::agent::RoleConfig;

        let role: RoleConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(role.compaction.prune_after_seconds, Some(300));
    }

    #[test]
    fn empty_override_is_identity() {
        let role = CompactionConfig {
            enabled: true,
            token_threshold_fraction: 0.6,
            keep_recent_tool_results: 10,
            reset_time_delta_seconds: Some(ResetTimeDeltaSeconds(600)),
            prune_after_seconds: Some(300),
        };
        let merged = role.with_override(&CompactionOverride::default());
        assert_eq!(merged.enabled, role.enabled);
        assert_eq!(
            merged.token_threshold_fraction,
            role.token_threshold_fraction
        );
        assert_eq!(
            merged.keep_recent_tool_results,
            role.keep_recent_tool_results
        );
        assert_eq!(
            merged.reset_time_delta_seconds,
            role.reset_time_delta_seconds
        );
        assert_eq!(merged.prune_after_seconds, role.prune_after_seconds);
    }

    #[test]
    fn enabled_false_override_short_circuits_determine_action_for_every_input() {
        let role = CompactionConfig::default();
        let overridden = role.with_override(&CompactionOverride {
            enabled: Some(false),
            ..Default::default()
        });
        assert_eq!(
            overridden.determine_action(999_999, 200_000, Duration::from_secs(99_999), None),
            CompactionAction::None,
            "enabled: false disables the token path too (D7), not just the time path"
        );
    }

    #[test]
    fn prune_after_seconds_double_none_clears_the_time_path() {
        let role = CompactionConfig::default(); // prune_after_seconds: Some(300)
        let overridden = role.with_override(&CompactionOverride {
            prune_after_seconds: Some(None),
            ..Default::default()
        });
        assert_eq!(overridden.prune_after_seconds, None);
        // Threshold path is untouched by the override.
        assert_eq!(
            overridden.determine_action(150_000, 200_000, Duration::from_secs(9_999), None),
            CompactionAction::PruneThenSummarize
        );
    }

    #[test]
    fn override_prune_after_seconds_null_deserialises_as_explicit_clear() {
        let o: CompactionOverride = serde_yaml::from_str("prune_after_seconds: null").unwrap();
        assert_eq!(
            o.prune_after_seconds,
            Some(None),
            "present-with-null must be distinguishable from the key being absent"
        );
    }

    #[test]
    fn override_prune_after_seconds_absent_key_means_inherit() {
        let o: CompactionOverride = serde_yaml::from_str("{}").unwrap();
        assert_eq!(o.prune_after_seconds, None, "absent key means \"inherit\"");
    }

    #[test]
    fn override_prune_after_seconds_set_value_deserialises_as_explicit_set() {
        let o: CompactionOverride = serde_yaml::from_str("prune_after_seconds: 3600").unwrap();
        assert_eq!(o.prune_after_seconds, Some(Some(3600)));
    }
}
