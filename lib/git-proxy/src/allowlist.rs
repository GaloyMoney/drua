use serde::{Deserialize, Serialize};

use crate::error::AllowlistError;
use crate::policy::RefPatternSet;
use crate::primitives::GitProxyMode;

/// Serde shape of one allow-list entry as written in `drua.yml`.
/// Validated into [`AllowlistEntry`] at boot — bad config crashes
/// startup rather than silently failing every push.
///
/// The allow-list is GLOBAL: every project that authenticates as an
/// Agent can address every entry. Per-project policy variation lives
/// in `AuthSubject` (the agent's project_id is recorded in the audit
/// row for ops attribution) but the allow / deny decision itself
/// doesn't depend on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowlistEntryConfig {
    pub owner: String,
    pub repo: String,
    #[serde(default)]
    pub allowed_ref_patterns: Vec<String>,
    #[serde(default)]
    pub modes: Vec<GitProxyMode>,
    /// Override the upstream the mirror clones / fetches from. Defaults
    /// to `https://github.com/<owner>/<repo>.git`. Set this for local
    /// bats fixtures (`file:///tmp/upstream.git`) or for self-hosted
    /// GitHub Enterprise. Never user-influenced — config-only.
    #[serde(default)]
    pub upstream_url: Option<String>,
}

/// Top-level allow-list config block; deserialised from the
/// `[git_proxy.allowlist]` section of `drua.yml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowlistConfig {
    #[serde(default)]
    pub entries: Vec<AllowlistEntryConfig>,
}

/// One materialised entry — patterns are pre-compiled at boot so
/// each request is a `GlobSet::is_match` call, not a regex parse.
#[derive(Debug, Clone)]
pub struct AllowlistEntry {
    pub owner: String,
    pub repo: String,
    pub modes: Vec<GitProxyMode>,
    pub patterns: RefPatternSet,
    pub upstream_url: String,
}

impl AllowlistEntry {
    pub fn default_upstream_url(owner: &str, repo: &str) -> String {
        format!("https://github.com/{owner}/{repo}.git")
    }
}

/// In-memory allow-list keyed by `(owner, repo)`. Rebuilt at server
/// boot from [`AllowlistConfig`]; restart to re-load. Per memo
/// `019dfebc` §7.2 the dashboard surface is a follow-up — for MVP,
/// ops edit the YAML and recycle the pod.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    entries: Vec<AllowlistEntry>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compiles config into runtime form. Returns the first invalid
    /// pattern surfaced by `globset` so misconfigs fail loud at boot.
    pub fn from_config(cfg: &AllowlistConfig) -> Result<Self, AllowlistError> {
        let mut entries = Vec::with_capacity(cfg.entries.len());
        for raw in &cfg.entries {
            entries.push(AllowlistEntry {
                owner: raw.owner.clone(),
                repo: raw.repo.clone(),
                modes: raw.modes.clone(),
                patterns: RefPatternSet::new(&raw.allowed_ref_patterns)?,
                upstream_url: raw
                    .upstream_url
                    .clone()
                    .unwrap_or_else(|| AllowlistEntry::default_upstream_url(&raw.owner, &raw.repo)),
            });
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[AllowlistEntry] {
        &self.entries
    }

    /// Case-insensitive on `owner` and `repo` — GitHub treats both
    /// segments as ASCII-case-insensitive on URL fetch (it 301s
    /// `/galoymoney/drua` to `/GaloyMoney/drua`), and stripping the
    /// case-sensitivity at the gate avoids the policy-bypass surface
    /// that case mismatch would otherwise create.
    pub fn lookup(&self, owner: &str, repo: &str) -> Option<&AllowlistEntry> {
        self.entries
            .iter()
            .find(|e| e.owner.eq_ignore_ascii_case(owner) && e.repo.eq_ignore_ascii_case(repo))
    }

    /// Authorization point called per smart-HTTP request. `refs` may be
    /// empty for `info/refs` advertisements — in that case we authorize
    /// the *mode* against the (owner, repo) pair but don't reject on
    /// ref-pattern. For `git-receive-pack` POSTs we reach in again with
    /// the parsed ref list once it's pulled from the pkt-line stream.
    ///
    /// Fail-closed: any miss returns `Err(_)` so the caller can record
    /// the rejection and respond 403 without forwarding upstream.
    pub fn check_authorization(
        &self,
        owner: &str,
        repo: &str,
        mode: GitProxyMode,
        refs: &[String],
    ) -> Result<&AllowlistEntry, AllowlistError> {
        let entry = self
            .lookup(owner, repo)
            .ok_or_else(|| AllowlistError::RepoNotAllowed {
                owner: owner.to_string(),
                repo: repo.to_string(),
            })?;

        if !entry.modes.contains(&mode) {
            return Err(AllowlistError::ModeNotAllowed {
                owner: owner.to_string(),
                repo: repo.to_string(),
                mode,
            });
        }

        for r in refs {
            if !entry.patterns.matches(r) {
                return Err(AllowlistError::RefPatternDenied {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    mode,
                    ref_name: r.clone(),
                });
            }
        }

        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AllowlistConfig {
        AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                owner: "GaloyMoney".into(),
                repo: "drua".into(),
                allowed_ref_patterns: vec!["refs/heads/bot/*".into(), "refs/heads/main".into()],
                modes: vec![GitProxyMode::Pull, GitProxyMode::Push],
                upstream_url: None,
            }],
        }
    }

    #[test]
    fn from_config_rejects_invalid_pattern() {
        let bad = AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                owner: "x".into(),
                repo: "y".into(),
                allowed_ref_patterns: vec!["[unterminated".into()],
                modes: vec![GitProxyMode::Pull],
                upstream_url: None,
            }],
        };
        assert!(matches!(
            Allowlist::from_config(&bad),
            Err(AllowlistError::InvalidRefPattern(_))
        ));
    }

    #[test]
    fn unknown_repo_denied() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        let err = a
            .check_authorization("attacker", "exfil", GitProxyMode::Pull, &[])
            .unwrap_err();
        assert!(matches!(err, AllowlistError::RepoNotAllowed { .. }));
        assert_eq!(err.reject_code(), "repo_not_allowed");
    }

    #[test]
    fn pull_allowed_when_mode_in_set() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        assert!(a
            .check_authorization("GaloyMoney", "drua", GitProxyMode::Pull, &[])
            .is_ok());
    }

    #[test]
    fn lookup_is_case_insensitive_on_owner_and_repo() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        // Allow-list entry is `GaloyMoney/drua`; client typed lowercase.
        assert!(a
            .check_authorization("galoymoney", "drua", GitProxyMode::Pull, &[])
            .is_ok());
        assert!(a
            .check_authorization("GALOYMONEY", "DRUA", GitProxyMode::Pull, &[])
            .is_ok());
        assert!(a
            .check_authorization("GaloyMoney", "DRUA", GitProxyMode::Pull, &[])
            .is_ok());
    }

    #[test]
    fn push_to_main_allowed_when_pattern_matches() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        assert!(a
            .check_authorization(
                "GaloyMoney",
                "drua",
                GitProxyMode::Push,
                &["refs/heads/main".to_string()],
            )
            .is_ok());
    }

    #[test]
    fn push_to_disallowed_ref_rejected_with_code() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        let err = a
            .check_authorization(
                "GaloyMoney",
                "drua",
                GitProxyMode::Push,
                &["refs/heads/release".to_string()],
            )
            .unwrap_err();
        assert!(matches!(err, AllowlistError::RefPatternDenied { .. }));
        assert_eq!(err.reject_code(), "ref_pattern_denied");
    }

    #[test]
    fn mode_not_allowed_when_only_pull_configured() {
        let a = Allowlist::from_config(&AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                owner: "GaloyMoney".into(),
                repo: "drua".into(),
                allowed_ref_patterns: vec!["refs/heads/main".into()],
                modes: vec![GitProxyMode::Pull],
                upstream_url: None,
            }],
        })
        .unwrap();
        let err = a
            .check_authorization("GaloyMoney", "drua", GitProxyMode::Push, &[])
            .unwrap_err();
        assert!(matches!(err, AllowlistError::ModeNotAllowed { .. }));
        assert_eq!(err.reject_code(), "mode_not_allowed");
    }

    #[test]
    fn empty_patterns_denies_every_ref() {
        let a = Allowlist::from_config(&AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                owner: "x".into(),
                repo: "y".into(),
                allowed_ref_patterns: vec![],
                modes: vec![GitProxyMode::Push],
                upstream_url: None,
            }],
        })
        .unwrap();
        assert!(a
            .check_authorization(
                "x",
                "y",
                GitProxyMode::Push,
                &["refs/heads/anything".to_string()],
            )
            .is_err());
    }
}
