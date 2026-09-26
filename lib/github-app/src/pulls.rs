//! Thin REST wrapper over the GitHub Pulls API — no octocrab, just the
//! three calls the staged-changesets handoff needs (§10):
//! `create_pull`/`get_pull`/`close_pull`. Reuses the crate's existing
//! JWT → installation-token flow; each call fetches a fresh token
//! rather than caching one, matching `generate_token`'s own "verify at
//! startup, call fresh at use" contract.

use serde::Deserialize;

use crate::{GitHubAppError, GitHubAppTokenProvider};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
    pub state: String,
    pub merged: bool,
    pub merge_commit_sha: Option<String>,
}

#[derive(Deserialize)]
struct PullResponse {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
}

impl From<PullResponse> for PullRequest {
    fn from(r: PullResponse) -> Self {
        Self {
            number: r.number,
            html_url: r.html_url,
            state: r.state,
            merged: r.merged,
            merge_commit_sha: r.merge_commit_sha,
        }
    }
}

impl GitHubAppTokenProvider {
    /// Opens a PR `head` → `base`. `head` is a bare branch name (same
    /// repo) — GitHub's API accepts `owner:branch` for cross-fork PRs,
    /// which this crate has no use for since drua only pushes to its
    /// own library repo.
    #[tracing::instrument(name = "github_app.create_pull", skip(self, title, body))]
    pub async fn create_pull(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, GitHubAppError> {
        let token = self.generate_token().await?;
        let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls");
        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "drua")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({
                "title": title,
                "head": head,
                "base": base,
                "body": body,
            }))
            .send()
            .await?;
        parse_pull_response(resp).await
    }

    #[tracing::instrument(name = "github_app.get_pull", skip(self))]
    pub async fn get_pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubAppError> {
        let token = self.generate_token().await?;
        let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}");
        let resp = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "drua")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        parse_pull_response(resp).await
    }

    /// Closes `number` without merging. `comment`, when given, is
    /// posted as an issue comment first (the two are separate GitHub
    /// API calls; PRs are issues for commenting purposes) — used by
    /// `Changesets::apply`/`discard` to leave a trail explaining why a
    /// PR closed without a merge.
    #[tracing::instrument(name = "github_app.close_pull", skip(self, comment))]
    pub async fn close_pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        comment: Option<&str>,
    ) -> Result<(), GitHubAppError> {
        let token = self.generate_token().await?;

        if let Some(comment) = comment {
            let url =
                format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments");
            let resp = self
                .http_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", token.token))
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "drua")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&serde_json::json!({ "body": comment }))
                .send()
                .await?;
            check_status(resp).await?;
        }

        let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}");
        let resp = self
            .http_client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", token.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "drua")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, GitHubAppError> {
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = resp
            .text()
            .await
            .unwrap_or_else(|_| "unknown error".to_string());
        tracing::warn!(status, %message, "GitHub Pulls API call failed");
        return Err(GitHubAppError::ApiError { status, message });
    }
    Ok(resp)
}

async fn parse_pull_response(resp: reqwest::Response) -> Result<PullRequest, GitHubAppError> {
    let resp = check_status(resp).await?;
    let parsed: PullResponse = resp.json().await?;
    Ok(parsed.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_response_defaults_unmerged_without_a_merge_sha() {
        let json = serde_json::json!({
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "state": "open",
        });
        let parsed: PullResponse = serde_json::from_value(json).unwrap();
        let pr: PullRequest = parsed.into();
        assert_eq!(pr.number, 42);
        assert!(!pr.merged);
        assert_eq!(pr.merge_commit_sha, None);
    }

    #[test]
    fn pull_response_carries_merge_sha_when_merged() {
        let json = serde_json::json!({
            "number": 7,
            "html_url": "https://github.com/o/r/pull/7",
            "state": "closed",
            "merged": true,
            "merge_commit_sha": "deadbeef",
        });
        let parsed: PullResponse = serde_json::from_value(json).unwrap();
        let pr: PullRequest = parsed.into();
        assert!(pr.merged);
        assert_eq!(pr.merge_commit_sha.as_deref(), Some("deadbeef"));
    }
}
