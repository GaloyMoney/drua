use serde::Deserialize;

/// Configuration for the MCP JWT signer used when forwarding calls to
/// remote MCP upstreams (e.g. the galoy-agents-proxy sidecar on a target
/// deployment). Non-secret fields come from the YAML config; the PEM key
/// path is injected from an env var pointing at a K8s secret mount.
#[derive(Clone, Debug, Deserialize)]
pub struct McpJwtConfig {
    /// Filesystem path to the PEM-encoded RSA private key.
    pub private_key_path: String,
    /// Value used as the `iss` claim. Remote Envoys validate against this.
    pub issuer: String,
    /// Stable identifier for this key — included as `kid` in the JWT
    /// header and the JWKS entry so Envoy can match signatures when
    /// multiple keys are published during rotation.
    pub kid: String,
}
