use globset::{Glob, GlobSet, GlobSetBuilder};

use super::primitives::GitProxyMode;

/// Compiled set of ref-name globs (e.g. `refs/heads/bot/*`).
/// Empty set means "no refs allowed" — fail-closed by construction.
#[derive(Clone, Debug)]
pub struct RefPatternSet {
    set: GlobSet,
    raw: Vec<String>,
}

impl RefPatternSet {
    pub fn new(patterns: &[String]) -> Result<Self, globset::Error> {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            builder.add(Glob::new(p)?);
        }
        Ok(Self {
            set: builder.build()?,
            raw: patterns.to_vec(),
        })
    }

    pub fn matches(&self, ref_name: &str) -> bool {
        self.set.is_match(ref_name)
    }

    pub fn raw(&self) -> &[String] {
        &self.raw
    }
}

/// Decision whether `mode` is in `allowed`.
pub fn mode_allowed(allowed: &[GitProxyMode], mode: GitProxyMode) -> bool {
    allowed.contains(&mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(patterns: &[&str]) -> RefPatternSet {
        RefPatternSet::new(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn empty_set_rejects_everything() {
        let s = set(&[]);
        assert!(!s.matches("refs/heads/main"));
        assert!(!s.matches("refs/heads/bot/foo"));
        assert!(!s.matches(""));
    }

    #[test]
    fn bot_glob_matches_subpaths() {
        let s = set(&["refs/heads/bot/*"]);
        assert!(s.matches("refs/heads/bot/heal-check"));
        assert!(s.matches("refs/heads/bot/x"));
        assert!(!s.matches("refs/heads/main"));
        assert!(!s.matches("refs/heads/feature/bot/x"));
    }

    #[test]
    fn exact_main_pattern() {
        let s = set(&["refs/heads/main"]);
        assert!(s.matches("refs/heads/main"));
        assert!(!s.matches("refs/heads/main2"));
        assert!(!s.matches("refs/heads/maint"));
    }

    #[test]
    fn multi_segment_glob() {
        let s = set(&["refs/heads/bot/**"]);
        assert!(s.matches("refs/heads/bot/a"));
        assert!(s.matches("refs/heads/bot/a/b/c"));
        assert!(!s.matches("refs/heads/main"));
    }

    #[test]
    fn multiple_patterns_or_combined() {
        let s = set(&["refs/heads/bot/*", "refs/heads/main"]);
        assert!(s.matches("refs/heads/bot/x"));
        assert!(s.matches("refs/heads/main"));
        assert!(!s.matches("refs/heads/topic"));
    }

    #[test]
    fn invalid_glob_surfaces_error() {
        let bad = vec!["[unterminated".to_string()];
        assert!(RefPatternSet::new(&bad).is_err());
    }

    #[test]
    fn mode_membership() {
        assert!(mode_allowed(
            &[GitProxyMode::Pull, GitProxyMode::Push],
            GitProxyMode::Push
        ));
        assert!(mode_allowed(&[GitProxyMode::Pull], GitProxyMode::Pull));
        assert!(!mode_allowed(&[GitProxyMode::Pull], GitProxyMode::Push));
        assert!(!mode_allowed(&[], GitProxyMode::Pull));
    }
}
