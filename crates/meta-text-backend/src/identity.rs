/*!
 * identity.rs
 *
 * The network identity: the key material one instance announces to its peers,
 * and the file that makes it outlive a restart.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-14
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`NetworkIdentity`] is an X25519 key pair: the public half is what a peer
 *   derives the pair's key from, the secret half is what a key agreement will use
 *   when A12's confidentiality half is closed
 * - [`IdentityStore`] keeps it in `<data-dir>/net-identity.json`, read at startup
 *   and written by atomic replace, so the value a peer pinned last run is the
 *   value it sees this run
 *
 * # Why the identity has to persist
 *
 * A session identity used to be `hex(random)` per process: two runs of the same
 * user were two different identities, and nothing a peer learned could be
 * remembered. That is what made the per-contact key half of A12 unverifiable — a
 * key derived from a value that changes every run can separate two pairs but
 * cannot be *checked* by either of them, so it could never be pinned, which in
 * turn is why the TCP outbox cannot be keyed by a peer's identity (see
 * `docs/ARCHITECTURE.md` §6.17).
 *
 * The identity is now an X25519 key pair generated once per data directory:
 *
 * - the **public half is announced** (`FRAME_IDENTITY`), exactly as the DID was,
 *   and has the same shape (64 hexadecimal characters), so no frame changed size
 *   and a peer needs no new code to receive it;
 * - the **secret half never leaves the process** and is not announced, logged or
 *   part of any snapshot; it is what a future key agreement uses;
 * - the **fingerprint** is a short form of the public half that a person can read
 *   out and compare (see [`NetworkIdentity::fingerprint`]).
 *
 * # What persisting the identity does *not* buy
 *
 * It does not authenticate the peer. The public half still travels in the clear,
 * so a passphrase holder who has seen both identities can derive the same contact
 * key the pair derives (A12, confidentiality half). What it buys is that the value
 * is now *stable and comparable*: a peer can pin it, a user can compare
 * fingerprints, and the contact key a pair derives is the same one across
 * restarts — the three things the open half of A12 needs to build on.
 *
 * # Failure policy
 *
 * Reading never fails: a missing file is the first run and is created; a corrupt
 * or unknown-version file is reported at `warn` and replaced by a fresh identity
 * rather than stopping the client. Losing the identity costs the pin a peer holds
 * (it sees a new key and reports a change), not the ability to run. Writing is
 * best effort for the same reason: the identity is still usable in memory for the
 * session, only its durability is lost.
 */

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

use crate::crypto::KEY_LENGTH;
use crate::error::{MetaTextError, MetaTextResult};

/// File name the network identity is persisted to.
///
/// It sits next to the session snapshot (`metatext-session.json`) and, in a Tox
/// build, next to `tox-savedata.bin`: the data directory names everything that has
/// to outlive a run.
pub const IDENTITY_FILE: &str = "net-identity.json";

/// Layout version of the file.
///
/// A reader only accepts the version it knows, so a future layout cannot be
/// misread as this one; a mismatch is reported and treated as "no identity".
pub const IDENTITY_VERSION: u32 = 1;

/// Length of the secret half, in bytes.
///
/// Equal to [`KEY_LENGTH`] because both are 256-bit values; the identity's is an
/// X25519 scalar rather than a symmetric key, which is why it is spelled out here
/// instead of reusing the other constant's name.
pub const SECRET_LENGTH: usize = KEY_LENGTH;

/// Length of the public half, in bytes (RFC 7748 uses 32-byte u-coordinates).
pub const PUBLIC_KEY_LENGTH: usize = 32;

/// Bytes of `SHA-256(public key)` shown as the fingerprint.
///
/// 128 bits: short enough to read out loud in eight groups, long enough that a
/// glance at two fingerprints means something. It is a *comparison aid* — the
/// public key itself is what gets announced and pinned, so a fingerprint that
/// collided would not authenticate anything by itself.
pub const FINGERPRINT_BYTES: usize = 16;

/// Number of hexadecimal characters per fingerprint group.
const FINGERPRINT_GROUP: usize = 4;

/// The identity one instance announces to its peers.
///
/// Cloneable and comparable, but its [`std::fmt::Debug`] view is redacted: the
/// secret half must never reach a log record (the same rule the core's own debug
/// view follows for the session key).
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkIdentity {
    /// The secret half (an X25519 scalar), never announced.
    secret: [u8; SECRET_LENGTH],

    /// The public half (an X25519 u-coordinate), announced to every peer.
    public: [u8; PUBLIC_KEY_LENGTH],
}

impl std::fmt::Debug for NetworkIdentity {
    /// Redacted view: the public half and its fingerprint, never the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkIdentity")
            .field("public_hex", &self.public_hex())
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl NetworkIdentity {
    /// Generate a fresh identity from the operating system random number
    /// generator.
    ///
    /// # Returns
    ///
    /// Returns a new identity; the secret half is drawn from `OsRng` and never
    /// derived from anything else.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::identity::NetworkIdentity;
    ///
    /// let identity = NetworkIdentity::generate();
    /// assert_eq!(identity.public_hex().len(), 64);
    /// ```
    #[must_use]
    pub fn generate() -> Self {
        let mut secret = [0_u8; SECRET_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        Self::from_secret(secret)
    }

    /// The identity belonging to an X25519 secret.
    ///
    /// # Arguments
    ///
    /// * `secret` - The 32 byte secret half, as read back from the store
    ///
    /// # Returns
    ///
    /// Returns the identity, with the public half derived from `secret`. Any 32
    /// bytes are a valid scalar (RFC 7748 clamps them on use), so this cannot fail.
    #[must_use]
    pub fn from_secret(secret: [u8; SECRET_LENGTH]) -> Self {
        let public = x25519(secret, X25519_BASEPOINT_BYTES);
        Self { secret, public }
    }

    /// The identity belonging to a hexadecimal secret.
    ///
    /// # Arguments
    ///
    /// * `secret` - 64 hexadecimal characters (either case)
    ///
    /// # Returns
    ///
    /// Returns `Some` when `secret` is exactly [`SECRET_LENGTH`] bytes of valid
    /// hexadecimal, and `None` otherwise — a truncated or non-hex value is a
    /// damaged file, not an identity to guess at.
    #[must_use]
    pub fn from_secret_hex(secret: &str) -> Option<Self> {
        let bytes = hex::decode(secret.trim()).ok()?;
        let secret: [u8; SECRET_LENGTH] = bytes.try_into().ok()?;
        Some(Self::from_secret(secret))
    }

    /// The secret half, lowercase hexadecimal.
    ///
    /// # Returns
    ///
    /// Returns the value the store writes. It is key material: a caller that only
    /// needs to *name* this instance wants [`Self::public_hex`] or
    /// [`Self::fingerprint`].
    #[must_use]
    pub fn secret_hex(&self) -> String {
        hex::encode(self.secret)
    }

    /// The announced value, uppercase hexadecimal.
    ///
    /// # Returns
    ///
    /// Returns 64 uppercase hexadecimal characters — the shape the DID had, so the
    /// identity frame and the contact-key derivation are unchanged.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::identity::NetworkIdentity;
    ///
    /// let identity = NetworkIdentity::generate();
    /// assert_eq!(identity.public_hex(), identity.public_hex().to_uppercase());
    /// ```
    #[must_use]
    pub fn public_hex(&self) -> String {
        hex::encode_upper(self.public)
    }

    /// The public half, as the bytes a key agreement takes.
    ///
    /// # Returns
    ///
    /// Returns the 32 byte X25519 u-coordinate that is announced.
    #[must_use]
    pub const fn public_key(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.public
    }

    /// Parse an announced public key.
    ///
    /// # Arguments
    ///
    /// * `public` - 64 hexadecimal characters (either case)
    ///
    /// # Returns
    ///
    /// Returns `Some` for a well-formed key and `None` otherwise. A peer's
    /// malformed announcement is refused rather than replaced by a guess.
    ///
    /// This is the free [`public_key_from_hex`] under a name that reads well on the
    /// type; the ephemeral half of a handshake is parsed with the same function.
    #[must_use]
    pub fn public_key_from_hex(public: &str) -> Option<[u8; PUBLIC_KEY_LENGTH]> {
        public_key_from_hex(public)
    }

    /// The shared secret with a peer's public half.
    ///
    /// # Arguments
    ///
    /// * `peer_public` - The peer's announced X25519 public key
    ///
    /// # Returns
    ///
    /// Returns the 32 byte Diffie-Hellman result; both ends compute the same value,
    /// which is what the key agreement needs. [`pair_key`] is its only caller: it mixes
    /// this static-static result with the two ephemeral ones and derives the key a
    /// directed message is sealed under (`docs/ARCHITECTURE.md` §3.11).
    #[must_use]
    pub fn agree(&self, peer_public: [u8; PUBLIC_KEY_LENGTH]) -> [u8; SECRET_LENGTH] {
        x25519(self.secret, peer_public)
    }

    /// A short, readable form of the public half.
    ///
    /// # Returns
    ///
    /// Returns the fingerprint of the announced key (see [`fingerprint`]) in
    /// groups of four, for example `A1B2-C3D4-E5F6-0718-293A-4B5C-6D7E-8F90`. Two
    /// users compare it out of band; the announced key stays the authority, so this
    /// is a reading aid rather than a second identity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::identity::NetworkIdentity;
    ///
    /// let identity = NetworkIdentity::generate();
    /// assert_eq!(identity.fingerprint().matches('-').count(), 7);
    /// ```
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

/// Domain separator for the *agreed* pair key (v2).
///
/// A derived key is only meaningful together with the label it was derived under, so
/// this is a version: [`crate::crypto`]'s v1 label belongs to the static derivation
/// (HKDF over the session key and the two announced identities), which an observer of
/// the handshake can reproduce. A v2 key needs a secret, so it cannot be derived from
/// what is public — see [`pair_key`].
pub const PAIR_KEY_LABEL: &[u8] = b"metaText/contact-key/v2";

/// Parse a 32 byte X25519 public key from 64 hexadecimal characters.
///
/// # Arguments
///
/// * `public` - The announced value (either case, surrounding whitespace ignored)
///
/// # Returns
///
/// Returns `Some` for a well-formed key and `None` otherwise. A peer's malformed
/// announcement is refused rather than replaced by a guess.
///
/// # Examples
///
/// ```rust
/// use meta_text_backend::identity::{public_key_from_hex, NetworkIdentity};
///
/// let identity = NetworkIdentity::generate();
/// assert!(public_key_from_hex(&identity.public_hex()).is_some());
/// assert!(public_key_from_hex("nope").is_none());
/// ```
#[must_use]
pub fn public_key_from_hex(public: &str) -> Option<[u8; PUBLIC_KEY_LENGTH]> {
    let bytes = hex::decode(public.trim()).ok()?;
    bytes.try_into().ok()
}

/// The fingerprint of an announced public half.
///
/// # Arguments
///
/// * `public_key` - The 32 byte X25519 public key somebody announced
///
/// # Returns
///
/// Returns the first [`FINGERPRINT_BYTES`] bytes of `SHA-256(public key)` as
/// uppercase hexadecimal in groups of four characters. The grouping is what makes
/// it readable out loud; the bytes underneath are what a caller compares.
///
/// # Examples
///
/// ```rust
/// use meta_text_backend::identity::{fingerprint, NetworkIdentity};
///
/// let identity = NetworkIdentity::generate();
/// assert_eq!(fingerprint(&identity.public_key()), identity.fingerprint());
/// ```
#[must_use]
pub fn fingerprint(public_key: &[u8; PUBLIC_KEY_LENGTH]) -> String {
    let digest = Sha256::digest(public_key);
    let shown = hex::encode_upper(&digest[..FINGERPRINT_BYTES]);

    shown
        .as_bytes()
        .chunks(FINGERPRINT_GROUP)
        .map(|group| std::str::from_utf8(group).unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// A per-connection X25519 key pair.
///
/// The *static* identity is what a peer pins and what is announced; this pair exists
/// only for the life of one connection, and its secret half is never sent anywhere.
/// It is what makes a pair key a **key agreement** rather than a derivation: the
/// agreed key depends on a value only the two ends hold, so knowing the session key
/// and both announced identities (which is all an observer of the handshake has) is
/// no longer enough to compute it.
///
/// Not `Clone`: one connection, one key. A reconnect is a new pair, which is the
/// point.
pub struct EphemeralKey {
    /// The secret half, never announced.
    secret: [u8; SECRET_LENGTH],

    /// The public half, announced on the connection's handshake.
    public: [u8; PUBLIC_KEY_LENGTH],
}

impl std::fmt::Debug for EphemeralKey {
    /// Redacted view: the public half, never the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EphemeralKey")
            .field("public_hex", &self.public_hex())
            .finish_non_exhaustive()
    }
}

impl EphemeralKey {
    /// Generate a fresh per-connection key pair.
    ///
    /// # Returns
    ///
    /// Returns a new pair; the secret is drawn from `OsRng` and never reused.
    #[must_use]
    pub fn generate() -> Self {
        let mut secret = [0_u8; SECRET_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        Self::from_secret(secret)
    }

    /// The pair belonging to a given secret (used by tests to pin a known value).
    ///
    /// # Arguments
    ///
    /// * `secret` - The 32 byte secret half
    ///
    /// # Returns
    ///
    /// Returns the pair, with the public half derived from `secret`.
    #[must_use]
    pub fn from_secret(secret: [u8; SECRET_LENGTH]) -> Self {
        let public = x25519(secret, X25519_BASEPOINT_BYTES);
        Self { secret, public }
    }

    /// The announced value of this connection, uppercase hexadecimal.
    #[must_use]
    pub fn public_hex(&self) -> String {
        hex::encode_upper(self.public)
    }

    /// The public half, as the bytes a key agreement takes.
    #[must_use]
    pub const fn public_key(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.public
    }
}

/// The key two ends agree on for one connection.
///
/// RFC 7748 (X25519) in a three-DH construction, in the style of a Noise `XX`
/// handshake without signatures:
///
/// ```text
///   v1 = DH(S_own, P_peer)      static · static
///   v2 = DH(E_a,   P_b)         ephemeral · static, a = the end with the lower key
///   v3 = DH(S_a,   E_b)         static · ephemeral
///   key = HKDF-SHA256(session key, label, v1 ‖ v2 ‖ v3)
/// ```
///
/// `DH(x, Y) == DH(y, X)`, so both ends compute the same three values; the *order*
/// they are concatenated in is pinned by the pair of static keys (the end whose key
/// sorts lower contributes its ephemeral first), which is what makes the two sides
/// agree without a round trip beyond the two announcements they already make.
///
/// # What each part is for
///
/// * **v1** is what makes the key unreachable for an observer: it needs a static
///   *secret*, so knowing the session key and both announced public keys — which is
///   exactly what a passive listener to the handshake has — is not enough. That is the
///   difference from the v1 *label* in [`crate::crypto`], whose key such an observer
///   can derive.
/// * **v2 and v3** mix in a value that is new for every connection, so a recorded
///   handshake does not yield the key of a later one, and the key is bound to these
///   two ends.
/// * The **session key** is the HKDF salt, so a pair key stays out of reach for a peer
///   that does not hold the passphrase: session membership remains a precondition
///   instead of being bypassed by the announcement layer.
///
/// # Parameters
///
/// * `session_key` - The session key both ends derived from the passphrase
/// * `own_static` - This end's persisted identity (its secret half is used)
/// * `own_ephemeral` - This connection's ephemeral pair
/// * `peer_static` - The public half the peer announced
/// * `peer_ephemeral` - The public half the peer announced for this connection
///
/// # Returns
///
/// Returns a [`KEY_LENGTH`] byte key. Both ends of the connection compute the same
/// value; which end calls it changes nothing.
///
/// # Errors
///
/// Returns [`MetaTextError::Cryptographic`] when the expansion fails, which cannot
/// happen for a 32 byte output under SHA-256 (whose cap is 255 × 32 bytes). The
/// `Result` exists because that is the shape of the HKDF API, not because this can
/// fail in practice.
pub fn pair_key(
    session_key: &[u8],
    own_static: &NetworkIdentity,
    own_ephemeral: &EphemeralKey,
    peer_static: [u8; PUBLIC_KEY_LENGTH],
    peer_ephemeral: [u8; PUBLIC_KEY_LENGTH],
) -> MetaTextResult<[u8; KEY_LENGTH]> {
    let local_static = own_static.public_key();

    let static_static = own_static.agree(peer_static);
    // The end with the lower static key contributes its *ephemeral* to v2 and the
    // other contributes its static key; both compute the same value, which is why the
    // pair does not need a third message to agree on an order.
    let (ephemeral_static, static_ephemeral) = if local_static <= peer_static {
        (
            x25519(own_ephemeral.secret, peer_static),
            own_static.agree(peer_ephemeral),
        )
    } else {
        (
            own_static.agree(peer_ephemeral),
            x25519(own_ephemeral.secret, peer_static),
        )
    };

    let mut info = [0_u8; 3 * SECRET_LENGTH];
    info[..SECRET_LENGTH].copy_from_slice(&static_static);
    info[SECRET_LENGTH..2 * SECRET_LENGTH].copy_from_slice(&ephemeral_static);
    info[2 * SECRET_LENGTH..].copy_from_slice(&static_ephemeral);

    let hkdf = Hkdf::<Sha256>::new(Some(PAIR_KEY_LABEL), session_key);
    let mut key = [0_u8; KEY_LENGTH];
    hkdf.expand(&info, &mut key)
        .map_err(|error| MetaTextError::Cryptographic {
            message: format!("Pair key derivation failed: {error}"),
            operation: "pair_key".to_string(),
            source: None,
        })?;
    Ok(key)
}

/// The stored layout: a version and the secret half.
///
/// The public half is not stored because it is a function of the secret; writing
/// both would create two values that could disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredIdentity {
    /// Layout version; [`IDENTITY_VERSION`] for anything this build writes.
    version: u32,

    /// The secret half, lowercase hexadecimal.
    ///
    /// A local file is the only thing that has to read it, and hexadecimal keeps
    /// the file text — but it *is* the key, so the file is created with owner-only
    /// permissions where the platform has them.
    secret: String,
}

/// The file the network identity is kept in.
///
/// Cheap to clone (a path and a shared write lock); one instance per process is
/// enough, and the lock is what keeps two writers from interleaving.
#[derive(Debug, Clone)]
pub struct IdentityStore {
    /// Where the identity is written.
    path: PathBuf,

    /// Serialises writers; see [`IdentityStore::store`].
    writing: Arc<StdMutex<()>>,
}

impl IdentityStore {
    /// The store for a data directory.
    ///
    /// # Arguments
    ///
    /// * `data_dir` - Directory the session snapshot lives in
    ///
    /// # Returns
    ///
    /// Returns the store; nothing is read or created until it is used.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(IDENTITY_FILE),
            writing: Arc::new(StdMutex::new(())),
        }
    }

    /// Where the identity is written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the persisted identity, if there is a usable one.
    ///
    /// # Returns
    ///
    /// Returns `Some` when the file exists and holds a layout this build knows,
    /// and `None` when it does not (the first run) or cannot be trusted (a corrupt
    /// or newer-version file). Each of the latter is logged, so an identity that
    /// was replaced — which a peer sees as a changed key — is visible in the log
    /// rather than only in a peer's warning.
    #[must_use]
    pub fn load(&self) -> Option<NetworkIdentity> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    "🧪 No persisted network identity at {}",
                    self.path.display()
                );
                return None;
            }
            Err(error) => {
                warn!("⚠️ Could not read {}: {error}", self.path.display());
                return None;
            }
        };

        match serde_json::from_slice::<StoredIdentity>(&bytes) {
            Ok(stored) if stored.version == IDENTITY_VERSION => {
                NetworkIdentity::from_secret_hex(&stored.secret).or_else(|| {
                    warn!(
                        "⚠️ {} holds a malformed identity secret; generating a new identity",
                        self.path.display()
                    );
                    None
                })
            }
            Ok(stored) => {
                warn!(
                    "⚠️ {} holds network identity layout version {} and this build writes \
                     {IDENTITY_VERSION}; generating a new identity rather than guessing",
                    self.path.display(),
                    stored.version
                );
                None
            }
            Err(error) => {
                warn!(
                    "⚠️ {} is not a readable network identity ({error}); generating a new one",
                    self.path.display()
                );
                None
            }
        }
    }
}

impl IdentityStore {
    /// Read the persisted identity, generating and storing one on first use.
    ///
    /// # Returns
    ///
    /// Returns the identity for this data directory. A generated identity is
    /// written immediately, so the next run announces the same value; if that write
    /// fails the identity is still returned, because an unusable file must not stop
    /// the client — the cost is that a peer pins a value that changes next run.
    #[must_use]
    pub fn load_or_create(&self) -> NetworkIdentity {
        if let Some(identity) = self.load() {
            debug!(
                "🔑 Using the persisted network identity {} (fingerprint {}) from {}",
                crate::utils::abbreviate(&identity.public_hex()),
                identity.fingerprint(),
                self.path.display()
            );
            return identity;
        }

        let identity = NetworkIdentity::generate();
        if let Err(error) = self.store(&identity) {
            warn!(
                "⚠️ Could not persist a new network identity to {}: {error}",
                self.path.display()
            );
        } else {
            debug!(
                "🆕 Generated a network identity {} (fingerprint {}) at {}",
                crate::utils::abbreviate(&identity.public_hex()),
                identity.fingerprint(),
                self.path.display()
            );
        }
        identity
    }

    /// Publish an identity.
    ///
    /// The write is a whole-value atomic replace: a temporary file in the same
    /// directory, then a rename, so a reader sees either the previous identity or
    /// this one and never a half-written secret. On Unix the temporary file is
    /// created owner-readable only, because nothing else has any business reading a
    /// private key.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity to write
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` once the rename succeeded.
    ///
    /// # Errors
    ///
    /// Returns the i/o error when the directory cannot be created, the temporary
    /// file cannot be written or the rename fails.
    pub fn store(&self, identity: &NetworkIdentity) -> std::io::Result<()> {
        // Nothing else takes this lock, so holding it across the snapshot and the
        // write cannot deadlock against the transport.
        let _guard = lock(&self.writing);
        let state = StoredIdentity {
            version: IDENTITY_VERSION,
            secret: identity.secret_hex(),
        };

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let bytes = serde_json::to_vec(&state)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("tmp");
        write_private(&temporary, &bytes)?;
        std::fs::rename(&temporary, &self.path)?;

        debug!(
            "💾 Persisted the network identity {} to {}",
            identity.fingerprint(),
            self.path.display()
        );
        Ok(())
    }
}

/// Write `bytes` to `path`, owner-readable only where the platform supports it.
///
/// A plain `std::fs::write` creates the file with the process umask, which on a
/// shared machine can mean world-readable. The temporary file is therefore created
/// with an explicit `0o600` on Unix and renamed into place as usual; on other
/// platforms there is no mode to set, so the write is a plain one.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// Write `bytes` to `path` (see the Unix twin for the permission policy).
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Take a mutex, treating a poisoned one as usable.
///
/// A panic while the lock is held leaves a writable file, and refusing to touch it
/// afterwards would turn one panic into a permanently unpersisted identity.
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generated identity is a usable key pair with the announced shape.
    #[test]
    fn test_a_generated_identity_is_a_key_pair() {
        let identity = NetworkIdentity::generate();

        let public = identity.public_hex();
        assert_eq!(public.len(), PUBLIC_KEY_LENGTH * 2);
        assert_eq!(public, public.to_uppercase(), "the DID shape is uppercase");
        assert!(
            public
                .chars()
                .all(|character| character.is_ascii_hexdigit()),
            "the announced value must be hexadecimal"
        );
        assert_ne!(
            public,
            identity.secret_hex().to_uppercase(),
            "the announced value must not be the secret"
        );
    }

    /// Two identities are different, and the fingerprint names one of them.
    #[test]
    fn test_two_identities_differ_and_each_has_a_name() {
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();

        assert_ne!(alice.public_hex(), bob.public_hex());
        assert_ne!(alice.secret_hex(), bob.secret_hex());
        assert_ne!(alice.fingerprint(), bob.fingerprint());

        // 16 bytes in groups of four, joined by seven separators.
        assert_eq!(alice.fingerprint().len(), FINGERPRINT_BYTES * 2 + 7);
        assert_eq!(alice.fingerprint().matches('-').count(), 7);
        assert_eq!(
            alice.fingerprint(),
            alice.fingerprint(),
            "the fingerprint must be a function of the key, not of the moment"
        );
    }

    /// The secret is what the file holds, and the public half is derived from it.
    #[test]
    fn test_the_secret_round_trips_through_hexadecimal() {
        let identity = NetworkIdentity::generate();
        let restored =
            NetworkIdentity::from_secret_hex(&identity.secret_hex()).expect("a valid secret");

        assert_eq!(restored.public_hex(), identity.public_hex());
        assert_eq!(restored.fingerprint(), identity.fingerprint());

        // Either case is accepted, like every other hexadecimal value on the wire.
        let upper = NetworkIdentity::from_secret_hex(&identity.secret_hex().to_uppercase())
            .expect("uppercase hexadecimal");
        assert_eq!(upper.public_hex(), identity.public_hex());
    }

    /// A truncated or non-hexadecimal secret is refused rather than padded.
    #[test]
    fn test_a_damaged_secret_is_refused() {
        assert!(NetworkIdentity::from_secret_hex("").is_none());
        assert!(NetworkIdentity::from_secret_hex("00").is_none());
        assert!(NetworkIdentity::from_secret_hex(&"00".repeat(SECRET_LENGTH - 1)).is_none());
        assert!(NetworkIdentity::from_secret_hex(&"zz".repeat(SECRET_LENGTH)).is_none());
    }

    /// The announced key parses back, and a malformed one does not.
    #[test]
    fn test_the_announced_key_parses_back() {
        let identity = NetworkIdentity::generate();
        let parsed = NetworkIdentity::public_key_from_hex(&identity.public_hex())
            .expect("a well-formed key");

        assert_eq!(parsed, identity.public_key());
        assert!(NetworkIdentity::public_key_from_hex("").is_none());
        assert!(NetworkIdentity::public_key_from_hex(&"00".repeat(SECRET_LENGTH - 1)).is_none());
    }

    /// Both ends of a pair compute the same shared secret.
    #[test]
    fn test_two_parties_agree_on_the_same_secret() {
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();

        assert_eq!(
            alice.agree(bob.public_key()),
            bob.agree(alice.public_key()),
            "X25519 is symmetric"
        );
    }

    /// The first run writes an identity and every later run reads the same one.
    #[test]
    fn test_the_identity_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = IdentityStore::new(dir.path());

        assert!(store.load().is_none(), "the first run has nothing to read");

        let first = store.load_or_create();
        assert_eq!(
            store.load().map(|identity| identity.public_hex()),
            Some(first.public_hex()),
            "the generated identity must be persisted at once"
        );

        // A restart: a new store over the same directory.
        let second = IdentityStore::new(dir.path()).load_or_create();
        assert_eq!(first.public_hex(), second.public_hex());
        assert_eq!(first.secret_hex(), second.secret_hex());
    }

    /// A corrupt file is replaced, and the replacement is durable.
    #[test]
    fn test_a_corrupt_file_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = IdentityStore::new(dir.path());
        std::fs::write(store.path(), b"{ not an identity").expect("write");

        let created = store.load_or_create();
        assert_eq!(
            store.load().map(|identity| identity.public_hex()),
            Some(created.public_hex())
        );
        assert!(
            !store.path().with_extension("tmp").exists(),
            "the temporary file must be renamed, not left behind"
        );
    }

    /// A layout this build does not know is refused rather than misread.
    #[test]
    fn test_an_unknown_layout_version_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = IdentityStore::new(dir.path());
        let secret = NetworkIdentity::generate().secret_hex();

        for version in [IDENTITY_VERSION - 1, IDENTITY_VERSION + 1] {
            let stored = format!("{{\"version\":{version},\"secret\":\"{secret}\"}}");
            std::fs::write(store.path(), stored).expect("write");
            assert!(
                store.load().is_none(),
                "version {version} must not be read as {IDENTITY_VERSION}"
            );
        }
    }

    /// A future layout is not silently upgraded either: the secret of a version
    /// this build cannot interpret is not reused.
    #[test]
    fn test_a_future_layout_is_not_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = IdentityStore::new(dir.path());
        let secret = NetworkIdentity::generate().secret_hex();
        std::fs::write(
            store.path(),
            format!(
                "{{\"version\":{},\"secret\":\"{secret}\"}}",
                IDENTITY_VERSION + 1
            ),
        )
        .expect("write");

        let created = store.load_or_create();
        assert_ne!(
            created.secret_hex(),
            secret,
            "a file this build cannot read must not decide the identity"
        );
    }

    /// The file holds a private key, so it is not readable by everybody.
    #[cfg(unix)]
    #[test]
    fn test_the_identity_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = IdentityStore::new(dir.path());
        store.store(&NetworkIdentity::generate()).expect("store");

        let mode = std::fs::metadata(store.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "the identity file must not be group or world readable, got {mode:o}"
        );
    }

    /// Both ends of a connection agree on the same key, and it is not the session key.
    #[test]
    fn test_the_agreed_pair_key_is_the_same_on_both_ends() {
        let session = crate::crypto::CryptoManager::generate_key();
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();
        let alice_ephemeral = EphemeralKey::generate();
        let bob_ephemeral = EphemeralKey::generate();

        let from_alice = pair_key(
            &session,
            &alice,
            &alice_ephemeral,
            bob.public_key(),
            bob_ephemeral.public_key(),
        )
        .expect("a pair key");
        let from_bob = pair_key(
            &session,
            &bob,
            &bob_ephemeral,
            alice.public_key(),
            alice_ephemeral.public_key(),
        )
        .expect("a pair key");

        assert_eq!(from_alice, from_bob, "the agreement is symmetric");
        assert_eq!(from_alice.len(), KEY_LENGTH);
        assert_ne!(
            from_alice.as_slice(),
            session.as_slice(),
            "the pair key is not the session key"
        );
    }

    /// A second connection of the same pair agrees a *different* key.
    ///
    /// The ephemeral is what this buys: a handshake recorded on the wire does not yield
    /// the key of a later connection, and a key one connection used does not open
    /// another.
    #[test]
    fn test_every_connection_agrees_a_different_key() {
        let session = crate::crypto::CryptoManager::generate_key();
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();

        let first = pair_key(
            &session,
            &alice,
            &EphemeralKey::generate(),
            bob.public_key(),
            EphemeralKey::generate().public_key(),
        )
        .expect("a pair key");

        // The same statics, a new connection: a new ephemeral pair on each side.
        let bob_second = EphemeralKey::generate();
        let second = pair_key(
            &session,
            &alice,
            &EphemeralKey::generate(),
            bob.public_key(),
            bob_second.public_key(),
        )
        .expect("a pair key");

        assert_ne!(first, second, "each connection has a key of its own");
    }

    /// Knowing the session key and both announced identities is not enough.
    ///
    /// This is the property the agreement exists for: the static derivation
    /// ([`crate::crypto::contact_key`]) is reproducible by anybody who has seen the
    /// handshake — every member holds the session key — while the agreed key needs a
    /// secret that never left either end.
    #[test]
    fn test_the_agreed_key_is_not_the_observable_derivation() {
        let session = crate::crypto::CryptoManager::generate_key();
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();

        let agreed = pair_key(
            &session,
            &alice,
            &EphemeralKey::generate(),
            bob.public_key(),
            EphemeralKey::generate().public_key(),
        )
        .expect("a pair key");
        let observable =
            crate::crypto::contact_key(&session, &alice.public_hex(), &bob.public_hex())
                .expect("a static pair key");

        assert_eq!(observable.len(), KEY_LENGTH);
        assert_ne!(
            agreed.to_vec(),
            observable,
            "an observer must not be able to derive what the pair agreed"
        );
    }

    /// A participant that substitutes its own identity can talk to one end, but the two
    /// honest ends no longer agree — which is what the pin and the fingerprint
    /// comparison report.
    #[test]
    fn test_a_substituted_identity_does_not_go_unnoticed() {
        let session = crate::crypto::CryptoManager::generate_key();
        let alice = NetworkIdentity::generate();
        let bob = NetworkIdentity::generate();
        let alice_ephemeral = EphemeralKey::generate();
        let bob_ephemeral = EphemeralKey::generate();

        let alice_to_bob = pair_key(
            &session,
            &alice,
            &alice_ephemeral,
            bob.public_key(),
            bob_ephemeral.public_key(),
        )
        .expect("a pair key");
        let bob_to_alice = pair_key(
            &session,
            &bob,
            &bob_ephemeral,
            alice.public_key(),
            alice_ephemeral.public_key(),
        )
        .expect("a pair key");
        assert_eq!(alice_to_bob, bob_to_alice);

        // The participant in the middle runs the same exchange with its own identity
        // and its own ephemeral: it can agree with Alice...
        let attacker = NetworkIdentity::generate();
        let attacker_ephemeral = EphemeralKey::generate();
        let alice_to_attacker = pair_key(
            &session,
            &alice,
            &alice_ephemeral,
            attacker.public_key(),
            attacker_ephemeral.public_key(),
        )
        .expect("a pair key");
        let attacker_to_alice = pair_key(
            &session,
            &attacker,
            &attacker_ephemeral,
            alice.public_key(),
            alice_ephemeral.public_key(),
        )
        .expect("a pair key");
        assert_eq!(alice_to_attacker, attacker_to_alice);

        // ...but that is not the key Bob has, so the traffic cannot be passed on
        // unchanged — and the identity the attacker presented is not the pinned one.
        assert_ne!(
            alice_to_attacker, alice_to_bob,
            "a substituted identity yields a different key"
        );
    }

    /// A connection's ephemeral is named, not disclosed, by its debug view.
    #[test]
    fn test_the_ephemeral_debug_view_does_not_leak_the_secret() {
        let ephemeral = EphemeralKey::generate();
        let rendered = format!("{ephemeral:?}");

        assert!(rendered.contains(&ephemeral.public_hex()));
        assert!(
            !rendered.contains(&hex::encode(ephemeral.secret)),
            "the secret half must never reach a log record"
        );
    }

    /// The debug view names the identity without disclosing it.
    #[test]
    fn test_the_debug_view_does_not_leak_the_secret() {
        let identity = NetworkIdentity::generate();
        let rendered = format!("{identity:?}");

        assert!(!rendered.contains(&identity.secret_hex()));
        assert!(rendered.contains(&identity.fingerprint()));
    }
}
