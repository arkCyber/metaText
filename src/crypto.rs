/*!
 * crypto.rs
 *
 * Cryptographic operations for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Authenticated encryption with ChaCha20-Poly1305
 * - Cryptographically secure random key generation
 * - Password based key derivation with Argon2
 * - Random nonce generation with nonce prefixing
 */

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use tracing::{info, warn};

use crate::error::{MetaTextError, MetaTextResult};

/// Length in bytes of a symmetric encryption key (256 bits)
pub const KEY_LENGTH: usize = 32;

/// Length in bytes of the ChaCha20-Poly1305 nonce (96 bits)
pub const NONCE_LENGTH: usize = 12;

/// Minimum length in bytes required for an Argon2 salt
pub const MIN_SALT_LENGTH: usize = 8;

/// Suggested length in bytes for a freshly generated salt
pub const SALT_LENGTH: usize = 16;

/// Fixed salt used to derive a shared session key from a passphrase.
///
/// A fixed salt makes the derivation deterministic across processes, which is
/// what allows peers configured with the same passphrase to talk to each
/// other without an explicit key exchange. Because the salt is not secret,
/// callers should pick a long, high-entropy passphrase.
pub const PASSPHRASE_SALT: &[u8] = b"metaText-shared-salt";

/// Cryptographic manager for handling encryption and key management
///
/// The manager owns a symmetric key that is used to encrypt and decrypt
/// messages. When encryption is disabled in the configuration the manager
/// degrades gracefully to a pass-through implementation so that the rest of
/// the application does not need to special case the disabled state.
pub struct CryptoManager {
    /// Whether encryption is enabled
    enabled: bool,

    /// Current encryption algorithm
    algorithm: String,

    /// Symmetric key used for authenticated encryption
    key: Vec<u8>,
}

impl std::fmt::Debug for CryptoManager {
    /// Format the manager without leaking key material
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CryptoManager")
            .field("enabled", &self.enabled)
            .field("algorithm", &self.algorithm)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl CryptoManager {
    /// Create a new cryptographic manager
    ///
    /// # Arguments
    ///
    /// * `config` - Cryptographic configuration describing the desired algorithm
    ///
    /// # Returns
    ///
    /// Returns an initialized [`CryptoManager`] with a freshly generated key.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if the random number generator
    /// is unavailable.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::config::CryptoConfig;
    /// use meta_text::crypto::CryptoManager;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let config = CryptoConfig::default();
    /// let manager = CryptoManager::new(&config).await?;
    /// assert!(manager.is_enabled());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(config: &crate::config::CryptoConfig) -> MetaTextResult<Self> {
        let timestamp = chrono::Utc::now();
        info!(
            "🔐 [{}] Initializing cryptographic manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        let manager = Self {
            enabled: config.enable_encryption,
            algorithm: config.algorithm.clone(),
            key: Self::generate_key(),
        };

        if manager.enabled {
            info!(
                "✅ [{}] Encryption enabled with algorithm: {}",
                timestamp.format("%Y-%m-%d %H:%M:%S"),
                manager.algorithm
            );
        } else {
            warn!(
                "⚠️ [{}] Encryption disabled - messages will not be encrypted",
                timestamp.format("%Y-%m-%d %H:%M:%S")
            );
        }

        Ok(manager)
    }

    /// Test-only constructor for CryptoManager
    #[cfg(test)]
    pub(crate) fn test_new(enabled: bool, algorithm: String) -> Self {
        Self {
            enabled,
            algorithm,
            key: Self::generate_key(),
        }
    }

    /// Generate a cryptographically secure random symmetric key
    ///
    /// # Returns
    ///
    /// Returns a [`KEY_LENGTH`] byte key filled using the operating system
    /// random number generator.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::crypto::{CryptoManager, KEY_LENGTH};
    ///
    /// let key = CryptoManager::generate_key();
    /// assert_eq!(key.len(), KEY_LENGTH);
    /// ```
    pub fn generate_key() -> Vec<u8> {
        let mut key = vec![0_u8; KEY_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut key);
        key
    }

    /// Generate a cryptographically secure random salt
    ///
    /// # Returns
    ///
    /// Returns a [`SALT_LENGTH`] byte salt suitable for Argon2 key derivation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::crypto::{CryptoManager, SALT_LENGTH};
    ///
    /// let salt = CryptoManager::generate_salt();
    /// assert_eq!(salt.len(), SALT_LENGTH);
    /// ```
    pub fn generate_salt() -> Vec<u8> {
        let mut salt = vec![0_u8; SALT_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        salt
    }

    /// Derive a symmetric key from a password using Argon2
    ///
    /// # Arguments
    ///
    /// * `password` - Password or passphrase provided by the user
    /// * `salt` - Random salt with at least [`MIN_SALT_LENGTH`] bytes
    ///
    /// # Returns
    ///
    /// Returns a [`KEY_LENGTH`] byte key derived from the password.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if the salt is too short or the
    /// key derivation fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::crypto::{CryptoManager, KEY_LENGTH};
    ///
    /// let salt = b"0123456789abcdef";
    /// let key = CryptoManager::derive_key("correct horse battery staple", salt).unwrap();
    /// assert_eq!(key.len(), KEY_LENGTH);
    /// ```
    pub fn derive_key(password: &str, salt: &[u8]) -> MetaTextResult<Vec<u8>> {
        if salt.len() < MIN_SALT_LENGTH {
            return Err(MetaTextError::Cryptographic {
                message: format!("Salt must be at least {MIN_SALT_LENGTH} bytes long"),
                operation: "derive_key".to_string(),
                source: None,
            });
        }

        let mut key = vec![0_u8; KEY_LENGTH];
        // NOTE: `argon2::Error` does not implement `std::error::Error` unless
        // the `std` feature is enabled, so the error is captured in the message
        // instead of being boxed as a source.
        Argon2::default()
            .hash_password_into(password.as_bytes(), salt, &mut key)
            .map_err(|e| MetaTextError::Cryptographic {
                message: format!("Failed to derive key: {e}"),
                operation: "derive_key".to_string(),
                source: None,
            })?;

        Ok(key)
    }

    /// Derive a session key from a shared passphrase
    ///
    /// Every peer using the same passphrase derives the same key with the
    /// application-wide [`PASSPHRASE_SALT`], which is what makes peer to peer
    /// communication possible without an explicit key exchange.
    ///
    /// # Arguments
    ///
    /// * `config` - Cryptographic configuration describing the desired algorithm
    /// * `passphrase` - Shared secret known by all participants
    ///
    /// # Returns
    ///
    /// Returns a [`CryptoManager`] able to decrypt messages produced by any
    /// peer configured with the same passphrase.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if key derivation fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::config::CryptoConfig;
    /// use meta_text::crypto::CryptoManager;
    ///
    /// let config = CryptoConfig::default();
    /// let alice = CryptoManager::from_passphrase(&config, "shared secret").unwrap();
    /// let bob = CryptoManager::from_passphrase(&config, "shared secret").unwrap();
    ///
    /// let ciphertext = alice.encrypt(b"hello").unwrap();
    /// assert_eq!(bob.decrypt(&ciphertext).unwrap(), b"hello");
    /// ```
    pub fn from_passphrase(
        config: &crate::config::CryptoConfig,
        passphrase: &str,
    ) -> MetaTextResult<Self> {
        let key = Self::derive_key(passphrase, PASSPHRASE_SALT)?;
        Self::with_key(config, key)
    }

    /// Load a pre-existing key into a new manager
    ///
    /// # Arguments
    ///
    /// * `config` - Cryptographic configuration describing the desired algorithm
    /// * `key` - A [`KEY_LENGTH`] byte symmetric key
    ///
    /// # Returns
    ///
    /// Returns a [`CryptoManager`] that uses the provided key.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if the key has an invalid length.
    pub fn with_key(config: &crate::config::CryptoConfig, key: Vec<u8>) -> MetaTextResult<Self> {
        if key.len() != KEY_LENGTH {
            return Err(MetaTextError::Cryptographic {
                message: format!(
                    "Invalid key length: expected {KEY_LENGTH} bytes, got {}",
                    key.len()
                ),
                operation: "with_key".to_string(),
                source: None,
            });
        }

        Ok(Self {
            enabled: config.enable_encryption,
            algorithm: config.algorithm.clone(),
            key,
        })
    }

    /// Check if encryption is enabled
    ///
    /// # Returns
    ///
    /// Returns `true` when encryption is enabled, `false` otherwise.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get the current encryption algorithm
    ///
    /// # Returns
    ///
    /// Returns the configured algorithm name.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Get the raw symmetric key bytes
    ///
    /// # Returns
    ///
    /// Returns the symmetric key. Callers must treat the returned bytes as
    /// sensitive key material.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Get the symmetric key encoded as a hexadecimal string
    ///
    /// # Returns
    ///
    /// Returns the hexadecimal representation of the symmetric key.
    pub fn key_hex(&self) -> String {
        hex::encode(&self.key)
    }

    /// Encrypt a plaintext buffer
    ///
    /// The 12 byte nonce is generated randomly and prefixed to the returned
    /// ciphertext so that decryption is self contained.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - Bytes to encrypt
    ///
    /// # Returns
    ///
    /// Returns `nonce || ciphertext` when encryption is enabled, or a copy of
    /// the plaintext when encryption is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if encryption fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::config::CryptoConfig;
    /// use meta_text::crypto::CryptoManager;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let manager = CryptoManager::new(&CryptoConfig::default()).await?;
    /// let ciphertext = manager.encrypt(b"hello metaText")?;
    /// assert_ne!(&ciphertext[..], b"hello metaText");
    /// assert_eq!(manager.decrypt(&ciphertext)?, b"hello metaText");
    /// # Ok(())
    /// # }
    /// ```
    pub fn encrypt(&self, plaintext: &[u8]) -> MetaTextResult<Vec<u8>> {
        if !self.enabled {
            return Ok(plaintext.to_vec());
        }

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));

        // Generate a fresh random nonce for every message (never reuse a nonce)
        let mut nonce_bytes = [0_u8; NONCE_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext =
            cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| MetaTextError::Cryptographic {
                    message: format!("Encryption failed: {e}"),
                    operation: "encrypt".to_string(),
                    source: None,
                })?;

        let mut output = Vec::with_capacity(NONCE_LENGTH + ciphertext.len());
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    /// Decrypt a buffer produced by [`CryptoManager::encrypt`]
    ///
    /// # Arguments
    ///
    /// * `data` - `nonce || ciphertext` buffer
    ///
    /// # Returns
    ///
    /// Returns the recovered plaintext when encryption is enabled, or a copy of
    /// the input when encryption is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if the buffer is malformed or
    /// authentication fails.
    pub fn decrypt(&self, data: &[u8]) -> MetaTextResult<Vec<u8>> {
        if !self.enabled {
            return Ok(data.to_vec());
        }

        if data.len() <= NONCE_LENGTH {
            return Err(MetaTextError::Cryptographic {
                message: "Ciphertext is too short to contain a nonce".to_string(),
                operation: "decrypt".to_string(),
                source: None,
            });
        }

        let (nonce_bytes, ciphertext) = data.split_at(NONCE_LENGTH);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));

        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|e| MetaTextError::Cryptographic {
                message: format!("Decryption failed: {e}"),
                operation: "decrypt".to_string(),
                source: None,
            })
    }

    /// Encrypt a UTF-8 string
    ///
    /// # Arguments
    ///
    /// * `plaintext` - Text to encrypt
    ///
    /// # Returns
    ///
    /// Returns the encrypted byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if encryption fails.
    pub fn encrypt_string(&self, plaintext: &str) -> MetaTextResult<Vec<u8>> {
        self.encrypt(plaintext.as_bytes())
    }

    /// Decrypt a buffer into a UTF-8 string
    ///
    /// # Arguments
    ///
    /// * `data` - `nonce || ciphertext` buffer
    ///
    /// # Returns
    ///
    /// Returns the decrypted text.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if decryption fails or the
    /// plaintext is not valid UTF-8.
    pub fn decrypt_string(&self, data: &[u8]) -> MetaTextResult<String> {
        let plaintext = self.decrypt(data)?;
        String::from_utf8(plaintext).map_err(|e| MetaTextError::Cryptographic {
            message: format!("Decrypted payload is not valid UTF-8: {e}"),
            operation: "decrypt_string".to_string(),
            source: Some(Box::new(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CryptoConfig;

    /// Build an enabled crypto configuration for tests
    fn enabled_config() -> CryptoConfig {
        CryptoConfig {
            enable_encryption: true,
            algorithm: "ChaCha20-Poly1305".to_string(),
            kdf: "Argon2".to_string(),
            key_rotation_days: 30,
            enable_pfs: true,
        }
    }

    #[tokio::test]
    async fn test_crypto_manager_creation() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        assert!(manager.is_enabled());
        assert_eq!(manager.algorithm(), "ChaCha20-Poly1305");
        assert_eq!(manager.key().len(), KEY_LENGTH);
    }

    #[tokio::test]
    async fn test_crypto_encrypt_decrypt_roundtrip() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        let plaintext = "hello metaText 🚀";

        let ciphertext = manager.encrypt_string(plaintext).unwrap();
        assert_ne!(ciphertext, plaintext.as_bytes());
        assert_eq!(manager.decrypt_string(&ciphertext).unwrap(), plaintext);
    }

    #[tokio::test]
    async fn test_crypto_ciphertext_is_nonce_prefixed() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        let ciphertext = manager.encrypt(b"data").unwrap();
        // nonce (12 bytes) + tag (16 bytes) + 4 bytes of plaintext
        assert_eq!(ciphertext.len(), NONCE_LENGTH + 16 + 4);
    }

    #[tokio::test]
    async fn test_crypto_nonces_are_unique() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        let first = manager.encrypt(b"same plaintext").unwrap();
        let second = manager.encrypt(b"same plaintext").unwrap();
        assert_ne!(first, second, "nonces must differ between messages");
    }

    #[tokio::test]
    async fn test_crypto_decrypt_with_wrong_key_fails() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        let other = CryptoManager::new(&enabled_config()).await.unwrap();
        let ciphertext = manager.encrypt(b"top secret").unwrap();
        assert!(other.decrypt(&ciphertext).is_err());
    }

    #[tokio::test]
    async fn test_crypto_decrypt_rejects_truncated_input() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        assert!(manager.decrypt(&[0_u8; 4]).is_err());
    }

    #[tokio::test]
    async fn test_crypto_disabled_is_passthrough() {
        let mut config = enabled_config();
        config.enable_encryption = false;
        let manager = CryptoManager::new(&config).await.unwrap();

        assert!(!manager.is_enabled());
        let payload = b"plain text".to_vec();
        assert_eq!(manager.encrypt(&payload).unwrap(), payload);
        assert_eq!(manager.decrypt(&payload).unwrap(), payload);
    }

    #[test]
    fn test_generate_key_has_expected_length() {
        let key = CryptoManager::generate_key();
        assert_eq!(key.len(), KEY_LENGTH);
        assert_ne!(key, CryptoManager::generate_key());
    }

    #[test]
    fn test_generate_salt_has_expected_length() {
        let salt = CryptoManager::generate_salt();
        assert_eq!(salt.len(), SALT_LENGTH);
    }

    #[test]
    fn test_derive_key_is_deterministic() {
        let salt = b"0123456789abcdef";
        let first = CryptoManager::derive_key("passphrase", salt).unwrap();
        let second = CryptoManager::derive_key("passphrase", salt).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), KEY_LENGTH);
    }

    #[test]
    fn test_derive_key_differs_with_salt() {
        let first = CryptoManager::derive_key("passphrase", b"0123456789abcdef").unwrap();
        let second = CryptoManager::derive_key("passphrase", b"fedcba9876543210").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn test_derive_key_rejects_short_salt() {
        assert!(CryptoManager::derive_key("passphrase", b"short").is_err());
    }

    #[tokio::test]
    async fn test_with_key_rejects_invalid_length() {
        let config = enabled_config();
        assert!(CryptoManager::with_key(&config, vec![0_u8; 4]).is_err());
    }

    #[tokio::test]
    async fn test_with_key_roundtrip() {
        let config = enabled_config();
        let key = CryptoManager::generate_key();
        let first = CryptoManager::with_key(&config, key.clone()).unwrap();
        let second = CryptoManager::with_key(&config, key).unwrap();

        let ciphertext = first.encrypt(b"shared secret").unwrap();
        assert_eq!(second.decrypt(&ciphertext).unwrap(), b"shared secret");
    }

    #[tokio::test]
    async fn test_debug_does_not_leak_key() {
        let manager = CryptoManager::new(&enabled_config()).await.unwrap();
        let rendered = format!("{manager:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(&manager.key_hex()));
    }
}
