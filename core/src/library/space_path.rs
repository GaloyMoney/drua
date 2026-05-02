//! Parser + validator for `space:<slug>/<rel>` paths used by the
//! sandboxless space read tools.

use super::SpaceError;

/// Parsed view of a `space:<slug>` or `space:<slug>/<rel>` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceRef<'a> {
    pub slug: &'a str,
    /// Empty for the space root (`space:<slug>` or `space:<slug>/`).
    pub rel_path: &'a str,
}

/// Returns `Some(SpaceRef)` iff `path` starts with the `space:` prefix
/// and has a non-empty slug. Anything else returns `None` so callers
/// can fall through to the existing sandbox dispatch.
pub fn parse(path: &str) -> Option<SpaceRef<'_>> {
    let rest = path.strip_prefix("space:")?;
    let (slug, rel) = match rest.split_once('/') {
        Some((slug, rel)) => (slug, rel),
        None => (rest, ""),
    };
    if slug.is_empty() {
        return None;
    }
    Some(SpaceRef {
        slug,
        rel_path: rel,
    })
}

/// Rejects path-traversal, absolute paths, NUL bytes, and leading `/`.
/// Empty `rel` is allowed (means "the space root").
pub fn validate_rel_path(rel: &str) -> Result<(), SpaceError> {
    if rel.is_empty() {
        return Ok(());
    }
    if rel.contains('\0') || rel.starts_with('/') || rel.starts_with('\\') {
        return Err(invalid(rel));
    }
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(invalid(rel));
        }
    }
    Ok(())
}

fn invalid(rel: &str) -> SpaceError {
    SpaceError::InvalidRelPath {
        path: rel.to_string(),
        reason: "must be relative; no '..', leading '/', empty segments, or NUL bytes".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_prefix_and_slug() {
        let r = parse("space:oncall/runbooks/foo.md").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "runbooks/foo.md");
    }

    #[test]
    fn parse_root_with_trailing_slash() {
        let r = parse("space:oncall/").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_root_without_slash() {
        let r = parse("space:oncall").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_returns_none_for_non_space_paths() {
        assert!(parse("/etc/passwd").is_none());
        assert!(parse("oncall/foo.md").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn parse_rejects_empty_slug() {
        assert!(parse("space:").is_none());
        assert!(parse("space:/foo").is_none());
    }

    #[test]
    fn validate_rejects_traversal() {
        assert!(validate_rel_path("../etc").is_err());
        assert!(validate_rel_path("foo/../etc").is_err());
        assert!(validate_rel_path("./foo").is_err());
    }

    #[test]
    fn validate_rejects_absolute() {
        assert!(validate_rel_path("/etc/passwd").is_err());
        assert!(validate_rel_path("\\windows").is_err());
    }

    #[test]
    fn validate_rejects_nul() {
        assert!(validate_rel_path("foo\0bar").is_err());
    }

    #[test]
    fn validate_rejects_double_slash() {
        // Empty segment from `//` collapses to ".." style invalid.
        assert!(validate_rel_path("foo//bar").is_err());
    }

    #[test]
    fn validate_accepts_normal_paths() {
        assert!(validate_rel_path("README.md").is_ok());
        assert!(validate_rel_path("runbooks/foo/bar.md").is_ok());
        assert!(validate_rel_path("").is_ok());
    }
}
