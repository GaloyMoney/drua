pub mod config;
pub mod error;

use tracing::instrument;

pub use config::*;
pub use error::*;

pub struct InstallationToken {
    pub token: String,
    pub expires_at: String,
}

/// RS256 JWT flow → GitHub installation access tokens. Designed to be shared.
#[derive(Clone)]
pub struct GitHubAppTokenProvider {
    client_id: String,
    installation_id: String,
    encoding_key: jsonwebtoken::EncodingKey,
    http_client: reqwest::Client,
}

impl GitHubAppTokenProvider {
    pub fn new(config: &GitHubAppConfig) -> Result<Self, GitHubAppError> {
        tracing::info!(
            private_key_path = %config.private_key_path,
            client_id = %config.client_id,
            installation_id = %config.installation_id,
            "Initializing GitHub App token provider"
        );
        let pem_bytes =
            std::fs::read(&config.private_key_path).map_err(GitHubAppError::PrivateKeyRead)?;
        tracing::info!(pem_bytes_len = pem_bytes.len(), "Read PEM private key");
        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem_bytes)?;
        Ok(Self {
            client_id: config.client_id.clone(),
            installation_id: config.installation_id.clone(),
            encoding_key,
            http_client: reqwest::Client::new(),
        })
    }

    #[instrument(name = "github_app.generate_token", skip_all)]
    pub async fn generate_token(&self) -> Result<InstallationToken, GitHubAppError> {
        let jwt = self.sign_jwt()?;
        self.exchange_for_installation_token(&jwt).await
    }

    /// iss=client_id, iat=now-60s (clock drift), exp=now+10min.
    fn sign_jwt(&self) -> Result<String, GitHubAppError> {
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": self.client_id,
            "iat": now - 60,
            "exp": now + (10 * 60),
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding_key)?;
        Ok(token)
    }

    async fn exchange_for_installation_token(
        &self,
        jwt: &str,
    ) -> Result<InstallationToken, GitHubAppError> {
        let url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            self.installation_id
        );

        let body = installation_token_request_body();

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "drua")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await?;

        let resp_status = resp.status().as_u16();
        if !resp.status().is_success() {
            let message = resp
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            tracing::warn!(status = resp_status, %message, "GitHub API token exchange failed");
            return Err(GitHubAppError::ApiError {
                status: resp_status,
                message,
            });
        }
        tracing::info!(status = resp_status, "GitHub API token exchange succeeded");

        let json: serde_json::Value = resp.json().await?;
        let token = json["token"].as_str().unwrap_or_default().to_string();
        let expires_at = json["expires_at"].as_str().unwrap_or_default().to_string();

        Ok(InstallationToken { token, expires_at })
    }
}

fn installation_token_request_body() -> serde_json::Value {
    serde_json::json!({
        "permissions": {
            "contents": "write",
            "pull_requests": "write",
            "issues": "write",
            "metadata": "read",
            "checks": "read",
            "statuses": "read",
            "actions": "write"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::installation_token_request_body;

    #[test]
    fn installation_token_requests_ci_permissions() {
        let body = installation_token_request_body();
        let permissions = body
            .get("permissions")
            .and_then(|v| v.as_object())
            .expect("permissions object");

        assert_eq!(
            permissions.get("checks").and_then(|v| v.as_str()),
            Some("read")
        );
        assert_eq!(
            permissions.get("statuses").and_then(|v| v.as_str()),
            Some("read")
        );
        assert_eq!(
            permissions.get("actions").and_then(|v| v.as_str()),
            Some("write")
        );
    }

    #[test]
    fn installation_token_preserves_existing_permissions() {
        let body = installation_token_request_body();
        let permissions = body
            .get("permissions")
            .and_then(|v| v.as_object())
            .expect("permissions object");

        assert_eq!(
            permissions.get("contents").and_then(|v| v.as_str()),
            Some("write")
        );
        assert_eq!(
            permissions.get("pull_requests").and_then(|v| v.as_str()),
            Some("write")
        );
        assert_eq!(
            permissions.get("issues").and_then(|v| v.as_str()),
            Some("write")
        );
        assert_eq!(
            permissions.get("metadata").and_then(|v| v.as_str()),
            Some("read")
        );
    }
}
