//! Tool-name-keyed preprocessors. Each one owns its tool's canonical
//! shape: given the upstream's structured root, it returns a new root
//! with the text field(s) in place transformed, plus a json-path locating
//! the transformed text and a line-mapping back to the raw bytes.
//!
//! Why structured-aware: a preprocessor like concourse's strips ANSI +
//! timestamps + `\r`-progress reflow from the build log. The upstream
//! schema is `{logs: <raw>}`, so the preprocessor knows to read+write
//! the `logs` key. The walker just walks the (possibly preprocessed)
//! root — it doesn't need per-leaf preprocessor matching.

mod concourse;

use serde_json::Value;

/// Result of a preprocessor run — the transformed root, the json-path
/// where the preprocessed text now lives (used as the `<summary>`
/// `path` attribute), and a per-output-line index back to the raw
/// bytes (used by line-mode recovery templates).
pub(crate) struct PreprocessedRoot {
    pub root: Value,
    pub root_path: String,
    pub preprocessed_to_raw: Vec<u32>,
    /// True when the interesting payload is at the END of the text
    /// (e.g. CI build logs: failure trace lives in the last lines).
    /// Walker biases the head/tail split toward the tail when set.
    pub prefer_tail: bool,
}

/// Run any registered preprocessor that claims `tool_name` AND finds
/// its text inside `root`. Returns `None` when no preprocessor matches —
/// the walker should walk `root` unchanged with identity line mapping.
pub(crate) fn preprocess(tool_name: &str, root: &Value) -> Option<PreprocessedRoot> {
    concourse::preprocess(tool_name, root)
    // bash is currently identity-only; nothing to do here for it.
}

/// Per-line identity mapping for non-matched paths. One entry per
/// `raw.lines()` line — `[0, 1, 2, …]`.
pub(crate) fn identity_mapping(raw: &str) -> Vec<u32> {
    (0..raw.lines().count() as u32).collect()
}
