use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Generates a cryptographically random 32-byte token.
///
/// Returns `(raw_token, token_hash)` where:
/// - `raw_token` is base64url-encoded (return to caller ONCE, never store)
/// - `token_hash` is the SHA-256 hex digest (safe to store in DB)
pub fn generate_token() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let raw_token = URL_SAFE_NO_PAD.encode(bytes);
    let token_hash = hash_token(&raw_token);
    (raw_token, token_hash)
}

/// SHA-256 hash a raw token string, returning a hex-encoded digest.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    bytes_to_hex(&digest)
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_returns_unique_values() {
        let (t1, h1) = generate_token();
        let (t2, h2) = generate_token();
        assert_ne!(t1, t2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_token_is_deterministic() {
        let hash1 = hash_token("test-token");
        let hash2 = hash_token("test-token");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_token_returns_64_char_hex() {
        let hash = hash_token("anything");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generated_token_is_base64url() {
        let (token, _) = generate_token();
        assert!(URL_SAFE_NO_PAD.decode(&token).is_ok());
    }
}
