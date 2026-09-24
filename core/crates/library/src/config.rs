#[derive(Clone)]
pub struct LibraryConfig {
    pub data_dir: String,
    pub repo_url: String,
    /// How often the fetcher task pulls from origin.
    pub fetch_interval_ms: u64,
}

impl LibraryConfig {
    /// `(owner, repo)` when `repo_url` is a `github.com` remote (HTTPS
    /// or SSH); `None` for a local filesystem path (dev/test fixtures)
    /// or any other host — `Changesets::submit` treats `None` as
    /// `ChangesetError::PrUnavailable` (handoff OQ-14).
    pub fn github_coord(&self) -> Option<(String, String)> {
        let url = self.repo_url.trim();
        let rest = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("http://github.com/"))
            .or_else(|| url.strip_prefix("git@github.com:"))?;
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let (owner, repo) = rest.split_once('/')?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        Some((owner.to_string(), repo.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repo_url: &str) -> LibraryConfig {
        LibraryConfig {
            data_dir: "/tmp/x".to_string(),
            repo_url: repo_url.to_string(),
            fetch_interval_ms: 1000,
        }
    }

    #[test]
    fn github_coord_parses_https() {
        assert_eq!(
            cfg("https://github.com/GaloyMoney/drua-library").github_coord(),
            Some(("GaloyMoney".to_string(), "drua-library".to_string()))
        );
    }

    #[test]
    fn github_coord_strips_dot_git_suffix() {
        assert_eq!(
            cfg("https://github.com/GaloyMoney/drua-library.git").github_coord(),
            Some(("GaloyMoney".to_string(), "drua-library".to_string()))
        );
    }

    #[test]
    fn github_coord_parses_ssh() {
        assert_eq!(
            cfg("git@github.com:GaloyMoney/drua-library.git").github_coord(),
            Some(("GaloyMoney".to_string(), "drua-library".to_string()))
        );
    }

    #[test]
    fn github_coord_none_for_local_path() {
        assert_eq!(cfg("/tmp/some/bare/repo.git").github_coord(), None);
        assert_eq!(cfg("./tests/.library").github_coord(), None);
    }

    #[test]
    fn github_coord_none_for_other_hosts() {
        assert_eq!(
            cfg("https://gitlab.com/GaloyMoney/drua-library").github_coord(),
            None
        );
    }
}
