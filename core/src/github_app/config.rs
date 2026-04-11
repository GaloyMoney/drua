use serde::Deserialize;

/// Configuration for the GitHub App integration.
/// Passed through `AppConfig` — non-secret fields come from the YAML config file,
/// while `private_key_path` is injected from an env var (K8s secret mount).
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubAppConfig {
    /// The GitHub App's client ID (used as `iss` in the JWT).
    pub client_id: String,
    /// The installation ID for the org where the app is installed.
    pub installation_id: String,
    /// Filesystem path to the PEM-encoded RSA private key (K8s secret mount).
    pub private_key_path: String,
}
