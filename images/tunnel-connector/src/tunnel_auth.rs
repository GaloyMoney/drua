use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};

pub(crate) fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let pem = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading private key from {}: {e}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("private key is not PKCS#8 Ed25519 PEM: {e}"))
}

/// `Authorization: Tunnel <deployment_id>:<ts_ms>:<sig>`. Fresh ts per call
/// so a stolen header cannot outlive drua's replay window.
pub(crate) fn sign_handshake(deployment_id: &str, signing_key: &SigningKey) -> String {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let payload = format!("{deployment_id}|{ts_ms}");
    let sig = signing_key.sign(payload.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    format!("Tunnel {deployment_id}:{ts_ms}:{sig_b64}")
}
