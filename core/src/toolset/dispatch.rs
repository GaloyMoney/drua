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
