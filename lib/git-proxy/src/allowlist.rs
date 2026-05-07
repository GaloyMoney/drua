use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AllowlistError;
use crate::policy::RefPatternSet;
use crate::primitives::GitProxyMode;

/// Serde shape of one allow-list entry as written in `drua.yml`.
/// Validated into [`AllowlistEntry`] at boot — bad config crashes
/// startup rather than silently failing every push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowlistEntryConfig {
    pub project_id: Uuid,
    pub owner: String,
    pub repo: String,
    #[serde(default)]
    pub allowed_ref_patterns: Vec<String>,
    #[serde(default)]
    pub modes: Vec<GitProxyMode>,
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
    pub project_id: Uuid,
    pub owner: String,
    pub repo: String,
    pub modes: Vec<GitProxyMode>,
    pub patterns: RefPatternSet,
}

/// In-memory allow-list keyed by `(project_id, owner, repo)`.
/// Rebuilt at server boot from [`AllowlistConfig`]; restart to
/// re-load. Per memo `019dfebc` §7.2 the dashboard surface is a
/// follow-up — for MVP, ops edit the YAML and recycle the pod.
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
                project_id: raw.project_id,
                owner: raw.owner.clone(),
                repo: raw.repo.clone(),
                modes: raw.modes.clone(),
                patterns: RefPatternSet::new(&raw.allowed_ref_patterns)?,
            });
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[AllowlistEntry] {
        &self.entries
    }

    pub fn lookup(&self, project_id: Uuid, owner: &str, repo: &str) -> Option<&AllowlistEntry> {
        self.entries
            .iter()
            .find(|e| e.project_id == project_id && e.owner == owner && e.repo == repo)
    }

    /// Authorization point called per smart-HTTP request. `refs` may be
    /// empty for `info/refs` advertisements — in that case we authorize
    /// the *mode* against the (project, owner, repo) triple but don't
    /// reject on ref-pattern. For `git-receive-pack` POSTs we reach in
    /// again with the parsed ref list once it's pulled from the
    /// pkt-line stream.
    ///
    /// Fail-closed: any miss returns `Err(_)` so the caller can record
    /// the rejection and respond 403 without forwarding upstream.
    pub fn check_authorization(
        &self,
        project_id: Uuid,
        owner: &str,
        repo: &str,
        mode: GitProxyMode,
        refs: &[String],
    ) -> Result<&AllowlistEntry, AllowlistError> {
        let entry =
            self.lookup(project_id, owner, repo)
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
                project_id: Uuid::nil(),
                owner: "GaloyMoney".into(),
                repo: "drua".into(),
                allowed_ref_patterns: vec!["refs/heads/bot/*".into(), "refs/heads/main".into()],
                modes: vec![GitProxyMode::Pull, GitProxyMode::Push],
            }],
        }
    }

    #[test]
    fn from_config_rejects_invalid_pattern() {
        let bad = AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                project_id: Uuid::nil(),
                owner: "x".into(),
                repo: "y".into(),
                allowed_ref_patterns: vec!["[unterminated".into()],
                modes: vec![GitProxyMode::Pull],
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
            .check_authorization(Uuid::nil(), "attacker", "exfil", GitProxyMode::Pull, &[])
            .unwrap_err();
        assert!(matches!(err, AllowlistError::RepoNotAllowed { .. }));
        assert_eq!(err.reject_code(), "repo_not_allowed");
    }

    #[test]
    fn other_project_does_not_leak_entry() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        let other = Uuid::new_v4();
        assert!(a
            .check_authorization(other, "GaloyMoney", "drua", GitProxyMode::Pull, &[])
            .is_err());
    }

    #[test]
    fn pull_allowed_when_mode_in_set() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        assert!(a
            .check_authorization(Uuid::nil(), "GaloyMoney", "drua", GitProxyMode::Pull, &[])
            .is_ok());
    }

    #[test]
    fn push_to_main_allowed_when_pattern_matches() {
        let a = Allowlist::from_config(&cfg()).unwrap();
        assert!(a
            .check_authorization(
                Uuid::nil(),
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
                Uuid::nil(),
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
                project_id: Uuid::nil(),
                owner: "GaloyMoney".into(),
                repo: "drua".into(),
                allowed_ref_patterns: vec!["refs/heads/main".into()],
                modes: vec![GitProxyMode::Pull],
            }],
        })
        .unwrap();
        let err = a
            .check_authorization(Uuid::nil(), "GaloyMoney", "drua", GitProxyMode::Push, &[])
            .unwrap_err();
        assert!(matches!(err, AllowlistError::ModeNotAllowed { .. }));
        assert_eq!(err.reject_code(), "mode_not_allowed");
    }

    #[test]
    fn empty_patterns_denies_every_ref() {
        let a = Allowlist::from_config(&AllowlistConfig {
            entries: vec![AllowlistEntryConfig {
                project_id: Uuid::nil(),
                owner: "x".into(),
                repo: "y".into(),
                allowed_ref_patterns: vec![],
                modes: vec![GitProxyMode::Push],
            }],
        })
        .unwrap();
        assert!(a
            .check_authorization(
                Uuid::nil(),
                "x",
                "y",
                GitProxyMode::Push,
                &["refs/heads/anything".to_string()],
            )
            .is_err());
    }
}
