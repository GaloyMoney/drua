//! Shared dispatch helpers — used by both the catalog tool and compose's
//! catalog dispatcher. Single source of truth for visibility + prefix
//! handling across the two paths.

use std::sync::Arc;

use crate::auth::AuthSubject;

use super::traits::SearchableToolSet;

/// Find the searchable toolset that owns `prefixed_name` (e.g.
/// `honeycomb_list_environments`) and is visible to `subject`. Returns the
/// matched set and the unprefixed tool name.
pub(crate) fn find_searchable<'a, I>(
    sets: I,
    subject: &AuthSubject,
    prefixed_name: &str,
) -> Option<(Arc<dyn SearchableToolSet>, String)>
where
    I: IntoIterator<Item = &'a Arc<dyn SearchableToolSet>>,
{
    for set in sets {
        if !set.is_visible(subject) {
            continue;
        }
        let prefix = format!("{}_", set.prefix());
        if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
            if set.tools().iter().any(|t| t.name == tool_name) {
                return Some((Arc::clone(set), tool_name.to_string()));
            }
        }
    }
    None
}

/// Spellings to try for a model-supplied tool name, most literal first, so
/// an exact match always beats a rewritten one (`drua_admin_*` are real
/// names and are only stripped when nothing matches as given). Callers
/// resolve a candidate through their normal visibility-gated lookup, so a
/// rewrite can never reach a tool the caller couldn't reach by its real name.
pub(crate) fn candidate_tool_names(name: &str) -> Vec<String> {
    fn push(out: &mut Vec<String>, candidate: &str) {
        if !candidate.is_empty() && !out.iter().any(|c| c == candidate) {
            out.push(candidate.to_string());
        }
    }
    fn collapse_doubled(name: &str) -> Option<String> {
        let (first, rest) = name.split_once('_')?;
        let rest_first = rest.split('_').next()?;
        (first == rest_first).then(|| rest.to_string())
    }

    let mut out = Vec::new();
    push(&mut out, name);
    let trimmed = name.trim().trim_end_matches('.');
    push(&mut out, trimmed);
    let undotted = trimmed.replace('.', "_");
    push(&mut out, &undotted);
    let unprefixed = undotted
        .strip_prefix("mcp__drua__")
        .unwrap_or(&undotted)
        .to_string();
    push(&mut out, &unprefixed);
    let drua_stripped = unprefixed
        .strip_prefix("drua_")
        .unwrap_or(&unprefixed)
        .to_string();
    push(&mut out, &drua_stripped);
    for base in [unprefixed, drua_stripped] {
        if let Some(collapsed) = collapse_doubled(&base) {
            push(&mut out, &collapsed);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_start_with_the_name_as_given() {
        assert_eq!(
            candidate_tool_names("drua_admin_spaces")[0],
            "drua_admin_spaces"
        );
        assert!(candidate_tool_names("drua_admin_spaces").contains(&"admin_spaces".to_string()));
    }

    #[test]
    fn candidates_cover_each_transformation() {
        let has = |input: &str, want: &str| {
            let got = candidate_tool_names(input);
            assert!(
                got.iter().any(|c| c == want),
                "{input} -> {got:?}, want {want}"
            );
        };
        has("  github_pull_request_read", "github_pull_request_read");
        has("github.pull_request_read", "github_pull_request_read");
        has("mcp__drua__tool_output_fetch", "tool_output_fetch");
        has("drua_describe_tool", "describe_tool");
        has("drua_call_tool", "call_tool");
        has("library_library_search", "library_search");
        has("zenduty_zenduty_get_incident", "zenduty_get_incident");
        has("drua_library_library_search", "library_search");
        has("drua_Read", "Read");
    }

    #[test]
    fn candidates_are_unique_and_never_empty_strings() {
        let got = candidate_tool_names("read");
        assert_eq!(got, vec!["read".to_string()]);
        assert!(candidate_tool_names("").is_empty());
    }
}
