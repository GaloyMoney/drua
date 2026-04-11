pub mod config;
pub mod error;

use tracing::instrument;

pub use config::*;
pub use error::*;

/// A successfully generated GitHub installation access token.
pub struct InstallationToken {
    pub token: String,
    pub expires_at: String,
}

/// Generates GitHub App installation access tokens via the RS256 JWT flow.
///
/// Holds the cached `EncodingKey` and HTTP client. Designed to be shared
/// across requests (put it in `Arc` or `Clone` the wrapper).
#[derive(Clone)]
pub struct GitHubAppTokenProvider {
    client_id: String,
    installation_id: String,
    encoding_key: jsonwebtoken::EncodingKey,
    http_client: reqwest::Client,
}

impl GitHubAppTokenProvider {
    /// Create a new provider by reading the PEM private key from disk.
    /// Returns an error only if the key file cannot be read or parsed.
    pub fn new(config: &GitHubAppConfig) -> Result<Self, GitHubAppError> {
        let pem_bytes =
            std::fs::read(&config.private_key_path).map_err(GitHubAppError::PrivateKeyRead)?;
        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem_bytes)?;
        Ok(Self {
            client_id: config.client_id.clone(),
            installation_id: config.installation_id.clone(),
            encoding_key,
            http_client: reqwest::Client::new(),
        })
    }

    /// Generate a fresh installation access token from the GitHub API.
    ///
    /// 1. Signs an RS256 JWT (10 min expiry) with the app's client_id as `iss`.
    /// 2. POSTs to `/app/installations/{id}/access_tokens` with scoped permissions.
    /// 3. Returns the token string and its expiration timestamp.
    #[instrument(name = "github_app.generate_token", skip_all)]
    pub async fn generate_token(&self) -> Result<InstallationToken, GitHubAppError> {
        let jwt = self.sign_jwt()?;
        self.exchange_for_installation_token(&jwt).await
    }

    /// Sign an RS256 JWT for GitHub App authentication.
    /// Claims: iss = client_id, iat = now - 60s (clock drift), exp = now + 10min.
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

    /// Exchange the JWT for an installation access token with scoped permissions.
    async fn exchange_for_installation_token(
        &self,
        jwt: &str,
    ) -> Result<InstallationToken, GitHubAppError> {
        let url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            self.installation_id
        );

        let body = serde_json::json!({
            "permissions": {
                "contents": "write",
                "pull_requests": "write",
                "issues": "write",
                "metadata": "read"
            }
        });

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "galoy-agents")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(GitHubAppError::ApiError { status, message });
        }

        let json: serde_json::Value = resp.json().await?;
        let token = json["token"].as_str().unwrap_or_default().to_string();
        let expires_at = json["expires_at"].as_str().unwrap_or_default().to_string();

        Ok(InstallationToken { token, expires_at })
    }
}
