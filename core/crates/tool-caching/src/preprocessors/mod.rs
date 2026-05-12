//! Tool-name-keyed preprocessors. Each one is a `&str -> String`
//! transform that runs before the generic chain when the upstream
//! tool name matches.

mod bash;
mod concourse;

/// Run any registered preprocessor whose tool-name set matches.
/// Falls through to the input unchanged when nothing matches.
pub(crate) fn run(tool_name: &str, raw: &str) -> String {
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
    raw.to_string()
}
