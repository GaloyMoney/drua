pub mod config;
pub mod error;

pub use config::*;
pub use error::*;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};

/// Signs short-lived RS256 JWTs for outbound MCP calls to remote proxies,
/// and exposes the matching public key as a JWKS document so remote Envoys
/// can verify signatures via `remote_jwks`.
///
/// Intended to be built once at startup and shared (Arc-cloned) across
/// every `RemoteProxyToolSet`.
#[derive(Clone)]
pub struct McpJwtSigner {
    issuer: String,
    kid: String,
    encoding_key: jsonwebtoken::EncodingKey,
    /// Public modulus (base64url, no padding) — cached for JWKS.
    public_n: String,
    /// Public exponent (base64url, no padding) — cached for JWKS.
    public_e: String,
}

/// JWKS document as served at `/.well-known/jwks.json`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Jwks {
    pub keys: Vec<JwkEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct JwkEntry {
    pub kty: String,
    #[serde(rename = "use")]
    pub use_: String,
    pub alg: String,
    pub kid: String,
    pub n: String,
    pub e: String,
}

impl McpJwtSigner {
    pub fn new(config: &McpJwtConfig) -> Result<Self, McpJwtError> {
        tracing::info!(
            private_key_path = %config.private_key_path,
            issuer = %config.issuer,
            kid = %config.kid,
            "Initializing MCP JWT signer"
        );

        let pem_bytes =
            std::fs::read(&config.private_key_path).map_err(McpJwtError::PrivateKeyRead)?;

        // Parse once for public key extraction (JWKS).
        let pem_str = std::str::from_utf8(&pem_bytes)
            .map_err(|e| McpJwtError::PrivateKeyParse(e.to_string()))?;
        let rsa_key = RsaPrivateKey::from_pkcs8_pem(pem_str)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem_str))
            .map_err(|e| McpJwtError::PrivateKeyParse(e.to_string()))?;
        let public_key = rsa_key.to_public_key();
        let public_n = URL_SAFE_NO_PAD.encode(public_key.n().to_bytes_be());
        let public_e = URL_SAFE_NO_PAD.encode(public_key.e().to_bytes_be());

        // Parse again for signing (jsonwebtoken's internal representation).
        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem_bytes)?;

        Ok(Self {
            issuer: config.issuer.clone(),
            kid: config.kid.clone(),
            encoding_key,
            public_n,
            public_e,
        })
    }

    /// Mint a JWT with the given audience and TTL. `sub` identifies the
    /// caller-scope claim (currently a static value — sandbox-id threading
    /// is a follow-up).
    pub fn mint(&self, audience: &str, subject: &str, ttl_seconds: i64) -> Result<String, McpJwtError> {
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": self.issuer,
            "aud": audience,
            "sub": subject,
            "iat": now - 60,
            "exp": now + ttl_seconds,
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding_key)?;
        Ok(token)
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public key in JWKS form. Intended to be served at
    /// `/.well-known/jwks.json`.
    pub fn jwks(&self) -> Jwks {
        Jwks {
            keys: vec![JwkEntry {
                kty: "RSA".to_string(),
                use_: "sig".to_string(),
                alg: "RS256".to_string(),
                kid: self.kid.clone(),
                n: self.public_n.clone(),
                e: self.public_e.clone(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::EncodePrivateKey;

    fn write_test_key() -> (tempfile::NamedTempFile, RsaPrivateKey) {
        let mut rng = chacha20poly1305::aead::OsRng;
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), pem.as_bytes()).unwrap();
        (file, key)
    }

    #[test]
    fn mints_and_verifies() {
        let (file, rsa_key) = write_test_key();
        let config = McpJwtConfig {
            private_key_path: file.path().to_string_lossy().to_string(),
            issuer: "galoy-agents".to_string(),
            kid: "test-kid".to_string(),
        };
        let signer = McpJwtSigner::new(&config).unwrap();

        let token = signer
            .mint("mcp.staging.galoy.io", "galoy-agents", 60)
            .unwrap();

        // Parse header and claims to verify structure.
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&["mcp.staging.galoy.io"]);
        validation.set_issuer(&["galoy-agents"]);

        use rsa::pkcs1::EncodeRsaPublicKey;
        let pub_der = rsa_key.to_public_key().to_pkcs1_der().unwrap();
        let decoding_key =
            jsonwebtoken::DecodingKey::from_rsa_der(pub_der.as_bytes());
        let data =
            jsonwebtoken::decode::<serde_json::Value>(&token, &decoding_key, &validation).unwrap();
        assert_eq!(data.claims["sub"], "galoy-agents");
        assert_eq!(data.header.kid.as_deref(), Some("test-kid"));
    }

    #[test]
    fn jwks_contains_public_key() {
        let (file, _) = write_test_key();
        let config = McpJwtConfig {
            private_key_path: file.path().to_string_lossy().to_string(),
            issuer: "galoy-agents".to_string(),
            kid: "test-kid".to_string(),
        };
        let signer = McpJwtSigner::new(&config).unwrap();

        let jwks = signer.jwks();
        assert_eq!(jwks.keys.len(), 1);
        assert_eq!(jwks.keys[0].kid, "test-kid");
        assert_eq!(jwks.keys[0].kty, "RSA");
        assert_eq!(jwks.keys[0].alg, "RS256");
        assert!(!jwks.keys[0].n.is_empty());
        assert!(!jwks.keys[0].e.is_empty());
    }

    /// End-to-end round trip matching what a remote Envoy does:
    /// 1. Fetch JWKS (our `/.well-known/jwks.json` output).
    /// 2. Reconstruct a verifying key from `n` and `e` via
    ///    `DecodingKey::from_rsa_components`.
    /// 3. Verify a freshly minted JWT against that key.
    ///
    /// If this passes, a correctly-configured Envoy `jwt_authn` filter
    /// fetching our JWKS will accept our tokens.
    #[test]
    fn jwks_round_trip_verifies_minted_jwt() {
        let (file, _) = write_test_key();
        let config = McpJwtConfig {
            private_key_path: file.path().to_string_lossy().to_string(),
            issuer: "galoy-agents".to_string(),
            kid: "round-trip-kid".to_string(),
        };
        let signer = McpJwtSigner::new(&config).unwrap();

        // Step 1: serialize and deserialize the JWKS (as Envoy would fetch).
        let jwks_json = serde_json::to_string(&signer.jwks()).unwrap();
        let fetched_jwks: Jwks = serde_json::from_str(&jwks_json).unwrap();
        let entry = &fetched_jwks.keys[0];

        // Step 2: reconstruct the decoding key purely from the JWKS
        // components, without access to the private key or the
        // EncodingKey used to sign.
        let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(&entry.n, &entry.e)
            .expect("reconstruct decoding key from JWKS n/e");

        // Step 3: mint a token and verify it with that reconstructed key.
        let token = signer
            .mint("mcp.staging.galoy.io", "sandbox-xyz", 60)
            .unwrap();

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&["mcp.staging.galoy.io"]);
        validation.set_issuer(&["galoy-agents"]);

        let data =
            jsonwebtoken::decode::<serde_json::Value>(&token, &decoding_key, &validation).unwrap();
        assert_eq!(data.header.kid.as_deref(), Some("round-trip-kid"));
        assert_eq!(data.claims["sub"], "sandbox-xyz");
        assert_eq!(data.claims["aud"], "mcp.staging.galoy.io");
        assert_eq!(data.claims["iss"], "galoy-agents");
    }

    /// A token whose audience doesn't match the verifier's expected
    /// audience must be rejected — same posture remote Envoys enforce.
    #[test]
    fn jwks_round_trip_rejects_wrong_audience() {
        let (file, _) = write_test_key();
        let config = McpJwtConfig {
            private_key_path: file.path().to_string_lossy().to_string(),
            issuer: "galoy-agents".to_string(),
            kid: "aud-kid".to_string(),
        };
        let signer = McpJwtSigner::new(&config).unwrap();

        let entry = &signer.jwks().keys[0].clone();
        let decoding_key =
            jsonwebtoken::DecodingKey::from_rsa_components(&entry.n, &entry.e).unwrap();

        // Token minted for staging, but verifier expects qa — must fail.
        let token = signer
            .mint("mcp.staging.galoy.io", "sub", 60)
            .unwrap();

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&["mcp.qa.galoy.io"]);
        validation.set_issuer(&["galoy-agents"]);

        let err = jsonwebtoken::decode::<serde_json::Value>(&token, &decoding_key, &validation)
            .expect_err("wrong audience must be rejected");
        assert!(
            matches!(
                err.kind(),
                jsonwebtoken::errors::ErrorKind::InvalidAudience
            ),
            "expected InvalidAudience, got {err:?}"
        );
    }
}
