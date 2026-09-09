use std::collections::HashMap;

use serde::{Deserialize, Serialize};

const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 8192;
const DEFAULT_SENTINEL_HARD_CAP_BYTES: usize = 16 * 1024;
const DEFAULT_SENTINEL_MIN_BYTES: usize = 512;
const DEFAULT_MAX_FETCH_RESPONSE_BYTES: usize = 16 * 1024;
// Inlined bytes are re-read on every later turn of the session, so the
// break-even against one recovery turn (~25k tokens, mostly cache
// reads) sits in the single-digit KB — not the 32 KB a per-turn
// accounting suggests. Measured Aug–Sep 2026: tools with <15% recovery
// rate (`library_search`, `*_resources_get`) paid 2–3x more context
// under a 32 KB floor than under elision.
const DEFAULT_MIN_HIDDEN_BYTES: usize = 6 * 1024;
// Documents are read in full 40–70% of the time, so eliding them mostly
// defers the bytes to a fetch and costs a turn; only genuinely large
// ones are worth withholding.
const DEFAULT_DOCUMENT_MIN_HIDDEN_BYTES: usize = 64 * 1024;
// Every elision point costs ~350 bytes of recovery metadata on the
// wire, so a path that hides less than this spends more context than
// it withholds — the wide object whose per-key budget nicks every
// 500-byte cell was the canonical case.
const DEFAULT_MIN_HIDDEN_PER_PATH_BYTES: usize = 2048;

/// Knobs the walker actually reads. Per-string head/tail counts are
/// derived adaptively from `generic_threshold_bytes` at walk time;
/// per-array head/tail counts are derived from `sentinel_min_bytes` /
/// `sentinel_hard_cap_bytes` via a shrink loop.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCachingConfig {
    /// Strings / containers larger than this trigger elision.
    #[serde(default = "default_generic_threshold_bytes")]
    pub generic_threshold_bytes: usize,
    /// Lower bound for the array-sentinel size-shrink loop budget.
    #[serde(default = "default_sentinel_min_bytes")]
    pub sentinel_min_bytes: usize,
    /// Upper bound for the array-sentinel size-shrink loop budget.
    #[serde(default = "default_sentinel_hard_cap_bytes")]
    pub sentinel_hard_cap_bytes: usize,
    /// Ceiling on a single `tool_output_fetch` response. Recovery
    /// templates the walker emits stay under this by construction; ad
    /// hoc fetches with a bigger `len` or no `query` get rejected with
    /// `FetchResponseTooLarge`.
    #[serde(default = "default_max_fetch_response_bytes")]
    pub max_fetch_response_bytes: usize,
    /// Below this many hidden bytes (`total_bytes - shown_bytes` at the
    /// primary path), skip eliding entirely and pass the whole value
    /// through — the round trip to recover it would cost more than the
    /// bytes it hides.
    #[serde(default = "default_min_hidden_bytes")]
    pub min_hidden_bytes: usize,
    /// `min_hidden_bytes` for tools declaring [`ToolOutputShape::Document`].
    #[serde(default = "default_document_min_hidden_bytes")]
    pub document_min_hidden_bytes: usize,
    /// Below this many hidden bytes at a single elision point, leave
    /// that value whole — its recovery template would outweigh the
    /// saving. `Log` shapes ignore it, like the root floor.
    #[serde(default = "default_min_hidden_per_path_bytes")]
    pub min_hidden_per_path_bytes: usize,
    /// Per-tool overrides, keyed by the prefixed tool name (e.g.
    /// `library_get_files`). Falls back to the fields above when a tool
    /// has no entry, or when an entry doesn't set a given field.
    #[serde(default)]
    pub per_tool: HashMap<String, ToolCachingOverride>,
}

/// Per-tool override for a subset of [`ToolCachingConfig`]'s fields.
/// Unset fields fall back to the global default.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCachingOverride {
    #[serde(default)]
    pub generic_threshold_bytes: Option<usize>,
    #[serde(default)]
    pub min_hidden_bytes: Option<usize>,
}

/// What kind of payload a tool emits, declared by the tool itself (a
/// trait method for in-process tools, upstream config for proxied MCP
/// servers) and passed down at call time. Read-fraction — the share of
/// a payload the caller actually wants — is a property of the tool, not
/// of the byte count, so the tool is the only place that knows this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputShape {
    /// Structured results, documents, API responses. Elision competes
    /// with the recovery round trip, so the min-hidden-bytes floor
    /// applies.
    #[default]
    Generic,
    /// A raw log or dump: the caller wants the tail, reads a fraction of
    /// it, and rarely recovers the rest. Elision earns its keep at any
    /// size here, so the floor does not apply.
    Log,
    /// A document the caller usually wants whole (space files, source
    /// files, PR bodies). Recovery is likely, so eliding mostly defers
    /// the bytes to a fetch and costs a turn: the floor is raised to
    /// `document_min_hidden_bytes`.
    Document,
}

/// The elision knobs resolved for one call, in precedence order:
/// global config → the tool's declared [`ToolOutputShape`] → operator
/// `per_tool` override. [`crate::Walker`] consumes these and makes no
/// per-tool decisions of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElisionBudget {
    pub threshold_bytes: usize,
    pub min_hidden_bytes: usize,
    pub min_hidden_per_path_bytes: usize,
}

impl ToolCachingConfig {
    pub fn budget_for(&self, tool_name: &str, shape: ToolOutputShape) -> ElisionBudget {
        let mut budget = ElisionBudget {
            threshold_bytes: self.generic_threshold_bytes,
            min_hidden_bytes: match shape {
                ToolOutputShape::Generic => self.min_hidden_bytes,
                ToolOutputShape::Log => 0,
                ToolOutputShape::Document => self.document_min_hidden_bytes,
            },
            min_hidden_per_path_bytes: match shape {
                ToolOutputShape::Log => 0,
                ToolOutputShape::Generic | ToolOutputShape::Document => {
                    self.min_hidden_per_path_bytes
                }
            },
        };
        // Operator config is last so a deployment can always overrule a
        // tool's own declaration.
        if let Some(over) = self.per_tool.get(tool_name) {
            if let Some(threshold) = over.generic_threshold_bytes {
                budget.threshold_bytes = threshold;
            }
            if let Some(min_hidden) = over.min_hidden_bytes {
                budget.min_hidden_bytes = min_hidden;
            }
        }
        budget
    }
}

impl Default for ToolCachingConfig {
    fn default() -> Self {
        Self {
            generic_threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
            sentinel_min_bytes: DEFAULT_SENTINEL_MIN_BYTES,
            sentinel_hard_cap_bytes: DEFAULT_SENTINEL_HARD_CAP_BYTES,
            max_fetch_response_bytes: DEFAULT_MAX_FETCH_RESPONSE_BYTES,
            min_hidden_bytes: DEFAULT_MIN_HIDDEN_BYTES,
            document_min_hidden_bytes: DEFAULT_DOCUMENT_MIN_HIDDEN_BYTES,
            min_hidden_per_path_bytes: DEFAULT_MIN_HIDDEN_PER_PATH_BYTES,
            per_tool: HashMap::new(),
        }
    }
}

fn default_generic_threshold_bytes() -> usize {
    DEFAULT_GENERIC_THRESHOLD_BYTES
}
fn default_sentinel_hard_cap_bytes() -> usize {
    DEFAULT_SENTINEL_HARD_CAP_BYTES
}
fn default_sentinel_min_bytes() -> usize {
    DEFAULT_SENTINEL_MIN_BYTES
}
fn default_max_fetch_response_bytes() -> usize {
    DEFAULT_MAX_FETCH_RESPONSE_BYTES
}
fn default_min_hidden_bytes() -> usize {
    DEFAULT_MIN_HIDDEN_BYTES
}
fn default_document_min_hidden_bytes() -> usize {
    DEFAULT_DOCUMENT_MIN_HIDDEN_BYTES
}
fn default_min_hidden_per_path_bytes() -> usize {
    DEFAULT_MIN_HIDDEN_PER_PATH_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_tool_override_deserializes_partial_fields() {
        let cfg: ToolCachingConfig = serde_json::from_value(serde_json::json!({
            "per_tool": {
                "library_get_files": { "generic_threshold_bytes": 65536 },
            }
        }))
        .unwrap();
        let over = cfg.per_tool.get("library_get_files").unwrap();
        assert_eq!(over.generic_threshold_bytes, Some(65536));
        assert_eq!(over.min_hidden_bytes, None);
        // Untouched globals keep their defaults.
        assert_eq!(cfg.generic_threshold_bytes, DEFAULT_GENERIC_THRESHOLD_BYTES);
        assert_eq!(cfg.min_hidden_bytes, DEFAULT_MIN_HIDDEN_BYTES);
    }

    #[test]
    fn absent_config_yields_defaults() {
        let cfg: ToolCachingConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.min_hidden_bytes, DEFAULT_MIN_HIDDEN_BYTES);
        assert!(cfg.per_tool.is_empty());
    }

    #[test]
    fn generic_shape_takes_the_global_budget() {
        let cfg = ToolCachingConfig::default();
        assert_eq!(
            cfg.budget_for("some_tool", ToolOutputShape::Generic),
            ElisionBudget {
                threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
                min_hidden_bytes: DEFAULT_MIN_HIDDEN_BYTES,
                min_hidden_per_path_bytes: DEFAULT_MIN_HIDDEN_PER_PATH_BYTES,
            }
        );
    }

    /// A log tool elides at any size: the caller reads a fraction of the
    /// payload and rarely recovers the rest, so the round-trip argument
    /// the floor encodes doesn't apply.
    #[test]
    fn log_shape_drops_the_floor_but_keeps_the_threshold() {
        let cfg = ToolCachingConfig::default();
        let budget = cfg.budget_for("some_log_tool", ToolOutputShape::Log);
        assert_eq!(budget.min_hidden_bytes, 0);
        assert_eq!(budget.min_hidden_per_path_bytes, 0);
        assert_eq!(budget.threshold_bytes, DEFAULT_GENERIC_THRESHOLD_BYTES);
    }

    #[test]
    fn per_tool_override_beats_the_declared_shape() {
        let mut per_tool = HashMap::new();
        per_tool.insert(
            "noisy_log_tool".to_string(),
            ToolCachingOverride {
                generic_threshold_bytes: Some(65_536),
                min_hidden_bytes: Some(4_096),
            },
        );
        let cfg = ToolCachingConfig {
            per_tool,
            ..ToolCachingConfig::default()
        };

        // Log would zero the floor; the operator's 4 KB wins.
        assert_eq!(
            cfg.budget_for("noisy_log_tool", ToolOutputShape::Log),
            ElisionBudget {
                threshold_bytes: 65_536,
                min_hidden_bytes: 4_096,
                // The shape's per-path floor (0 for Log) is not overridable.
                min_hidden_per_path_bytes: 0,
            }
        );
        // Tools without an entry are untouched by it.
        assert_eq!(
            cfg.budget_for("other_tool", ToolOutputShape::Log)
                .threshold_bytes,
            DEFAULT_GENERIC_THRESHOLD_BYTES
        );
    }

    #[test]
    fn document_shape_raises_the_floor() {
        let cfg = ToolCachingConfig::default();
        let budget = cfg.budget_for("library_get_files", ToolOutputShape::Document);
        assert_eq!(budget.min_hidden_bytes, DEFAULT_DOCUMENT_MIN_HIDDEN_BYTES);
        assert!(budget.min_hidden_bytes > cfg.min_hidden_bytes);
        assert_eq!(budget.threshold_bytes, DEFAULT_GENERIC_THRESHOLD_BYTES);
    }

    /// A partial override leaves the field it doesn't set to whatever the
    /// shape resolved it to.
    #[test]
    fn partial_override_leaves_other_fields_to_the_shape() {
        let mut per_tool = HashMap::new();
        per_tool.insert(
            "doc_tool".to_string(),
            ToolCachingOverride {
                generic_threshold_bytes: Some(65_536),
                min_hidden_bytes: None,
            },
        );
        let cfg = ToolCachingConfig {
            per_tool,
            ..ToolCachingConfig::default()
        };
        let budget = cfg.budget_for("doc_tool", ToolOutputShape::Log);
        assert_eq!(budget.threshold_bytes, 65_536);
        assert_eq!(budget.min_hidden_bytes, 0, "shape still applies");
    }
}
