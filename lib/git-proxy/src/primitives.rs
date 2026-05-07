use serde::{Deserialize, Serialize};

/// Operations a sandbox may perform against a repo through the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitProxyMode {
    Pull,
    Push,
}

impl std::fmt::Display for GitProxyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitProxyMode::Pull => write!(f, "pull"),
            GitProxyMode::Push => write!(f, "push"),
        }
    }
}

/// Smart-HTTP service identifier as it appears on the wire.
/// The proxy maps these 1:1 to [`GitProxyMode`] for policy lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitService {
    GitUploadPack,
    GitReceivePack,
}

impl GitService {
    pub fn mode(self) -> GitProxyMode {
        match self {
            GitService::GitUploadPack => GitProxyMode::Pull,
            GitService::GitReceivePack => GitProxyMode::Push,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GitService::GitUploadPack => "git-upload-pack",
            GitService::GitReceivePack => "git-receive-pack",
        }
    }

    pub fn from_query(s: &str) -> Option<Self> {
        match s {
            "git-upload-pack" => Some(GitService::GitUploadPack),
            "git-receive-pack" => Some(GitService::GitReceivePack),
            _ => None,
        }
    }
}

impl std::fmt::Display for GitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decision recorded for every proxy attempt; written to
/// `sandbox_git_proxy_attempts.decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitProxyDecision {
    Accepted,
    Rejected,
}

impl GitProxyDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            GitProxyDecision::Accepted => "accepted",
            GitProxyDecision::Rejected => "rejected",
        }
    }
}

/// Identifies the (owner, repo) target on the GitHub side. Validated
/// at parse time — owner + repo must match GitHub's character set so a
/// malicious sandbox can't inject path-traversal into the mirror layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCoord {
    pub owner: String,
    pub repo: String,
}

impl RepoCoord {
    /// Per GitHub: owners and repo names are `[A-Za-z0-9._-]`.
    /// Reject anything else fail-closed; the URL path is attacker-influenced.
    pub fn parse(owner: &str, repo: &str) -> Option<Self> {
        fn ok(s: &str) -> bool {
            !s.is_empty()
                && !s.starts_with('.')
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if !ok(owner) || !ok(repo) {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_coord_accepts_normal_names() {
        let r = RepoCoord::parse("GaloyMoney", "drua.git").unwrap();
        assert_eq!(r.owner, "GaloyMoney");
        assert_eq!(r.repo, "drua");
    }

    #[test]
    fn repo_coord_rejects_path_traversal() {
        assert!(RepoCoord::parse("..", "x").is_none());
        assert!(RepoCoord::parse("a/b", "x").is_none());
        assert!(RepoCoord::parse("a", "b/c").is_none());
        assert!(RepoCoord::parse("", "x").is_none());
        assert!(RepoCoord::parse("a", "").is_none());
        assert!(RepoCoord::parse(".hidden", "x").is_none());
    }

    #[test]
    fn git_service_round_trip() {
        for s in ["git-upload-pack", "git-receive-pack"] {
            assert_eq!(GitService::from_query(s).unwrap().as_str(), s);
        }
        assert!(GitService::from_query("git-archive").is_none());
    }

    #[test]
    fn service_to_mode() {
        assert_eq!(GitService::GitUploadPack.mode(), GitProxyMode::Pull);
        assert_eq!(GitService::GitReceivePack.mode(), GitProxyMode::Push);
    }
}
