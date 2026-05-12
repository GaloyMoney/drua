//! Tool-name-keyed preprocessors. Each one transforms the raw upstream
//! text before the generic chain runs, returning both the transformed
//! string and a per-output-line index pointing back to the raw line it
//! came from. The mapping lets the walker translate chain-compacted
//! line offsets all the way back to raw-text line space — necessary
//! because `tool_output_fetch(mode: lines)` slices the persisted raw
//! string, not the preprocessed one.

mod bash;
mod concourse;

/// Output of [`run`]: the preprocessed text plus a per-output-line
/// index pointing back to the raw line each preprocessed line came
/// from. `preprocessed_to_raw[i]` is the raw line index (0-based by
/// `\n`) that contributed preprocessed line `i`.
pub(crate) struct Preprocessed {
    pub text: String,
    pub preprocessed_to_raw: Vec<u32>,
}

/// Run any registered preprocessor whose tool-name set matches. Falls
/// through to the raw input with an identity line-mapping when nothing
/// matches.
pub(crate) fn run(tool_name: &str, raw: &str) -> Preprocessed {
    if bash::TOOL_NAMES.contains(&tool_name) {
        return bash::run(raw);
    }
    if concourse::TOOL_NAMES.contains(&tool_name) {
        return concourse::run(raw);
    }
    // Some upstream-prefixed names look like `<server>_concourse_get_build_logs`;
    // match by suffix so the catalog-prefix doesn't break detection.
    if concourse::TOOL_NAMES.iter().any(|n| tool_name.ends_with(n)) {
        return concourse::run(raw);
    }
    Preprocessed {
        text: raw.to_string(),
        preprocessed_to_raw: identity_mapping(raw),
    }
}

/// Per-line identity mapping for preprocessors that preserve line
/// count. Returns one entry per `raw.lines()` line — `[0, 1, 2, …]`.
pub(crate) fn identity_mapping(raw: &str) -> Vec<u32> {
    (0..raw.lines().count() as u32).collect()
}
