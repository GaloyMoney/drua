use chacha20poly1305::{
    aead::{Aead, OsRng},
    AeadCore, ChaCha20Poly1305, KeyInit,
};
use serde::{Deserialize, Serialize};

/// Identifies which key was used to encrypt a value, enabling key rotation detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionKeyId([u8; 4]);

/// A ChaCha20-Poly1305 encryption key with a derived ID for rotation tracking.
#[derive(Clone)]
pub struct EncryptionKey {
    key: chacha20poly1305::Key,
    id: EncryptionKeyId,
}

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptionKey")
            .field("id", &self.id)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl EncryptionKey {
    pub fn new(raw: [u8; 32]) -> Self {
        let key = chacha20poly1305::Key::from(raw);
        // Derive a 4-byte ID from the key material for rotation tracking
        let hash = <sha2::Sha256 as sha2::Digest>::digest(raw);
        let mut id = [0u8; 4];
        id.copy_from_slice(&hash[..4]);
        Self {
            key,
            id: EncryptionKeyId(id),
        }
    }

    /// Encrypt arbitrary bytes, returning an `Encrypted` envelope.
    pub fn encrypt(&self, plaintext: &[u8]) -> Encrypted {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .expect("encryption should not fail");
        Encrypted {
            ciphertext,
            nonce: nonce.to_vec(),
            key_id: self.id.clone(),
        }
    }

    /// Decrypt an `Encrypted` envelope back to plaintext bytes.
    pub fn decrypt(&self, encrypted: &Encrypted) -> Result<Vec<u8>, EncryptionError> {
        if encrypted.key_id != self.id {
            return Err(EncryptionError::KeyMismatch);
        }
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = chacha20poly1305::Nonce::from_slice(&encrypted.nonce);
        cipher
            .decrypt(nonce, encrypted.ciphertext.as_ref())
            .map_err(|_| EncryptionError::DecryptionFailed)
    }

    /// Encrypt a string value.
    pub fn encrypt_string(&self, value: &str) -> Encrypted {
        self.encrypt(value.as_bytes())
    }

    /// Decrypt to a UTF-8 string.
    pub fn decrypt_string(&self, encrypted: &Encrypted) -> Result<String, EncryptionError> {
        let bytes = self.decrypt(encrypted)?;
        String::from_utf8(bytes).map_err(|_| EncryptionError::DecryptionFailed)
    }
}

impl Default for EncryptionKey {
    fn default() -> Self {
        Self::new([0u8; 32])
    }
}

/// An encrypted value with its nonce and key ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Encrypted {
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_id: EncryptionKeyId,
}

impl Encrypted {
    /// Check whether this value was encrypted with the given key.
    pub fn matches_key(&self, key: &EncryptionKey) -> bool {
        self.key_id == key.id
    }
}

#[derive(thiserror::Error, Debug)]
pub enum EncryptionError {
    #[error("EncryptionError - KeyMismatch: ciphertext was encrypted with a different key")]
    KeyMismatch,
    #[error("EncryptionError - DecryptionFailed")]
    DecryptionFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = EncryptionKey::new([42u8; 32]);
        let plaintext = "super-secret-value";
        let encrypted = key.encrypt_string(plaintext);
        let decrypted = key.decrypt_string(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = EncryptionKey::new([1u8; 32]);
        let key2 = EncryptionKey::new([2u8; 32]);
        let encrypted = key1.encrypt_string("secret");
        assert!(key2.decrypt_string(&encrypted).is_err());
    }

    #[test]
    fn matches_key_works() {
        let key = EncryptionKey::new([99u8; 32]);
        let encrypted = key.encrypt_string("test");
        assert!(encrypted.matches_key(&key));

        let other = EncryptionKey::new([100u8; 32]);
        assert!(!encrypted.matches_key(&other));
    }
}
