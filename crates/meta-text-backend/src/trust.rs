/*!
 * trust.rs
 *
 * Trust on first use for the network identity: the public key a peer announced,
 * remembered so a change can be seen.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-14
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`PinStore::observe`] records the identity a peer announces and reports
 *   whether it is new, already known, or a *change* from the pinned value
 * - the pins live in `<data-dir>/net-peers.json`, bounded and versioned, written at
 *   most twice per nickname per run, so a peer cannot grow them — or drive the disk
 *   — on their own schedule (R7)
 * - the counters behind [`PinStore::changed`] / [`PinStore::refused`] are what
 *   `/metrics` reports, so a changed identity is visible rather than only logged
 *
 * # What a pin means today, and what it will mean
 *
 * On TCP a peer is *named* by the nickname it announces, and a nickname is not an
 * identity: it is self-asserted, it can be changed at any time, and any holder of
 * the passphrase can claim one. A pin therefore cannot *authenticate* a peer
 * today — anyone able to reach the socket can present a fresh key under a fresh
 * name, and the honest answer to that is that the identity is not yet verified.
 *
 * What the pin provides is *detection*: the identity of a peer that was seen
 * before is remembered, so when the same nickname announces a different key, the
 * user is told (`peer_identities_changed`, and a `warn` line naming both
 * fingerprints) instead of the change passing unnoticed. The comparison a user can
 * make out of band is the fingerprint
 * ([`crate::identity::NetworkIdentity::fingerprint`]).
 *
 * The pin becomes load-bearing when the open half of A12 is closed: a key
 * agreement authenticated with this pinned key is what turns "the same name sent a
 * different key" from a warning into a refusal. Nothing here has to change shape
 * for that — the pin already stores the public key such a handshake takes.
 *
 * # Failure policy
 *
 * Like the identity store, reading never fails: a missing file is the first run,
 * and a corrupt, malformed or over-capacity one is reported at `warn` and treated
 * as "nothing pinned yet". Writing is best effort — a failed write costs the
 * durable half of the pin, not the session, because the in-memory table still
 * detects a change for as long as the process runs.
 */

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::identity::NetworkIdentity;

/// File name the peer pins are persisted to.
pub const PIN_FILE: &str = "net-peers.json";

/// Layout version of the file.
pub const PIN_VERSION: u32 = 1;

/// How many peer identities are remembered.
///
/// A nickname costs nothing to produce — a peer only has to connect and announce
/// one — so an unbounded table is the one class of growth R7 exists to prevent.
/// The bound is the friend limit ([`crate::config`]'s `app.max_friends`, 1024 by
/// default): remembering more distinct peer names than a peer can have friends is
/// already more than a session can use.
pub const PIN_CAPACITY: usize = 1024;

/// What happened to a peer identity that was just announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOutcome {
    /// The nickname was not known: its identity is now pinned.
    FirstSeen,

    /// The nickname is known and announced the same identity as before.
    Known,

    /// The nickname is known and announced a *different* identity.
    ///
    /// The pin is updated (see the module documentation: the identity is not yet
    /// verifiable, so refusing would only push the peer to a new nickname) and the
    /// change is counted and logged.
    Changed {
        /// The identity that was pinned before, uppercase hexadecimal.
        previous: String,
    },

    /// Nothing was pinned: the announcement was unusable (no nickname, or a key
    /// that is not 64 hexadecimal characters) or the table is full.
    Refused,
}

/// One remembered peer identity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    /// The identity the peer announced, uppercase hexadecimal.
    public_key: String,

    /// Milliseconds since the Unix epoch when the nickname was first seen.
    first_seen_unix_ms: u64,

    /// Whether this run saw the identity change under a nickname that was already
    /// pinned. A property of the run, so it is not persisted.
    changed: bool,

    /// Whether the one change this run is willing to *write* for this nickname has
    /// already been attempted.
    ///
    /// See [`PinStore::observe`]: a nickname is written when it is first seen and once
    /// more when it changes, and a later change is kept in memory only. That is what
    /// bounds the file's writes by the size of the table instead of by the number of
    /// announcements a peer decides to send (R7).
    change_written: bool,
}

/// A pinned identity as the transport reports it.
///
/// The core turns this into the wire view
/// ([`meta_text_proto::ipc::protocol::PeerIdentityView`]); the backend keeps its
/// own type so a subsystem never has to name a protocol struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The nickname the peer announced.
    pub nickname: String,

    /// The identity it announced, uppercase hexadecimal.
    pub public_key: String,

    /// Fingerprint of [`Self::public_key`], in groups of four.
    pub fingerprint: String,

    /// Whether this run saw the identity change.
    pub changed: bool,
}

/// The stored layout: a version and the pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPin {
    /// The nickname, lowercased.
    nickname: String,

    /// The identity, uppercase hexadecimal.
    public_key: String,

    /// Milliseconds since the Unix epoch when the nickname was first seen.
    first_seen_unix_ms: u64,
}

/// The stored file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPins {
    /// Layout version; [`PIN_VERSION`] for anything this build writes.
    version: u32,

    /// The pins, sorted by nickname when written so the file is deterministic.
    pins: Vec<StoredPin>,
}

/// The set of peer identities this instance has seen.
///
/// Cheap to clone (a path, a shared table and shared counters), so the transport
/// and the bridge task can each hold one.
#[derive(Debug, Clone)]
pub struct PinStore {
    /// Where the pins are written (`None` in a volatile store).
    path: Option<PathBuf>,

    /// The pins, keyed by lowercased nickname.
    pins: Arc<StdMutex<HashMap<String, Pin>>>,

    /// Serialises writers; see [`PinStore::persist`].
    writing: Arc<StdMutex<()>>,

    /// Identities that changed under a nickname already in the table.
    changed: Arc<AtomicU64>,

    /// Announcements that could not be pinned: unusable, or a full table.
    refused: Arc<AtomicU64>,
}

impl PinStore {
    /// Load the pins for a data directory.
    ///
    /// # Arguments
    ///
    /// * `data_dir` - Directory the session snapshot lives in
    ///
    /// # Returns
    ///
    /// Returns the store. A missing file is the first run; a corrupt, malformed or
    /// over-capacity file is reported and read as far as it is usable, so an
    /// unusable file costs the pins rather than the session.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let store = Self::with_path(Some(data_dir.join(PIN_FILE)));
        store.restore();
        store
    }

    /// A store that remembers for this process only.
    ///
    /// What a caller with no data directory gets (a test, or an embedder that keeps
    /// its own state): a change under a known nickname is still detected for as long
    /// as the process runs, and nothing is written.
    #[must_use]
    pub fn volatile() -> Self {
        Self::with_path(None)
    }

    /// Build an empty store that writes to `path`, if it has one.
    fn with_path(path: Option<PathBuf>) -> Self {
        Self {
            path,
            pins: Arc::new(StdMutex::new(HashMap::new())),
            writing: Arc::new(StdMutex::new(())),
            changed: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Where the pins are written, if this store has a file at all.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// How many peer identities are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.pins).len()
    }

    /// Whether nothing is remembered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Identities that changed under a nickname already in the table.
    ///
    /// Non-zero means a peer that was seen before announced a different key — the
    /// one thing a pin can tell a user about an identity that is not yet verified.
    #[must_use]
    pub fn changed(&self) -> u64 {
        self.changed.load(Ordering::SeqCst)
    }

    /// Announcements that could not be pinned: unusable, or a full table.
    ///
    /// Non-zero means an identity was seen and *not* remembered, so a later change
    /// of it cannot be reported. The table is bounded because a nickname is free to
    /// produce.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::SeqCst)
    }

    /// The identity pinned for a nickname, if there is one.
    ///
    /// # Arguments
    ///
    /// * `nickname` - The nickname a peer announced (matched case-insensitively)
    ///
    /// # Returns
    ///
    /// Returns the pinned public key, uppercase hexadecimal.
    #[must_use]
    pub fn pinned(&self, nickname: &str) -> Option<String> {
        lock(&self.pins)
            .get(&normalize_nickname(nickname))
            .map(|pin| pin.public_key.clone())
    }

    /// Record the identity a peer announced.
    ///
    /// # Arguments
    ///
    /// * `nickname` - The nickname the peer announced
    /// * `public_key` - The identity it announced, 64 hexadecimal characters
    ///
    /// # Returns
    ///
    /// Returns what the announcement was relative to the pin: [`PinOutcome::FirstSeen`]
    /// when the nickname is new, [`PinOutcome::Known`] when it announced the same key
    /// as before, [`PinOutcome::Changed`] when it announced a different one, and
    /// [`PinOutcome::Refused`] when nothing could be pinned.
    ///
    /// A change is recorded and counted, not refused: the identity is announced in
    /// the clear and is not yet verifiable (see the module documentation), so
    /// refusing would not stop the peer — it would only stop us from knowing.
    ///
    /// # What is written, and what is not
    ///
    /// Every announcement is compared and counted, but the **file** is written on a
    /// first sighting and on the *first* change of a nickname in this run — at most
    /// `2 × PIN_CAPACITY` writes per run, whatever rate a peer announces identities at.
    /// A later change is kept in memory (the pin moves on, so the next frame is compared
    /// against it, and `identities()` reports it) and is not written again; a restart then
    /// reports that change once more, because the file holds the earlier value. Stale is
    /// the safe direction — it costs a repeated warning, not a missed one — and it is what
    /// keeps the write rate a function of the table's size rather than of a peer's frame
    /// rate (R7).
    pub fn observe(&self, nickname: &str, public_key: &str) -> PinOutcome {
        let nickname = normalize_nickname(nickname);
        if nickname.is_empty() {
            self.refused.fetch_add(1, Ordering::SeqCst);
            warn!("⚠️ Refused an identity announcement without a nickname to pin it to");
            return PinOutcome::Refused;
        }

        let Some(public_key) =
            NetworkIdentity::public_key_from_hex(public_key).map(hex::encode_upper)
        else {
            self.refused.fetch_add(1, Ordering::SeqCst);
            warn!(
                "⚠️ Refused an unusable identity announcement from '{nickname}': \
                 expected 64 hexadecimal characters"
            );
            return PinOutcome::Refused;
        };

        let (outcome, write) = {
            let mut pins = lock(&self.pins);
            // `if let` rather than `match`: a guard (`None if pins.len() …`) would hold the
            // mutable borrow of the table across itself, and the table is borrowed again to
            // insert.
            let decision = if let Some(pin) = pins.get_mut(&nickname) {
                if pin.public_key == public_key {
                    (PinOutcome::Known, false)
                } else {
                    let previous = pin.public_key.clone();
                    // When the *name* was first seen does not change; the key it
                    // announces is what is being reported. The pin itself moves on, so
                    // the next frame is compared against what this one announced.
                    pin.public_key.clone_from(&public_key);
                    pin.changed = true;
                    // One change per nickname is written per run; a later one is still
                    // reported and counted, but does not rewrite the file.
                    let write = !pin.change_written;
                    pin.change_written = true;
                    (PinOutcome::Changed { previous }, write)
                }
            } else if pins.len() >= PIN_CAPACITY {
                (PinOutcome::Refused, false)
            } else {
                pins.insert(
                    nickname.clone(),
                    Pin {
                        public_key: public_key.clone(),
                        first_seen_unix_ms: now_ms(),
                        changed: false,
                        change_written: false,
                    },
                );
                // A new nickname is always written: that is the entry a restart reads
                // back, and there can be at most `PIN_CAPACITY` of them.
                (PinOutcome::FirstSeen, true)
            };
            // The table lock ends here: the file write below must not hold it.
            drop(pins);
            decision
        };

        match &outcome {
            // Nothing changed, so neither the file nor the counters do.
            PinOutcome::Known => {}
            PinOutcome::Refused => {
                self.refused.fetch_add(1, Ordering::SeqCst);
                warn!(
                    "⚠️ Could not pin an identity: {PIN_CAPACITY} peer identities are \
                     remembered at most"
                );
            }
            PinOutcome::FirstSeen => {
                debug!("📌 Pinned the announced identity");
                if write {
                    self.persist();
                }
            }
            PinOutcome::Changed { previous } => {
                self.changed.fetch_add(1, Ordering::SeqCst);
                warn!(
                    "⚠️ A known nickname announced a different identity: pinned {}, now {}",
                    fingerprint_of(previous),
                    fingerprint_of(&public_key)
                );
                if write {
                    self.persist();
                } else {
                    // The change is in memory and was reported; the file keeps the value
                    // written earlier in this run, so a restart reports the change again
                    // rather than the frame rate deciding how often we write.
                    debug!(
                        "📎 '{nickname}' changed again; the pin file keeps the value written \
                         earlier in this run"
                    );
                }
            }
        }
        outcome
    }
}

impl PinStore {
    /// Read the pins back into the table.
    ///
    /// A file written by this build is read entry by entry; an unusable entry (no
    /// nickname, a malformed key) or one past the bound is left out and counted, so a
    /// file that was edited by hand cannot raise the bound the transport advertises.
    fn restore(&self) {
        // A store with no file has nothing to read.
        let Some(path) = &self.path else {
            return;
        };

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!("🧪 No pinned identities to restore at {}", path.display());
                return;
            }
            Err(error) => {
                warn!("⚠️ Could not read {}: {error}", path.display());
                return;
            }
        };

        match serde_json::from_slice::<StoredPins>(&bytes) {
            Ok(stored) if stored.version == PIN_VERSION => {
                let mut dropped = 0_usize;
                {
                    let mut pins = lock(&self.pins);
                    for pin in stored.pins {
                        let nickname = normalize_nickname(&pin.nickname);
                        let Some(public_key) =
                            NetworkIdentity::public_key_from_hex(&pin.public_key)
                                .map(hex::encode_upper)
                        else {
                            dropped += 1;
                            continue;
                        };
                        if nickname.is_empty()
                            || (pins.len() >= PIN_CAPACITY && !pins.contains_key(&nickname))
                        {
                            dropped += 1;
                            continue;
                        }
                        pins.insert(
                            nickname,
                            Pin {
                                public_key,
                                first_seen_unix_ms: pin.first_seen_unix_ms,
                                changed: false,
                                change_written: false,
                            },
                        );
                    }
                }

                if dropped > 0 {
                    self.refused
                        .fetch_add(u64::try_from(dropped).unwrap_or(u64::MAX), Ordering::SeqCst);
                    warn!(
                        "⚠️ Left out {dropped} unusable or excess pinned identit(ies) in {}",
                        path.display()
                    );
                }
                debug!(
                    "📌 Restored {} pinned identit(ies) from {}",
                    self.len(),
                    path.display()
                );
            }
            Ok(stored) => {
                warn!(
                    "⚠️ {} holds peer pin layout version {} and this build writes {PIN_VERSION}; \
                 starting from an empty table rather than guessing",
                    path.display(),
                    stored.version
                );
            }
            Err(error) => {
                warn!(
                    "⚠️ {} is not a readable peer pin file ({error}); starting from an empty table",
                    path.display()
                );
            }
        }
    }
}

impl PinStore {
    /// The current table, in a deterministic order.
    ///
    /// Sorted by nickname so that two runs with the same pins write the same file:
    /// a diff of the file is then a diff of what changed.
    fn snapshot(&self) -> StoredPins {
        let mut entries: Vec<StoredPin> = lock(&self.pins)
            .iter()
            .map(|(nickname, pin)| StoredPin {
                nickname: nickname.clone(),
                public_key: pin.public_key.clone(),
                first_seen_unix_ms: pin.first_seen_unix_ms,
            })
            .collect();
        entries.sort_by(|left, right| left.nickname.cmp(&right.nickname));

        StoredPins {
            version: PIN_VERSION,
            pins: entries,
        }
    }

    /// Publish the current table.
    ///
    /// The snapshot is taken *inside* the write lock, so a slower writer cannot
    /// publish a table that is missing a pin a faster one just added. The write is a
    /// whole-file atomic replace, like the identity store's; a failed write is
    /// reported and ignored, because the in-memory table still detects a change for
    /// as long as the process runs.
    fn persist(&self) {
        // A store with no file remembers for this process only.
        let Some(path) = &self.path else {
            return;
        };

        let _guard = lock(&self.writing);
        let state = self.snapshot();

        let bytes = match serde_json::to_vec(&state) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!("⚠️ Could not encode the peer pins: {error}");
                return;
            }
        };

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    warn!(
                        "⚠️ Could not create {} for the peer pins: {error}",
                        parent.display()
                    );
                    return;
                }
            }
        }

        let temporary = path.with_extension("tmp");
        if let Err(error) = std::fs::write(&temporary, &bytes) {
            warn!("⚠️ Could not write {}: {error}", temporary.display());
            return;
        }
        if let Err(error) = std::fs::rename(&temporary, path) {
            warn!(
                "⚠️ Could not publish the peer pins to {}: {error}",
                path.display()
            );
        }
    }

    /// Every pinned identity, with whether this run saw it change.
    ///
    /// Sorted by nickname, like the file, so a front-end renders a stable list.
    #[must_use]
    pub fn identities(&self) -> Vec<PeerIdentity> {
        let mut entries: Vec<PeerIdentity> = lock(&self.pins)
            .iter()
            .map(|(nickname, pin)| PeerIdentity {
                nickname: nickname.clone(),
                public_key: pin.public_key.clone(),
                fingerprint: fingerprint_of(&pin.public_key),
                changed: pin.changed,
            })
            .collect();
        entries.sort_by(|left, right| left.nickname.cmp(&right.nickname));
        entries
    }
}

/// Normalise a nickname for the table.
///
/// Nicknames are matched case-insensitively everywhere else (a message is addressed
/// by name), so a peer that re-announces its name in another case is the same peer
/// and must not look like a second one.
fn normalize_nickname(nickname: &str) -> String {
    nickname.trim().to_lowercase()
}

/// Milliseconds since the Unix epoch, saturating a clock set before it.
fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

/// The fingerprint of an announced key, or the key itself when it cannot be parsed.
///
/// The value is only used in a log line, and a value that does not parse is exactly
/// what a reader needs to see.
fn fingerprint_of(public_key: &str) -> String {
    NetworkIdentity::public_key_from_hex(public_key).map_or_else(
        || public_key.to_string(),
        |key| crate::identity::fingerprint(&key),
    )
}

/// Take a mutex, treating a poisoned one as usable (see [`crate::identity`]).
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed 64 character key derived from one byte.
    fn key(seed: u8) -> String {
        hex::encode_upper([seed; 32])
    }

    /// A store over a fresh directory, plus the directory itself.
    fn store() -> (tempfile::TempDir, PinStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PinStore::load(dir.path());
        (dir, store)
    }

    /// The first identity a nickname announces is remembered.
    #[test]
    fn test_the_first_announcement_is_pinned() {
        let (_dir, store) = store();

        assert_eq!(store.observe("Alice", &key(1)), PinOutcome::FirstSeen);
        assert_eq!(store.len(), 1);
        assert_eq!(store.pinned("Alice"), Some(key(1)));
        assert_eq!(store.changed(), 0);
        assert_eq!(store.refused(), 0);
    }

    /// The same identity again is not an event.
    #[test]
    fn test_the_same_identity_is_merely_known() {
        let (_dir, store) = store();

        store.observe("Alice", &key(1));
        assert_eq!(store.observe("Alice", &key(1)), PinOutcome::Known);
        assert_eq!(store.changed(), 0, "an unchanged identity is not a change");
        assert_eq!(store.len(), 1);
    }

    /// A different identity under a known nickname is reported, and becomes the pin.
    #[test]
    fn test_a_different_identity_is_reported_as_a_change() {
        let (_dir, store) = store();

        store.observe("Alice", &key(1));
        let outcome = store.observe("Alice", &key(2));

        assert_eq!(outcome, PinOutcome::Changed { previous: key(1) });
        assert_eq!(store.changed(), 1);
        assert_eq!(
            store.pinned("Alice"),
            Some(key(2)),
            "the announcement is recorded, not refused"
        );
    }

    /// A nickname is the same nickname in any case, like an address.
    #[test]
    fn test_nicknames_are_matched_case_insensitively() {
        let (_dir, store) = store();

        store.observe("Alice", &key(1));
        assert_eq!(store.observe("  alice ", &key(1)), PinOutcome::Known);
        assert_eq!(store.len(), 1, "one peer, one pin");
    }

    /// A pin outlives the process: that is what makes a change visible at all.
    #[test]
    fn test_pins_survive_a_restart() {
        let (dir, store) = store();
        store.observe("Alice", &key(1));
        store.observe("Bob", &key(2));

        // A restart: a new store over the same directory.
        let restarted = PinStore::load(dir.path());
        assert_eq!(restarted.len(), 2);
        assert_eq!(restarted.pinned("Alice"), Some(key(1)));
        assert_eq!(restarted.pinned("Bob"), Some(key(2)));
        assert_eq!(restarted.changed(), 0, "restoring is not a change");

        // The peer that was pinned last run announcing a new identity is a *change*
        // across the restart — the whole reason the table is a file.
        assert_eq!(
            restarted.observe("Alice", &key(3)),
            PinOutcome::Changed { previous: key(1) }
        );
        assert_eq!(restarted.changed(), 1);
    }

    /// An announcement that cannot be pinned is refused and counted.
    #[test]
    fn test_an_unusable_announcement_is_refused() {
        let (_dir, store) = store();

        assert_eq!(store.observe("", &key(1)), PinOutcome::Refused);
        assert_eq!(store.observe("   ", &key(1)), PinOutcome::Refused);
        assert_eq!(store.observe("Alice", ""), PinOutcome::Refused);
        assert_eq!(store.observe("Alice", "not-a-key"), PinOutcome::Refused);
        assert_eq!(
            store.observe("Alice", &"00".repeat(31)),
            PinOutcome::Refused,
            "a truncated key is not a key"
        );

        assert_eq!(store.refused(), 5);
        assert!(store.is_empty(), "nothing usable was stored");
        assert_eq!(store.changed(), 0);
    }

    /// The table is bounded, and a refused pin is counted rather than silent.
    #[test]
    fn test_the_table_is_bounded() {
        let (_dir, store) = store();

        for index in 0..PIN_CAPACITY {
            let nickname = format!("peer{index}");
            let key = format!("{index:064X}");
            assert_eq!(
                store.observe(&nickname, &key),
                PinOutcome::FirstSeen,
                "entry {index} is inside the bound"
            );
        }
        assert_eq!(store.len(), PIN_CAPACITY);

        assert_eq!(
            store.observe("one-too-many", &"FF".repeat(32)),
            PinOutcome::Refused
        );
        assert_eq!(store.len(), PIN_CAPACITY, "the bound holds");
        assert_eq!(store.refused(), 1);

        // A nickname already in the table is still updated when it is full: the
        // bound is on the table, not on the peer.
        assert!(matches!(
            store.observe("peer0", &"AB".repeat(32)),
            PinOutcome::Changed { .. }
        ));
    }

    /// A file with more pins than the bound is read up to the bound, and the
    /// excess is reported.
    #[test]
    fn test_an_over_capacity_file_is_read_up_to_the_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pins: Vec<String> = (0..PIN_CAPACITY + 2)
            .map(|index| {
                let public_key = format!("{index:064X}");
                format!(
                    "{{\"nickname\":\"peer{index}\",\"public_key\":\"{public_key}\",\
                     \"first_seen_unix_ms\":1}}"
                )
            })
            .collect();
        std::fs::write(
            dir.path().join(PIN_FILE),
            format!(
                "{{\"version\":{PIN_VERSION},\"pins\":[{}]}}",
                pins.join(",")
            ),
        )
        .expect("write");

        let store = PinStore::load(dir.path());
        assert_eq!(store.len(), PIN_CAPACITY);
        assert_eq!(store.refused(), 2, "the excess is counted, not silent");
    }

    /// A corrupt file is read as an empty table rather than stopping the client.
    #[test]
    fn test_a_corrupt_file_is_read_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PIN_FILE), b"{ not a pin file").expect("write");

        let store = PinStore::load(dir.path());
        assert!(store.is_empty());
        // And the next announcement replaces it.
        assert_eq!(store.observe("Alice", &key(1)), PinOutcome::FirstSeen);
        assert_eq!(PinStore::load(dir.path()).pinned("Alice"), Some(key(1)));
    }

    /// A layout this build does not know is refused rather than misread.
    #[test]
    fn test_an_unknown_layout_version_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stored = format!(
            "{{\"version\":{},\"pins\":[{{\"nickname\":\"alice\",\"public_key\":\"{}\",\
             \"first_seen_unix_ms\":1}}]}}",
            PIN_VERSION + 1,
            key(1)
        );
        std::fs::write(dir.path().join(PIN_FILE), stored).expect("write");

        let store = PinStore::load(dir.path());
        assert!(store.is_empty(), "a future layout must not be guessed at");
    }

    /// The file is written in a stable order, so a diff is a diff of what changed.
    #[test]
    fn test_the_file_lists_pins_in_a_stable_order() {
        let (dir, store) = store();
        store.observe("carol", &key(3));
        store.observe("alice", &key(1));
        store.observe("bob", &key(2));

        let written = std::fs::read_to_string(dir.path().join(PIN_FILE)).expect("read");
        let alice = written.find("alice").expect("alice");
        let bob = written.find("bob").expect("bob");
        let carol = written.find("carol").expect("carol");
        assert!(alice < bob && bob < carol, "pins are sorted by nickname");
    }

    /// A flood of changes cannot drive the disk.
    ///
    /// Every announcement is compared and counted, and the pin moves on in memory, but
    /// the file is only written for a first sighting and for the *first* change of a
    /// nickname in a run. A restart therefore reports the change again, which is the safe
    /// direction: a repeated warning, never a missed one.
    #[test]
    fn test_a_flood_of_changes_does_not_rewrite_the_file() {
        let (dir, store) = store();

        // First sighting, then the one change this run writes.
        assert_eq!(store.observe("Alice", &key(1)), PinOutcome::FirstSeen);
        assert!(matches!(
            store.observe("Alice", &key(2)),
            PinOutcome::Changed { .. }
        ));
        let written = std::fs::read_to_string(dir.path().join(PIN_FILE)).expect("read");

        // Twenty more changes: reported and counted, not written.
        for seed in 3..=22 {
            assert!(
                matches!(
                    store.observe("Alice", &key(seed)),
                    PinOutcome::Changed { .. }
                ),
                "change {seed} must still be reported"
            );
        }
        assert_eq!(store.changed(), 21);
        assert_eq!(
            store.pinned("Alice"),
            Some(key(22)),
            "the in-memory pin follows the peer"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(PIN_FILE)).expect("read"),
            written,
            "the file must not be rewritten for every change"
        );

        // The consequence, stated plainly: a restart sees the value that was written, so
        // it reports the change again instead of silently accepting it.
        let restarted = PinStore::load(dir.path());
        assert_eq!(restarted.pinned("Alice"), Some(key(2)));
        assert!(matches!(
            restarted.observe("Alice", &key(22)),
            PinOutcome::Changed { .. }
        ));
    }

    /// A volatile store detects a change without touching the disk.
    #[test]
    fn test_a_volatile_store_keeps_the_pins_in_memory() {
        let store = PinStore::volatile();
        assert!(store.path().is_none(), "there is nowhere to write");

        assert_eq!(store.observe("Alice", &key(1)), PinOutcome::FirstSeen);
        assert_eq!(
            store.observe("Alice", &key(2)),
            PinOutcome::Changed { previous: key(1) }
        );
        assert_eq!(store.pinned("Alice"), Some(key(2)));
        assert_eq!(store.changed(), 1);
    }

    /// The per-peer view names the fingerprint and flags a change per nickname.
    #[test]
    fn test_the_peer_view_reports_a_change_per_nickname() {
        let (_dir, store) = store();
        store.observe("bob", &key(2));
        store.observe("alice", &key(1));
        store.observe("alice", &key(3));

        let views = store.identities();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].nickname, "alice", "sorted by nickname");
        assert_eq!(views[0].public_key, key(3));
        assert_eq!(views[0].fingerprint, fingerprint_of(&key(3)));
        assert!(views[0].changed, "alice announced two identities");
        assert!(!views[1].changed, "bob announced one");
    }
}
