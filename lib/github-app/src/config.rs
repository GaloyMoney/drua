use serde::Deserialize;

/// `private_key_path` is injected from env (K8s secret mount).
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubAppConfig {
    /// JWT `iss`.
    pub client_id: String,
    pub installation_id: String,
    /// Path to PEM-encoded RSA private key.
    pub private_key_path: String,
}
