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

    /// Parses a GitHub clone URL. Accepts the four forms `git clone`
    /// understands (`https://`, `https://…/repo.git`, `git@github.com:…`,
    /// `git@github.com:….git`); returns `None` for anything else
    /// (self-hosted GHE, mirror.example.com, etc. — those would need
    /// their own allow-list lookup path).
    pub fn from_github_url(url: &str) -> Option<Self> {
        let after_host = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("http://github.com/"))
            .or_else(|| url.strip_prefix("git@github.com:"))?;
        let after_host = after_host.trim_end_matches('/');
        let (owner, repo) = after_host.split_once('/')?;
        Self::parse(owner, repo)
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
    fn from_github_url_handles_clone_url_forms() {
        let cases = [
            "https://github.com/GaloyMoney/drua",
            "https://github.com/GaloyMoney/drua.git",
            "https://github.com/GaloyMoney/drua/",
            "http://github.com/GaloyMoney/drua",
            "git@github.com:GaloyMoney/drua",
            "git@github.com:GaloyMoney/drua.git",
        ];
        for url in cases {
            let r = RepoCoord::from_github_url(url)
                .unwrap_or_else(|| panic!("expected Some for {url}"));
            assert_eq!(r.owner, "GaloyMoney");
            assert_eq!(r.repo, "drua");
        }
    }

    #[test]
    fn from_github_url_rejects_non_github_hosts() {
        assert!(RepoCoord::from_github_url("https://gitlab.com/x/y").is_none());
        assert!(RepoCoord::from_github_url("https://example.com/x/y").is_none());
        assert!(RepoCoord::from_github_url("not-a-url").is_none());
        assert!(RepoCoord::from_github_url("https://github.com/").is_none());
        assert!(RepoCoord::from_github_url("https://github.com/owner-only").is_none());
    }

    #[test]
    fn service_to_mode() {
        assert_eq!(GitService::GitUploadPack.mode(), GitProxyMode::Pull);
        assert_eq!(GitService::GitReceivePack.mode(), GitProxyMode::Push);
    }
}
