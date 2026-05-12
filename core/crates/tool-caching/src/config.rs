use serde::{Deserialize, Serialize};

const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 8192;
const DEFAULT_SENTINEL_HARD_CAP_BYTES: usize = 16 * 1024;
const DEFAULT_SENTINEL_MIN_BYTES: usize = 512;
const DEFAULT_MAX_FETCH_RESPONSE_BYTES: usize = 16 * 1024;

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
}

impl Default for ToolCachingConfig {
    fn default() -> Self {
        Self {
            generic_threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
            sentinel_min_bytes: DEFAULT_SENTINEL_MIN_BYTES,
            sentinel_hard_cap_bytes: DEFAULT_SENTINEL_HARD_CAP_BYTES,
            max_fetch_response_bytes: DEFAULT_MAX_FETCH_RESPONSE_BYTES,
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
