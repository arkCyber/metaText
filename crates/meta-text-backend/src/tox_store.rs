/*!
 * `tox_store.rs`
 *
 * The Tox transport's own persistence: the pending friend requests and the
 * outbox survive a restart.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-14
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`ToxStore`] reads and writes one bounded JSON file holding the two pieces
 *   of state `toxcore` does *not* save: the unanswered friend requests and the
 *   payloads buffered for an offline friend
 * - keyed by **public key**, because a friend number is assigned in the order
 *   the savedata lists friends and is therefore only stable while that list is
 * - written by atomic replace, serialised between writers, and never larger than
 *   the caps the transport already advertises
 *
 * # What has to be remembered, and why `toxcore` cannot
 *
 * The savedata (`tox-savedata.bin`) is the identity: keys, nospam, the friend
 * list and the conferences. Two things a user can observe are missing from it:
 *
 * 1. **An incoming friend request.** `toxcore` delivers it as a callback and
 *    treats "not answered" as the refusal, so the savedata has no field for it.
 *    Until this store existed a request that arrived before a restart was gone:
 *    the requester had to send it again, and a request the user had read but not
 *    yet answered could no longer be answered.
 * 2. **A payload buffered for an offline friend.** The transport held the outbox
 *    in memory, so `queued` quietly meant "queued until I exit" — a restart
 *    dropped a message the sender had been told was waiting.
 *
 * # Why the file is keyed by public key
 *
 * The live outbox is keyed by friend *number*, because that is what `toxcore`
 * takes. A number is assigned from the order of the savedata's friend list, so
 * removing a friend renumbers the ones after it; a file keyed by number would
 * therefore deliver a message to the wrong peer after an unrelated change. The
 * public key is the peer's identity, and the transport translates keys back into
 * this run's numbers when it restores the queue.
 *
 * # Failure policy
 *
 * Reading never fails: a missing file is the first run, and a corrupt or
 * unreadable one is reported at `warn` and treated as empty rather than stopping
 * the client — the queue is a durability *extra*, while the identity lives in the
 * savedata. Writing is best effort for the same reason: a failed write costs
 * durability, not delivery, because the payload is still in memory. The file
 * carries no key material, so it is not a secret; it is bounded by the same caps
 * the transport advertises ([`crate::transport::PENDING_REQUESTS_CAPACITY`] and
 * [`crate::transport::TOX_OUTBOX_CAPACITY`]), which is what keeps a remote party
 * from growing it on their own schedule (R7).
 */

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::types::MessageKind;

/// File name the Tox transport writes its pending state to.
///
/// It sits next to `tox-savedata.bin`: the savedata is what `toxcore` keeps, this
/// is everything it does not.
pub const STORE_FILE: &str = "tox-state.json";

/// Layout version of the file.
///
/// A reader only accepts the version it knows. A mismatch is reported and treated
/// as an empty state instead of being guessed at, so a future layout cannot be
/// misread as this one.
pub const STORE_VERSION: u32 = 1;

/// A friend request that has not been answered, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRequest {
    /// The requester's public key (64 hexadecimal characters).
    pub public_key: String,

    /// The message attached to the request.
    pub message: String,
}

/// A payload waiting for an offline friend, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    /// The friend's public key (64 hexadecimal characters).
    ///
    /// The key, not the friend number: see the module documentation.
    pub public_key: String,

    /// Identifier echoed to the front-end when the payload was buffered.
    pub message_id: u64,

    /// Whether the payload is a chat message or a third-person action.
    pub kind: MessageKind,

    /// Payload body, lowercase hexadecimal.
    ///
    /// Hex because the interface already encodes every other opaque byte string
    /// that way (`ContentType::Binary` bodies, group invitation tokens), and it
    /// keeps the file text: a queue can be read by a reviewer without a decoder.
    pub body: String,

    /// Milliseconds since the Unix epoch when the payload was buffered.
    ///
    /// A wall clock, not an `Instant`: an `Instant` is only meaningful inside the
    /// process that created it, and this queue has to be aged across a restart.
    pub queued_at_unix_ms: u64,
}

/// Everything the Tox transport has to remember across a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToxState {
    /// Layout version; [`STORE_VERSION`] for anything this build writes.
    pub version: u32,

    /// Identifier the next buffered payload is given.
    ///
    /// Persisted so that a delivery id names one payload for the lifetime of the
    /// queue rather than for the lifetime of the process.
    pub next_message_id: u64,

    /// Unanswered friend requests, oldest first.
    pub requests: Vec<StoredRequest>,

    /// Buffered payloads, in the order they must be delivered.
    pub outbox: Vec<StoredMessage>,
}

impl Default for ToxState {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            next_message_id: 1,
            requests: Vec::new(),
            outbox: Vec::new(),
        }
    }
}

impl ToxState {
    /// Whether there is nothing to remember.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.requests.is_empty() && self.outbox.is_empty()
    }
}

/// Milliseconds since the Unix epoch, saturating for a clock set before it.
#[must_use]
pub fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

/// The inverse of [`unix_ms`].
#[must_use]
pub fn from_unix_ms(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

/// The file the Tox transport persists [`ToxState`] to.
///
/// Cheap to clone (a path and a shared write lock), so the actor side and the
/// bridge task can each hold one and still write through the same serialisation.
#[derive(Debug, Clone)]
pub struct ToxStore {
    /// Where the state is written.
    path: PathBuf,

    /// Serialises writers; see [`ToxStore::update`].
    writing: Arc<StdMutex<()>>,
}

impl ToxStore {
    /// The store for a data directory.
    ///
    /// # Arguments
    ///
    /// * `data_dir` - Directory the Tox savedata lives in
    ///
    /// # Returns
    ///
    /// Returns the store; nothing is read or created until it is used.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(STORE_FILE),
            writing: Arc::new(StdMutex::new(())),
        }
    }

    /// Where the state is written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the state, or an empty one when there is nothing usable.
    ///
    /// # Returns
    ///
    /// Returns the stored state, or [`ToxState::default`] when the file does not
    /// exist (the first run), cannot be read, is not a readable queue, or carries
    /// a layout version this build does not know. Each of those is logged, so a
    /// queue that was silently thrown away is visible in the log rather than only
    /// in the missing messages.
    #[must_use]
    pub fn load(&self) -> ToxState {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!("🧪 No Tox queue to restore at {}", self.path.display());
                return ToxState::default();
            }
            Err(error) => {
                warn!("⚠️ Could not read {}: {error}", self.path.display());
                return ToxState::default();
            }
        };

        match serde_json::from_slice::<ToxState>(&bytes) {
            Ok(state) if state.version == STORE_VERSION => state,
            Ok(state) => {
                warn!(
                    "⚠️ {} holds Tox queue layout version {} and this build writes \
                     {STORE_VERSION}; starting from an empty queue rather than guessing",
                    self.path.display(),
                    state.version
                );
                ToxState::default()
            }
            Err(error) => {
                warn!(
                    "⚠️ {} is not a readable Tox queue ({error}); starting from an empty one",
                    self.path.display()
                );
                ToxState::default()
            }
        }
    }

    /// Publish a fresh snapshot of the state.
    ///
    /// The snapshot is taken *inside* the write lock, which is what makes
    /// concurrent writers safe: the actor thread queues a payload while the
    /// bridge task flushes the queue, and if the snapshot were taken before the
    /// lock, the slower writer could publish the older one and drop a payload
    /// from the file. With the snapshot under the lock, whoever writes last has
    /// seen every mutation that finished before it.
    ///
    /// The write itself is a whole-state atomic replace: a temporary file next to
    /// the target, then a rename. A reader therefore sees either the previous
    /// snapshot or this one, never a half-written queue.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - Called once, under the lock, to produce the state to write
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` once the rename succeeded.
    ///
    /// # Errors
    ///
    /// Returns the i/o error when the directory cannot be created, the temporary
    /// file cannot be written or the rename fails. The caller reports it and
    /// carries on: the queue is still in memory, so a failure costs durability
    /// rather than delivery.
    pub fn update<F>(&self, snapshot: F) -> std::io::Result<()>
    where
        F: FnOnce() -> ToxState,
    {
        // Nothing else takes this lock, and no caller holds one of the
        // transport's own locks inside the closure, so this ordering cannot
        // deadlock against the bridge task.
        let _guard = lock(&self.writing);
        let state = snapshot();

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let bytes = serde_json::to_vec(&state)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, &bytes)?;
        std::fs::rename(&temporary, &self.path)?;

        debug!(
            "💾 Persisted {} pending request(s) and {} queued payload(s) to {}",
            state.requests.len(),
            state.outbox.len(),
            self.path.display()
        );
        Ok(())
    }
}

/// Take a mutex, treating a poisoned one as usable.
///
/// A panic while a lock is held leaves data the transport can rebuild (an
/// in-memory queue), and refusing to touch it afterwards would turn one panic
/// into a permanently broken instance, so the poison flag is ignored — the same
/// policy the transport itself follows.
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state with one request and one buffered payload.
    fn sample() -> ToxState {
        ToxState {
            version: STORE_VERSION,
            next_message_id: 7,
            requests: vec![StoredRequest {
                public_key: "AB".repeat(32),
                message: "hi from Alice".to_string(),
            }],
            outbox: vec![StoredMessage {
                public_key: "CD".repeat(32),
                message_id: 5,
                kind: MessageKind::Action,
                body: "68656c6c6f".to_string(),
                queued_at_unix_ms: 1_700_000_000_000,
            }],
        }
    }

    /// A store that has never run reports nothing to restore.
    #[test]
    fn test_a_missing_file_is_an_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());

        let state = store.load();
        assert!(state.is_empty());
        assert_eq!(state.version, STORE_VERSION);
        assert_eq!(state.next_message_id, 1);
        // The queue lives next to the savedata, in the data directory itself.
        assert_eq!(store.path(), dir.path().join(STORE_FILE));
    }

    /// What was published comes back unchanged, in order.
    #[test]
    fn test_state_round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());

        let mut state = sample();
        let second = StoredMessage {
            message_id: 6,
            ..state.outbox[0].clone()
        };
        state.outbox.push(second);
        store.update(|| state.clone()).expect("publish");

        let restored = store.load();
        assert_eq!(restored, state);
        // The queue order *is* the delivery order, so it has to survive.
        assert_eq!(
            restored
                .outbox
                .iter()
                .map(|message| message.message_id)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
    }

    /// A second publish replaces the first, and the temporary file does not
    /// linger next to the queue.
    #[test]
    fn test_publishing_replaces_the_previous_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());

        store.update(sample).expect("first publish");
        let empty = ToxState {
            next_message_id: 9,
            ..ToxState::default()
        };
        store.update(|| empty.clone()).expect("second publish");

        assert_eq!(store.load(), empty);
        assert!(
            !store.path().with_extension("tmp").exists(),
            "the temporary file must be renamed, not left behind"
        );
    }

    /// A corrupt file is read as empty instead of stopping the transport, and
    /// the next publish replaces it.
    #[test]
    fn test_a_corrupt_file_is_read_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());
        std::fs::write(store.path(), b"{ this is not a queue").expect("write");

        assert!(store.load().is_empty());

        store.update(sample).expect("publish over a corrupt file");
        assert_eq!(store.load(), sample());
    }

    /// A layout this build does not know is refused rather than guessed at.
    #[test]
    fn test_an_unknown_layout_version_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());

        let mut future = sample();
        future.version = STORE_VERSION + 1;
        std::fs::write(store.path(), serde_json::to_vec(&future).expect("encode")).expect("write");

        let state = store.load();
        assert!(state.is_empty(), "a future layout must not be misread");
        assert_eq!(state.version, STORE_VERSION);

        // An older layout is refused for the same reason.
        let mut past = sample();
        past.version = 0;
        std::fs::write(store.path(), serde_json::to_vec(&past).expect("encode")).expect("write");
        assert!(store.load().is_empty());
    }

    /// What the transport advertises is what the file can hold.
    #[test]
    fn test_the_store_keeps_the_bound_the_transport_advertises() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());

        let mut state = sample();
        state.requests = (0..crate::transport::PENDING_REQUESTS_CAPACITY)
            .map(|index| StoredRequest {
                public_key: format!("{index:064X}"),
                message: "spam".to_string(),
            })
            .collect();
        store.update(|| state.clone()).expect("publish");

        let restored = store.load();
        assert_eq!(restored, state);
        assert_eq!(
            restored.requests.len(),
            crate::transport::PENDING_REQUESTS_CAPACITY
        );
    }

    /// The age of a queued payload is a wall clock, and a clock before the epoch
    /// does not panic or wrap around.
    #[test]
    fn test_unix_milliseconds_round_trip() {
        let time = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(unix_ms(time), 1_700_000_000_123);
        assert_eq!(from_unix_ms(1_700_000_000_123), time);

        assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert_eq!(from_unix_ms(0), UNIX_EPOCH);
    }
}
