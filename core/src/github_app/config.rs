use serde::Deserialize;

/// Configuration for the GitHub App integration.
/// All fields are required — if any env var is missing, the feature is disabled.
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubAppConfig {
    /// The GitHub App's client ID (used as `iss` in the JWT).
    pub client_id: String,
    /// The installation ID for the org where the app is installed.
    pub installation_id: String,
    /// Filesystem path to the PEM-encoded RSA private key (K8s secret mount).
    pub private_key_path: String,
}

impl GitHubAppConfig {
    /// Try to build config from environment variables.
    /// Returns `None` if any required variable is missing (feature disabled).
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("GITHUB_APP_CLIENT_ID").ok()?;
        let installation_id = std::env::var("GITHUB_APP_INSTALLATION_ID").ok()?;
        let private_key_path = std::env::var("GITHUB_APP_PRIVATE_KEY_PATH").ok()?;
        Some(Self {
            client_id,
            installation_id,
            private_key_path,
        })
    }
}
