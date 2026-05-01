//! Stable doc-id + title/body extraction for space files. The full
//! reverse-sync pipeline now lives in the unified `LibrarySyncRunner`;
//! this module only retains the deterministic identity helpers shared
//! with `SpaceFilesImporter`.

use uuid::Uuid;

use crate::primitives::SpaceId;

/// Frozen namespace UUID — changing it would invalidate every existing
/// `space_search_data` row's identity.
const SPACE_FILE_NAMESPACE: Uuid = Uuid::from_u128(0x6c4d339d_2184_4fa9_9f12_6e375b8291ae);

/// Deterministic `doc_id` for a space file. Idempotent re-imports rely
/// on this: same `(space, path)` always hashes the same UUID.
pub(crate) fn doc_id_for(space_id: SpaceId, relative_path: &str) -> Uuid {
    let key = format!("{}:{relative_path}", uuid::Uuid::from(space_id));
    Uuid::new_v5(&SPACE_FILE_NAMESPACE, key.as_bytes())
}

/// First H1 line wins; falls back to filename stem with `-`/`_` → spaces.
pub(crate) fn extract_title_and_body(content: &str, path: &str) -> (String, String) {
    for line in content.lines().take(20) {
        if let Some(title) = line.trim_start().strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return (title.to_string(), content.to_string());
            }
        }
    }
    let fallback = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.replace(['-', '_'], " "))
        .unwrap_or_else(|| path.to_string());
    (fallback, content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_id_is_deterministic() {
        let space = SpaceId::new();
        let a = doc_id_for(space, "runbooks/foo.md");
        let b = doc_id_for(space, "runbooks/foo.md");
        assert_eq!(a, b);
    }

    #[test]
    fn doc_id_changes_with_path() {
        let space = SpaceId::new();
        let a = doc_id_for(space, "runbooks/foo.md");
        let b = doc_id_for(space, "runbooks/bar.md");
        assert_ne!(a, b);
    }

    #[test]
    fn doc_id_changes_with_space() {
        let path = "runbooks/foo.md";
        let a = doc_id_for(SpaceId::new(), path);
        let b = doc_id_for(SpaceId::new(), path);
        assert_ne!(a, b);
    }

    #[test]
    fn extract_title_uses_first_h1() {
        let (title, _) =
            extract_title_and_body("# Incident playbook\n\nbody text\n", "runbooks/foo.md");
        assert_eq!(title, "Incident playbook");
    }

    #[test]
    fn extract_title_falls_back_to_filename() {
        let (title, _) = extract_title_and_body(
            "no heading here\nbody text\n",
            "runbooks/incident-playbook.md",
        );
        assert_eq!(title, "incident playbook");
    }

    #[test]
    fn extract_title_skips_empty_h1() {
        let (title, _) = extract_title_and_body("# \n\nbody\n", "runbooks/incident-playbook.md");
        assert_eq!(title, "incident playbook");
    }
}
