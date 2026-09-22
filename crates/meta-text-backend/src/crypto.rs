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
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
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

/// Domain separator for the per-contact key derivation.
///
/// A derived key is only meaningful together with the label it was derived under, so
/// the label is a version: the key agreement of [`crate::identity::pair_key`] takes
/// its own (`metaText/contact-key/v2`) instead of silently reinterpreting keys derived
/// under this one.
const CONTACT_KEY_LABEL: &[u8] = b"metaText/contact-key/v1";

/// Length of a derived contact key, in bytes.
///
/// Equal to [`KEY_LENGTH`], because the per-contact key encrypts with the same
/// AEAD as the session key; the two are kept apart by *how* they are derived, not
/// by their size.
pub const CONTACT_KEY_LENGTH: usize = 32;

/// Normalise an identity for the contact-key derivation.
///
/// DIDs are hexadecimal and different front-ends write them in different cases, so
/// the identity is trimmed and upper-cased before it is fed to the derivation. An
/// all-whitespace identity normalises to the empty string, which
/// [`CryptoManager::contact_key`] refuses.
#[must_use]
fn normalize_identity(identity: &str) -> String {
    identity.trim().to_ascii_uppercase()
}

/// The static per-pair key (label `v1`), from an explicit session key.
///
/// [`CryptoManager::contact_key`] is this function applied to the manager's session
/// key. It is also a free function so a caller that holds the session key can
/// reproduce the derivation without borrowing a manager — which is what the
/// transport's fallback path needs, and what makes that path testable against a key
/// an observer of the handshake could compute.
///
/// # Arguments
///
/// * `session_key` - The shared session key (the HKDF salt)
/// * `own_identity` - One end's announced identity
/// * `peer_identity` - The other end's announced identity
///
/// # Returns
///
/// Returns [`CONTACT_KEY_LENGTH`] bytes; the two ends get the same value whichever
/// order they pass the identities in.
///
/// # Errors
///
/// Returns [`MetaTextError::Cryptographic`] when either identity is empty or the
/// expansion fails (which cannot happen for 32 bytes under SHA-256).
pub fn contact_key(
    session_key: &[u8],
    own_identity: &str,
    peer_identity: &str,
) -> MetaTextResult<Vec<u8>> {
    let own = normalize_identity(own_identity);
    let peer = normalize_identity(peer_identity);
    if own.is_empty() || peer.is_empty() {
        return Err(MetaTextError::Cryptographic {
            message: "A contact key needs both identities".to_string(),
            operation: "contact_key".to_string(),
            source: None,
        });
    }

    // Ordering the pair is what makes the derivation symmetric: our own identity
    // and the peer's are not interchangeable, but the *key* must not depend on
    // who computes it.
    let (low, high) = if own <= peer {
        (own, peer)
    } else {
        (peer, own)
    };
    let info = format!("{low}:{high}");

    let hkdf = Hkdf::<Sha256>::new(Some(CONTACT_KEY_LABEL), session_key);
    let mut key = [0_u8; CONTACT_KEY_LENGTH];
    hkdf.expand(info.as_bytes(), &mut key)
        .map_err(|e| MetaTextError::Cryptographic {
            message: format!("Contact key derivation failed: {e}"),
            operation: "contact_key".to_string(),
            source: None,
        })?;
    Ok(key.to_vec())
}

/// Parse and length-check an explicit key.
///
/// Not a method: the check is about the argument, not about the manager, and a
/// wrongly sized key is a programming error rather than a property of the session.
fn checked_key(key: &[u8], operation: &str) -> MetaTextResult<ChaCha20Poly1305> {
    if key.len() != KEY_LENGTH {
        return Err(MetaTextError::Cryptographic {
            message: format!("Expected a {KEY_LENGTH} byte key, got {} bytes", key.len()),
            operation: operation.to_string(),
            source: None,
        });
    }
    Ok(ChaCha20Poly1305::new(Key::from_slice(key)))
}

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
    /// use meta_text_backend::config::CryptoConfig;
    /// use meta_text_backend::crypto::CryptoManager;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let config = CryptoConfig::default();
    /// let manager = CryptoManager::new(&config).await?;
    /// assert!(manager.is_enabled());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The constructor is `async` by contract even though nothing is awaited yet:
    /// it is one of the lifecycle steps (`new` → `start` → `shutdown`) `CoreService`
    /// drives in order, and a backend that has nothing to await must not make the
    /// actor, the transports and every test branch on which implementation they
    /// hold. The same reason already keeps the `Result` type.
    #[allow(clippy::unused_async)]
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

    /// Test-only constructor for `CryptoManager`
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
    /// use meta_text_backend::crypto::{CryptoManager, KEY_LENGTH};
    ///
    /// let key = CryptoManager::generate_key();
    /// assert_eq!(key.len(), KEY_LENGTH);
    /// ```
    #[must_use]
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
    /// use meta_text_backend::crypto::{CryptoManager, SALT_LENGTH};
    ///
    /// let salt = CryptoManager::generate_salt();
    /// assert_eq!(salt.len(), SALT_LENGTH);
    /// ```
    #[must_use]
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
    /// use meta_text_backend::crypto::{CryptoManager, KEY_LENGTH};
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
    /// use meta_text_backend::config::CryptoConfig;
    /// use meta_text_backend::crypto::CryptoManager;
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
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get the current encryption algorithm
    ///
    /// # Returns
    ///
    /// Returns the configured algorithm name.
    #[must_use]
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Get the raw symmetric key bytes
    ///
    /// # Returns
    ///
    /// Returns the symmetric key. Callers must treat the returned bytes as
    /// sensitive key material.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Get the symmetric key encoded as a hexadecimal string
    ///
    /// # Returns
    ///
    /// Returns the hexadecimal representation of the symmetric key.
    #[must_use]
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
    /// use meta_text_backend::config::CryptoConfig;
    /// use meta_text_backend::crypto::CryptoManager;
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
        self.encrypt_with(&self.key, plaintext)
    }

    /// Encrypt with an explicit key instead of the session key.
    ///
    /// The frames of one contact pair use a key of their own (see
    /// [`CryptoManager::contact_key`]), so "which key" has to be a parameter rather
    /// than a property of the manager. The envelope is unchanged: `nonce ||
    /// ciphertext+tag`, with a fresh random nonce per call.
    ///
    /// # Arguments
    ///
    /// * `key` - Key to encrypt under; must be [`KEY_LENGTH`] bytes.
    /// * `plaintext` - Bytes to encrypt.
    ///
    /// # Returns
    ///
    /// Returns the encrypted buffer, or a copy of the input when encryption is
    /// disabled.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] when the key has the wrong length or
    /// encryption fails.
    pub fn encrypt_with(&self, key: &[u8], plaintext: &[u8]) -> MetaTextResult<Vec<u8>> {
        if !self.enabled {
            return Ok(plaintext.to_vec());
        }
        let key = checked_key(key, "encrypt")?;

        // Generate a fresh random nonce for every message (never reuse a nonce)
        let mut nonce_bytes = [0_u8; NONCE_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext =
            key.encrypt(nonce, plaintext)
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
        self.decrypt_with(&self.key, data)
    }

    /// Decrypt with an explicit key instead of the session key.
    ///
    /// The counterpart of [`CryptoManager::encrypt_with`].
    ///
    /// # Arguments
    ///
    /// * `key` - Key the buffer was encrypted under; must be [`KEY_LENGTH`] bytes.
    /// * `data` - `nonce || ciphertext` buffer.
    ///
    /// # Returns
    ///
    /// Returns the recovered plaintext, or a copy of the input when encryption is
    /// disabled.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if the buffer is malformed,
    /// authentication fails, or the key has the wrong length.
    pub fn decrypt_with(&self, key: &[u8], data: &[u8]) -> MetaTextResult<Vec<u8>> {
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

        let key = checked_key(key, "decrypt")?;
        let (nonce_bytes, ciphertext) = data.split_at(NONCE_LENGTH);

        key.decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|e| MetaTextError::Cryptographic {
                message: format!("Decryption failed: {e}"),
                operation: "decrypt".to_string(),
                source: None,
            })
    }

    /// Key material shared by exactly one pair of session identities.
    ///
    /// The session key is a *group* secret: every peer that knows the passphrase can
    /// read every frame. A contact pair therefore gets a key of its own, derived
    /// with HKDF (RFC 5869) from the session key, domain-separated by the
    /// `CONTACT_KEY_LABEL` context string and bound to the pair through the HKDF
    /// `info` field.
    ///
    /// The two identities are ordered before use, so both ends derive the same key
    /// no matter which side asks, and compared case-insensitively because a DID is
    /// written in either case by different front-ends.
    ///
    /// # What this does and does not buy
    ///
    /// It separates the pairs: a frame is bound to the two identities it was written
    /// for, so a frame captured from one pair cannot be decrypted — or replayed — as
    /// part of another, and the session key alone is no longer enough for a directed
    /// message. It is **not** per-contact confidentiality against somebody who holds
    /// the passphrase *and* has seen both identities: they can derive the same key,
    /// because the derivation input is a shared secret, not a key agreement.
    ///
    /// That gap is what [`crate::identity::pair_key`] closes: a directed message
    /// between two peers that both run the key agreement is sealed under that key
    /// instead, and this derivation is then only the fallback for a peer that
    /// announced no per-connection key (see `crate::network::NetworkManager::contact_key`,
    /// which tries the agreed key first). Nothing here changes for that: the two are
    /// separated by their label, so a v1 key can never be mistaken for a v2 one.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] when either identity is empty (an
    /// empty identity would silently produce a key shared with everybody).
    pub fn contact_key(&self, own_identity: &str, peer_identity: &str) -> MetaTextResult<Vec<u8>> {
        contact_key(&self.key, own_identity, peer_identity)
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
            // The only value `AppConfig::problems` accepts: no rotation is implemented.
            key_rotation_days: 0,
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

    /// A contact key is symmetric, pair-specific and never the session key.
    #[tokio::test]
    async fn test_contact_key_is_a_key_per_pair() {
        let manager = CryptoManager::from_passphrase(&CryptoConfig::default(), "session-secret")
            .expect("derive key");
        let alice = "A".repeat(64);
        let bob = "B".repeat(64);
        let carol = "C".repeat(64);

        let from_alice = manager.contact_key(&alice, &bob).expect("derive");
        // Both ends compute the same key, in either argument order and in either
        // case: that is the whole point of ordering the pair.
        assert_eq!(
            from_alice,
            manager.contact_key(&bob, &alice).expect("derive")
        );
        assert_eq!(
            from_alice,
            manager
                .contact_key(&alice.to_lowercase(), &bob.to_lowercase())
                .expect("derive")
        );
        // Derivation is deterministic: asking twice gives the same key, so no cache
        // is needed (and none can go stale).
        assert_eq!(
            from_alice,
            manager.contact_key(&alice, &bob).expect("derive")
        );

        // A different pair gets a different key...
        let other_pair = manager.contact_key(&alice, &carol).expect("derive");
        assert_ne!(from_alice, other_pair);
        // ...and neither of them is the session key.
        assert_ne!(from_alice, manager.key());
        assert_eq!(from_alice.len(), CONTACT_KEY_LENGTH);
        assert_eq!(from_alice.len(), KEY_LENGTH);

        // Two managers with the same passphrase agree (they share the session key),
        // and one with a different passphrase does not.
        let same = CryptoManager::from_passphrase(&CryptoConfig::default(), "session-secret")
            .expect("derive key");
        assert_eq!(same.contact_key(&alice, &bob).expect("derive"), from_alice);
        let other = CryptoManager::from_passphrase(&CryptoConfig::default(), "another-secret")
            .expect("derive key");
        assert_ne!(other.contact_key(&alice, &bob).expect("derive"), from_alice);
    }

    /// A contact key needs both identities, and an empty one is refused.
    #[tokio::test]
    async fn test_contact_key_refuses_empty_identities() {
        let manager = CryptoManager::new(&CryptoConfig::default())
            .await
            .expect("manager");
        let did = "A".repeat(64);

        for (own, peer) in [
            ("", did.as_str()),
            (did.as_str(), ""),
            ("   ", did.as_str()),
        ] {
            let error = manager
                .contact_key(own, peer)
                .expect_err("an empty identity must be refused");
            assert!(error.to_string().contains("both identities"), "{error}");
        }
    }

    /// A pair key round-trips, and only under that key.
    #[tokio::test]
    async fn test_encryption_under_a_contact_key() {
        let manager = CryptoManager::new(&CryptoConfig::default())
            .await
            .expect("manager");
        let alice = "A".repeat(64);
        let bob = "B".repeat(64);
        let carol = "C".repeat(64);
        let pair = manager.contact_key(&alice, &bob).expect("derive");
        let other = manager.contact_key(&alice, &carol).expect("derive");

        // Bob derives the same key and reads the message.
        let ciphertext = manager.encrypt_with(&pair, b"hello").expect("encrypt");
        assert_eq!(
            manager
                .decrypt_with(
                    &manager.contact_key(&bob, &alice).expect("derive"),
                    &ciphertext
                )
                .expect("decrypt"),
            b"hello"
        );

        // Carol's pair key and the session key do not open it.
        assert!(manager.decrypt_with(&other, &ciphertext).is_err());
        assert!(manager.decrypt(&ciphertext).is_err());

        // A tampered frame is rejected by the AEAD rather than decrypted wrongly.
        let mut tampered = ciphertext.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(manager.decrypt_with(&pair, &tampered).is_err());

        // A key of the wrong size is a programming error, not a silent no-op.
        assert!(manager.encrypt_with(&pair[..16], b"hello").is_err());
        assert!(manager.decrypt_with(&[], &ciphertext).is_err());
    }

    /// With encryption disabled every explicit key is a pass-through, like the
    /// session-key path.
    #[tokio::test]
    async fn test_explicit_keys_degrade_when_encryption_is_disabled() {
        let config = CryptoConfig {
            enable_encryption: false,
            ..CryptoConfig::default()
        };
        let manager = CryptoManager::new(&config).await.expect("manager");

        let key = manager.contact_key("A", "B").expect("derive");
        let data = manager.encrypt_with(&key, b"plain").expect("encrypt");
        assert_eq!(data, b"plain");
        assert_eq!(
            manager.decrypt_with(&key, &data).expect("decrypt"),
            b"plain"
        );
    }
}
