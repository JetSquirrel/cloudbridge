//! Encryption module - for encrypting stored AK/SK

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::RngCore;

const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;

/// Encryption manager
pub struct CryptoManager {
    cipher: Aes256Gcm,
}

impl CryptoManager {
    /// Create encryption manager with key
    pub fn new(key: &[u8; KEY_SIZE]) -> Self {
        let cipher = Aes256Gcm::new_from_slice(key).expect("Invalid key size");
        Self { cipher }
    }

    /// Generate new encryption key
    pub fn generate_key() -> [u8; KEY_SIZE] {
        let mut key = [0u8; KEY_SIZE];
        OsRng.fill_bytes(&mut key);
        key
    }

    /// Encode key to Base64 string (for storage)
    pub fn key_to_string(key: &[u8; KEY_SIZE]) -> String {
        BASE64.encode(key)
    }

    /// Decode key from Base64 string
    pub fn key_from_string(key_str: &str) -> Result<[u8; KEY_SIZE]> {
        let decoded = BASE64.decode(key_str)?;
        if decoded.len() != KEY_SIZE {
            return Err(anyhow!("Invalid key length"));
        }
        let mut key = [0u8; KEY_SIZE];
        key.copy_from_slice(&decoded);
        Ok(key)
    }

    /// Encrypt data
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce_bytes = [0u8; NONCE_SIZE];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow!("Encryption failed: {}", e))?;

        // Encode nonce and ciphertext together
        let mut combined = nonce_bytes.to_vec();
        combined.extend(ciphertext);

        Ok(BASE64.encode(&combined))
    }

    /// Decrypt data
    pub fn decrypt(&self, encrypted: &str) -> Result<String> {
        let combined = BASE64.decode(encrypted)?;

        if combined.len() < NONCE_SIZE {
            return Err(anyhow!("Invalid encrypted data"));
        }

        let (nonce_bytes, ciphertext) = combined.split_at(NONCE_SIZE);
        let nonce = Nonce::from_slice(nonce_bytes);

        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("Decryption failed: {}", e))?;

        String::from_utf8(plaintext).map_err(|e| anyhow!("UTF-8 decode failed: {}", e))
    }
}

/// Get or create encryption manager
pub fn get_crypto_manager() -> Result<CryptoManager> {
    use crate::config::{load_config, save_config};

    let mut config = load_config()?;

    let key = if let Some(ref key_str) = config.encryption_key {
        CryptoManager::key_from_string(key_str)?
    } else {
        // Generate new key and save
        let key = CryptoManager::generate_key();
        config.encryption_key = Some(CryptoManager::key_to_string(&key));
        save_config(&config)?;
        key
    };

    Ok(CryptoManager::new(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let key = CryptoManager::generate_key();
        let manager = CryptoManager::new(&key);

        let plaintext = "AKIAIOSFODNN7EXAMPLE";
        let encrypted = manager.encrypt(plaintext).unwrap();
        let decrypted = manager.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_key_serialization() {
        let key = CryptoManager::generate_key();
        let key_str = CryptoManager::key_to_string(&key);
        let restored_key = CryptoManager::key_from_string(&key_str).unwrap();

        assert_eq!(key, restored_key);
    }
}
