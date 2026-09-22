/*!
 * tox.rs
 *
 * Self-contained Tox transport bindings over the system `libtoxcore`.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-12
 * Version: 0.4.0
 * License: MIT (the linked `libtoxcore` is GPL-3.0; see the note below)
 *
 * Features:
 * - Hand written FFI bindings to `libtoxcore` (no pure-Rust Tox crate)
 * - [`ToxClient`]: a threaded owner of one `Tox *` instance
 * - A blocking, testable command/event API decoupled from the UI and the actor
 *
 * # Why FFI instead of the `tox` crate
 *
 * The pure-Rust `tox`/`tox_core` crates are GPL-3.0+, pinned to tokio 0.2 and
 * expose only low level DHT/`net_crypto` primitives, with no client object. The
 * original C implementation (`metaText.c` in `metaText@0.4.0`) drives
 * `libtoxcore` directly, so this module mirrors exactly that: `tox_new`,
 * `tox_bootstrap`, `tox_friend_add`, `tox_friend_send_message` and the callback
 * set, reached through FFI.
 *
 * # Threading model
 *
 * `toxcore` is **not** thread safe: a single `Tox` instance must be driven by
 * one thread calling `tox_iterate` in a loop. [`ToxClient::start`] therefore
 * spawns a dedicated OS thread that owns the instance for its whole lifetime.
 * Callers talk to it through channels:
 *
 * ```text
 *   caller ──ToxCommand──▶ worker thread ──tox_*──▶ libtoxcore
 *   caller ◀──ToxEvent──── worker thread ◀─callbacks─┘
 * ```
 *
 * The handle methods that need a value back (add friend, send message, ...)
 * block on a per-command reply channel. That keeps this layer free of any
 * runtime dependency, so it can be unit tested on its own and later wrapped in
 * `spawn_blocking` by an async consumer.
 *
 * # Availability
 *
 * When `build.rs` cannot find `libtoxcore` the module still compiles, but
 * [`ToxClient::start`] fails with [`ToxError::Unavailable`] and
 * [`is_linked`] returns `false`. This keeps `--all-features` builds working on
 * machines without the C library.
 *
 * # Licensing
 *
 * Linking `libtoxcore` makes the resulting binary a combined work covered by
 * the GPL-3.0. Enable `tox-protocol` only if that is acceptable for the build
 * being produced.
 */
#![allow(unsafe_code)]

#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use std::sync::Mutex;
// The stub build (no `libtoxcore` found) never reaches the worker loop or the
// callbacks, so a handful of names in these imports are only used when the
// library is linked. Silencing *those* keeps `cargo clippy --all-features` on a
// machine without toxcore free of warnings without hiding a real one.
#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use tracing::warn;

/// The label rule this crate applies to every string a peer chose: see
/// [`meta_text_proto::ipc::validation::peer_label`].
#[cfg_attr(not(toxcore_found), allow(unused_imports))]
use meta_text_proto::ipc::validation;

/// Size of a Tox address in bytes.
///
/// The layout is a 32 byte public key, a 4 byte nospam and a 2 byte checksum,
/// rendered as 76 hexadecimal characters: exactly the format the original C
/// `metaText` prints (`TOX_ADDRESS_SIZE * 2 + 1`).
pub const ADDRESS_SIZE: usize = 38;

/// Size of a Tox public key in bytes (64 hexadecimal characters).
pub const PUBLIC_KEY_SIZE: usize = 32;

/// Maximum length of a Tox chat message, matching `TOX_MAX_MESSAGE_LENGTH`.
pub const MAX_MESSAGE_LENGTH: usize = 1372;

/// Maximum nickname length in bytes (`TOX_MAX_NAME_LENGTH`).
pub const MAX_NAME_LENGTH: usize = 128;

/// Maximum status message length in bytes (`TOX_MAX_STATUS_MESSAGE_LENGTH`).
pub const MAX_STATUS_LENGTH: usize = 1007;

/// Maximum friend request message length (`TOX_MAX_FRIEND_REQUEST_LENGTH`).
pub const MAX_FRIEND_REQUEST_LENGTH: usize = 1016;

/// Size of a conference identifier in bytes (`tox_conference_id_size`).
///
/// A conference is identified by this value rather than by its per-instance
/// `conference_number`, because the number may change when the instance is
/// recreated from savedata while the identifier does not.
pub const CONFERENCE_ID_SIZE: usize = 32;

/// Maximum conference title length in bytes (`TOX_MAX_NAME_LENGTH`).
pub const MAX_CONFERENCE_TITLE_LENGTH: usize = 128;

/// File name used to persist the Tox identity (public/private key, nospam,
/// friends and their state). Analogous to `savedata.meta` in the C version.
pub const SAVEDATA_FILE: &str = "tox-savedata.bin";

/// How many events may wait for the bridge before toxcore callbacks start
/// shedding them.
///
/// The callbacks run on the iteration thread and must not block, so the hand-off
/// to the bridge is a bounded `sync_channel` with `try_send`: an overloaded core
/// loses the newest event (counted in `ToxSender::dropped_events`) instead of
/// growing memory without limit. The value is generous enough that a normal
/// burst never loses anything.
pub const EVENT_QUEUE_CAPACITY: usize = 4096;

/// Errors produced by the Tox transport.
#[derive(Debug, thiserror::Error)]
pub enum ToxError {
    /// The crate was built without a usable `libtoxcore`.
    #[error(
        "this build has no usable libtoxcore; install it and rebuild with \
         `--features tox-protocol`"
    )]
    Unavailable,

    /// The worker thread has already exited.
    #[error("the Tox worker thread is no longer running")]
    WorkerStopped,

    /// A value that was expected to be hexadecimal was not.
    #[error("'{0}' is not a hexadecimal string")]
    NotHex(String),

    /// An address of the wrong size was supplied.
    #[error("a Tox address must be {ADDRESS_SIZE} bytes (76 hex characters); got {0} bytes")]
    BadAddressLength(usize),

    /// A public key of the wrong size was supplied.
    #[error("a Tox public key must be {PUBLIC_KEY_SIZE} bytes (64 hex characters); got {0} bytes")]
    BadKeyLength(usize),

    /// A configured value exceeds a toxcore limit.
    #[error("the {field} is {length} bytes, but toxcore allows at most {limit}")]
    TooLong {
        /// Which value was too long (`nickname`, `status message`, ...).
        field: &'static str,
        /// The supplied length in bytes.
        length: usize,
        /// toxcore's limit in bytes.
        limit: usize,
    },

    /// A message body was empty.
    #[error("a message body is required")]
    EmptyMessage,

    /// The configured UDP port range is inverted.
    #[error("start_port ({start}) is greater than end_port ({end})")]
    PortRange {
        /// First port of the configured range.
        start: u16,
        /// Last port of the configured range.
        end: u16,
    },

    /// The savedata file exists but toxcore refused to load it.
    ///
    /// The identity is **not** silently replaced: the file has to be repaired
    /// or moved aside before the transport can start again.
    #[error(
        "the Tox savedata at {path} could not be loaded (toxcore error {code}); \
         move it aside to start with a fresh identity"
    )]
    Savedata {
        /// The offending file.
        path: PathBuf,
        /// Raw `TOX_ERR_NEW` code reported by toxcore.
        code: i32,
    },

    /// `tox_new` failed.
    #[error("tox_new failed with error code {0}")]
    New(i32),

    /// `tox_friend_add` or `tox_friend_add_norequest` failed.
    #[error("tox_friend_add failed with error code {0} ({1})")]
    FriendAdd(i32, &'static str),

    /// `tox_friend_send_message` failed.
    #[error("tox_friend_send_message failed with error code {0} ({1})")]
    SendMessage(i32, &'static str),

    /// A friend query failed.
    #[error("a tox_friend query failed with error code {0}")]
    FriendQuery(i32),

    /// `tox_self_set_name` / `tox_self_set_status_message` failed.
    #[error("tox_self_set_* failed with error code {0}")]
    SetInfo(i32),

    /// `tox_bootstrap` failed.
    #[error("tox_bootstrap failed with error code {0}")]
    Bootstrap(i32),

    /// A `host:port:public_key` bootstrap entry could not be parsed.
    #[error(
        "'{0}' is not a valid Tox bootstrap node (expected host:port:PUBLIC_KEY, \
         where PUBLIC_KEY is 64 hexadecimal characters)"
    )]
    MalformedBootstrap(String),

    /// A peer identifier matched no known friend.
    #[error("'{0}' does not match a friend of this Tox instance")]
    UnknownFriend(String),

    /// A conference query or mutation failed.
    #[error("a tox_conference {operation} failed with error code {code} ({name})")]
    Conference {
        /// Which conference operation was attempted.
        operation: &'static str,
        /// Raw `TOX_ERR_CONFERENCE_*` code.
        code: i32,
        /// Human readable name for `code`.
        name: &'static str,
    },

    /// A group identifier matched no conference of this instance.
    #[error("'{0}' does not match a group of this Tox instance")]
    UnknownConference(String),

    /// A group identifier was not 32 hexadecimal bytes.
    #[error("a group id must be {CONFERENCE_ID_SIZE} bytes (64 hex characters); got {0} bytes")]
    BadConferenceIdLength(usize),

    /// Reading or writing the savedata file failed.
    #[error("tox savedata i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias for results from this module.
pub type ToxResult<T> = Result<T, ToxError>;

/// Whether this build actually linked `libtoxcore`.
///
/// `build.rs` sets the `toxcore_found` cfg after locating the library; when it
/// is missing the module compiles to a stub and this returns `false`.
#[must_use]
pub const fn is_linked() -> bool {
    cfg!(toxcore_found)
}

/// DHT bootstrap nodes carried over from the original `metaText@0.4.0`.
///
/// The C reference hard-codes this list in `metaText.h`; the Rust port keeps the
/// same entries so that both clients join the same DHT. Only nodes with a
/// literal IP address are listed, because [`BootstrapNode::new`] does not resolve
/// host names.
///
/// # Examples
///
/// ```rust
/// use meta_text_backend::tox::default_bootstrap_nodes;
///
/// let nodes = default_bootstrap_nodes();
/// assert!(nodes.len() >= 5);
/// assert!(nodes.iter().all(|node| node.port != 0));
/// ```
#[must_use]
pub fn default_bootstrap_nodes() -> Vec<BootstrapNode> {
    const RAW: [(&str, u16, &str); 8] = [
        (
            "144.217.167.73",
            33445,
            "7E5668E0EE09E19F320AD47902419331FFEE147BB3606769CFBE921A2A2FD34C",
        ),
        (
            "172.105.109.31",
            33445,
            "D46E97CF995DC1820B92B7D899E152A217D36ABE22730FEA4B6BF1BFC06C617C",
        ),
        (
            "128.199.199.197",
            33445,
            "B05C8869DBB4EDDD308F43C1A974A20A725A36EACCA123862FDE9945BF9D3E09",
        ),
        (
            "46.101.197.175",
            33445,
            "CD133B521159541FB1D326DE9850F5E56A6C724B5B8E5EB5CD8D950408E95707",
        ),
        (
            "195.201.7.101",
            33445,
            "B84E865125B4EC4C368CD047C72BCE447644A2DC31EF75BD2CDA345BFD310107",
        ),
        (
            "137.74.42.224",
            33445,
            "A95177FA018066CF044E811178D26B844CBF7E1E76F140095B3A1807E081A204",
        ),
        (
            "188.225.9.167",
            33445,
            "1911341A83E02503AB1FD6561BD64AF3A9D6C3F12B5FBB656976B2E678644A67",
        ),
        (
            "195.123.208.139",
            33445,
            "534A589BA7427C631773D13083570F529238211893640C99D1507300F055FE73",
        ),
    ];

    RAW.iter()
        .filter_map(|(host, port, key)| BootstrapNode::new(*host, *port, *key).ok())
        .collect()
}

/// Encode bytes as uppercase hexadecimal (the way Tox IDs are displayed).
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// Decode a hexadecimal string into bytes.
///
/// # Errors
///
/// Returns [`ToxError::NotHex`] when the input is not valid hexadecimal.
pub fn decode_hex(value: &str) -> ToxResult<Vec<u8>> {
    let trimmed = value.trim();
    if !trimmed.len().is_multiple_of(2) {
        return Err(ToxError::NotHex(value.to_string()));
    }
    (0..trimmed.len() / 2)
        .map(|i| {
            u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16)
                .map_err(|_| ToxError::NotHex(value.to_string()))
        })
        .collect()
}

/// One DHT bootstrap node, mirroring `struct DHT_node` in the C reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapNode {
    /// Host name or literal IP address.
    pub host: String,
    /// UDP/TCP port of the node.
    pub port: u16,
    /// The node's 32 byte public key, as 64 hexadecimal characters.
    pub public_key: String,
}

impl BootstrapNode {
    /// Create a bootstrap node entry.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`] or [`ToxError::BadAddressLength`] when
    /// `public_key` is not 32 bytes of hexadecimal.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        public_key: impl Into<String>,
    ) -> ToxResult<Self> {
        let public_key = public_key.into();
        let bytes = decode_hex(&public_key)?;
        if bytes.len() != PUBLIC_KEY_SIZE {
            return Err(ToxError::BadKeyLength(bytes.len()));
        }
        Ok(Self {
            host: host.into(),
            port,
            public_key,
        })
    }

    /// Parse a `host:port:PUBLIC_KEY` bootstrap entry.
    ///
    /// The C reference and the usual Tox clients accept this triple, so an
    /// operator can point the client at a private DHT node without rebuilding
    /// the binary. Host names and bracketed IPv6 literals are accepted;
    /// resolution happens inside toxcore.
    ///
    /// # Arguments
    ///
    /// * `value` - `host:port:public_key`, for example
    ///   `node.example.org:33445:7E5668E0…2A2FD34C`.
    ///
    /// # Returns
    ///
    /// Returns the parsed [`BootstrapNode`].
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::MalformedBootstrap`] when the shape is wrong, the
    /// port is not a non-zero `u16`, or the key is not 32 bytes of hexadecimal.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::tox::BootstrapNode;
    ///
    /// let key = "7E5668E0EE09E19F320AD47902419331FFEE147BB3606769CFBE921A2A2FD34C";
    /// let node = BootstrapNode::parse(&format!("144.217.167.73:33445:{key}")).expect("valid");
    /// assert_eq!(node.port, 33445);
    /// assert!(BootstrapNode::parse("144.217.167.73:33445").is_err());
    /// ```
    pub fn parse(value: &str) -> ToxResult<Self> {
        let trimmed = value.trim();
        let malformed = || ToxError::MalformedBootstrap(trimmed.to_string());

        // Split the key off the right so a bracketed IPv6 host can still use
        // colons internally.
        let (host_port, public_key) = trimmed.rsplit_once(':').ok_or_else(malformed)?;
        let (host, port) = split_host_port(host_port).ok_or_else(malformed)?;
        if host.is_empty() {
            return Err(malformed());
        }

        Self::new(host, port, public_key)
            .map_err(|_| ToxError::MalformedBootstrap(trimmed.to_string()))
    }
}

/// Split a `host:port` pair, accepting bracketed IPv6 literals.
///
/// Returns `None` when the port is missing, not numeric or zero.
fn split_host_port(value: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        // `[::1]:33445`
        let (inside, after) = rest.split_once(']')?;
        (inside.to_string(), after.strip_prefix(':')?)
    } else {
        let (host, port) = value.rsplit_once(':')?;
        (host.to_string(), port)
    };

    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host, port))
}

/// Everything the worker needs to come up.
#[derive(Debug, Clone)]
pub struct ToxConfig {
    /// Directory holding the savedata file.
    pub data_dir: PathBuf,
    /// Nickname announced to peers (via `tox_self_set_name`).
    pub nickname: String,
    /// Optional status message (via `tox_self_set_status_message`).
    pub status_message: String,
    /// DHT nodes dialled right after the instance is created.
    pub bootstrap_nodes: Vec<BootstrapNode>,
    /// First UDP port the instance may bind (`0` lets the OS choose).
    pub start_port: u16,
    /// Last UDP port the instance may bind (`0` lets the OS choose).
    pub end_port: u16,
    /// Whether UDP is enabled (TCP relaying works either way).
    pub udp_enabled: bool,
    /// Whether toxcore may use IPv6 (the DHT and direct peer connections).
    pub ipv6_enabled: bool,
}

impl ToxConfig {
    /// Configuration with sensible defaults for `data_dir` and `nickname`.
    ///
    /// Ports default to `0`, which lets the OS pick a free port, matching the
    /// C reference behaviour when no range is configured.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>, nickname: impl Into<String>) -> Self {
        Self {
            data_dir: data_dir.into(),
            nickname: nickname.into(),
            status_message: String::new(),
            bootstrap_nodes: Vec::new(),
            start_port: 0,
            end_port: 0,
            udp_enabled: true,
            ipv6_enabled: true,
        }
    }

    /// Where the identity is persisted (`<data_dir>/tox-savedata.bin`).
    #[must_use]
    pub fn savedata_path(&self) -> PathBuf {
        self.data_dir.join(SAVEDATA_FILE)
    }

    /// Check the values toxcore enforces before an instance is created.
    ///
    /// Catching these here turns a bare `TOX_ERR_NEW_PORT_ALLOC` (or a silently
    /// ignored `tox_self_set_name`) into an actionable error.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::PortRange`] for an inverted port range, or
    /// [`ToxError::TooLong`] when the nickname or status message exceeds the
    /// toxcore limit.
    pub const fn validate(&self) -> ToxResult<()> {
        // `end_port == 0` means "any port", which is always a valid range.
        if self.end_port != 0 && self.start_port > self.end_port {
            return Err(ToxError::PortRange {
                start: self.start_port,
                end: self.end_port,
            });
        }
        if self.nickname.len() > MAX_NAME_LENGTH {
            return Err(ToxError::TooLong {
                field: "nickname",
                length: self.nickname.len(),
                limit: MAX_NAME_LENGTH,
            });
        }
        if self.status_message.len() > MAX_STATUS_LENGTH {
            return Err(ToxError::TooLong {
                field: "status message",
                length: self.status_message.len(),
                limit: MAX_STATUS_LENGTH,
            });
        }
        Ok(())
    }
}

/// Tox connection state, matching `TOX_CONNECTION`.
///
/// The enum order matters: `NONE = 0`, `TCP = 1`, `UDP = 2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToxConnection {
    /// Not connected to the DHT.
    Offline,
    /// Connected over TCP (through a TCP relay).
    Tcp,
    /// Connected over UDP (direct).
    Udp,
}

impl ToxConnection {
    /// Map a raw `TOX_CONNECTION` value; unknown values become [`Self::Offline`].
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::Tcp,
            2 => Self::Udp,
            _ => Self::Offline,
        }
    }

    /// Lowercase name used in logs and messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Kind of a received Tox message (`TOX_MESSAGE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToxMessageKind {
    /// A normal chat message.
    Normal,
    /// An IRC-style `/me` action.
    Action,
}
impl ToxMessageKind {
    /// Map a raw `TOX_MESSAGE_TYPE` value; unknown values become [`Self::Normal`].
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        if value == 1 {
            Self::Action
        } else {
            Self::Normal
        }
    }

    /// The raw `TOX_MESSAGE_TYPE` value toxcore expects.
    #[must_use]
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Normal => 0,
            Self::Action => 1,
        }
    }
}

/// Kind of a conference (`TOX_CONFERENCE_TYPE`).
///
/// Only [`Self::Text`] conferences carry the `metaText` payload; a `ToxAV`
/// conference is reported so the front-end can say so instead of silently joining an
/// audio/video session it cannot render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToxConferenceType {
    /// A text conference.
    Text,
    /// An audio/video conference (handled by `toxav`, not by this client).
    Av,
}

impl ToxConferenceType {
    /// Map a raw `TOX_CONFERENCE_TYPE` value; unknown values become [`Self::Text`].
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        if value == 1 {
            Self::Av
        } else {
            Self::Text
        }
    }

    /// Lowercase name used in logs and messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Av => "audio/video",
        }
    }
}

/// A group chat (Tox conference) as this instance currently sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToxConference {
    /// The 64 character conference identifier, stable across restarts.
    pub id: String,
    /// The conference title (may be empty right after joining).
    pub title: String,
    /// How many peers are online in the conference.
    pub peers: usize,
    /// Whether the conference is reachable: its handshake finished, or it is one
    /// we created and somebody else is already in it.
    ///
    /// This is deliberately not a single toxcore flag. The `conference_connected`
    /// callback fires only for a conference joined with `tox_conference_join`, so a
    /// conference we *created* has to be judged by its peer count instead; see the
    /// documentation of `conference_info` for the full rule.
    pub connected: bool,
}

/// Something the transport reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToxEvent {
    /// Our own DHT connection state changed.
    SelfConnection {
        /// New state.
        status: ToxConnection,
    },
    /// A peer wants to become a friend.
    FriendRequest {
        /// The requester's public key (64 hexadecimal characters).
        public_key: String,
        /// The request message they attached.
        message: String,
    },
    /// A friend's connection state changed.
    FriendConnection {
        /// Friend number within this instance.
        friend_number: u32,
        /// New state.
        status: ToxConnection,
        /// The friend's announced nickname, if known.
        name: String,
    },
    /// A friend's announced nickname changed (or was learned for the first time).
    ///
    /// `friend_connection_status` is raised when the friend becomes reachable,
    /// which is *not* necessarily after their name has crossed the wire: toxcore
    /// raises `friend_name` for the name itself. Without this event a friend whose
    /// name arrived second stayed nameless forever — the friend table was only
    /// refreshed by the connection callback, so `/peers` reported the peer as
    /// still completing its handshake, inbound messages were attributed to a bare
    /// public key, and `/msg <nickname>` could not resolve the friend.
    FriendName {
        /// Friend number within this instance.
        friend_number: u32,
        /// The nickname the friend now has (empty when they cleared it).
        name: String,
    },
    /// A chat message arrived.
    Message {
        /// Friend number the message came from.
        friend_number: u32,
        /// Whether it is a normal message or an action.
        kind: ToxMessageKind,
        /// The message body, exactly as toxcore delivered it.
        ///
        /// Kept as raw bytes because metaText carries an encrypted envelope
        /// through this transport; a lossy UTF-8 conversion would corrupt it.
        /// Use [`ToxEvent::text`] for the human readable chat case.
        body: Vec<u8>,
    },
    /// A friend invited us to a conference.
    ConferenceInvite {
        /// The inviting friend's number.
        friend_number: u32,
        /// Whether it is a text or an audio/video conference.
        kind: ToxConferenceType,
        /// The opaque token needed to join, exactly as toxcore delivered it.
        ///
        /// The value is meaningless to metaText: it is handed back verbatim when
        /// the invitation is accepted.
        cookie: Vec<u8>,
    },
    /// The handshake with a joined conference completed.
    ///
    /// The conference is identified by its stable id, never by the per-instance
    /// conference number, which may change when the instance is rebuilt from
    /// savedata.
    ConferenceConnected {
        /// The 64 character conference identifier.
        conference_id: String,
        /// The title at the moment of the event (may be empty).
        title: String,
        /// How many peers are online.
        peers: usize,
    },
    /// A conference's peer list changed (someone joined or left).
    ConferencePeersChanged {
        /// The 64 character conference identifier.
        conference_id: String,
        /// The title at the moment of the event (may be empty).
        title: String,
        /// How many peers are online.
        peers: usize,
    },
    /// A conference was renamed.
    ///
    /// toxcore reports this through `conference_title` whenever a peer changes
    /// the title (and, with an unknown author, when the title is learned as part
    /// of joining). Surfacing it is what lets a front-end rename the group
    /// immediately instead of waiting for the next list: without this event a
    /// rename was only ever picked up by the next `/group list`.
    ConferenceTitleChanged {
        /// The 64 character conference identifier.
        conference_id: String,
        /// The title the conference now has.
        title: String,
        /// How many peers are online.
        peers: usize,
    },
    /// A message arrived in a conference.
    ConferenceMessage {
        /// The 64 character conference identifier.
        conference_id: String,
        /// Which peer in the conference sent it.
        peer_number: u32,
        /// The sender's announced name, resolved at delivery time (may be empty).
        peer_name: String,
        /// Whether it is a normal message or an action.
        kind: ToxMessageKind,
        /// The message body, exactly as toxcore delivered it.
        body: Vec<u8>,
    },
}

impl ToxEvent {
    /// Decode a message body as UTF-8 text.
    ///
    /// # Returns
    ///
    /// Returns `Some` for [`ToxEvent::Message`] and
    /// [`ToxEvent::ConferenceMessage`] events whose body is valid UTF-8, and
    /// `None` for every other event or an undecodable body. A body that is not
    /// valid UTF-8 is never replaced with U+FFFD: the caller decides how to
    /// report it.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::tox::{ToxEvent, ToxMessageKind};
    ///
    /// let event = ToxEvent::Message {
    ///     friend_number: 1,
    ///     kind: ToxMessageKind::Normal,
    ///     body: b"hi".to_vec(),
    /// };
    /// assert_eq!(event.text().as_deref(), Some("hi"));
    ///
    /// // A conference message decodes the same way.
    /// let group = ToxEvent::ConferenceMessage {
    ///     conference_id: "ab".repeat(32),
    ///     peer_number: 3,
    ///     peer_name: "Alice".to_string(),
    ///     kind: ToxMessageKind::Action,
    ///     body: b"waves".to_vec(),
    /// };
    /// assert_eq!(group.text().as_deref(), Some("waves"));
    /// ```
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Message { body, .. } | Self::ConferenceMessage { body, .. } => {
                String::from_utf8(body.clone()).ok()
            }
            _ => None,
        }
    }
}

/// A friend as reported by a [`ToxSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToxFriend {
    /// Friend number within this instance.
    pub number: u32,
    /// The friend's public key (64 hexadecimal characters).
    pub public_key: String,
    /// The friend's announced nickname (empty until announced).
    pub name: String,
    /// Current connection state.
    pub status: ToxConnection,
}

/// A point-in-time view of the instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToxSnapshot {
    /// Our own DHT connection state.
    pub self_connection: ToxConnection,
    /// The current friend list.
    pub friends: Vec<ToxFriend>,
    /// The conferences this instance is in.
    pub conferences: Vec<ToxConference>,
    /// Our own nickname.
    pub nickname: String,
}

/// The local identity, the equivalent of the "DID" the C version prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToxIdentity {
    /// The 76 character Tox address (public key + nospam + checksum).
    pub address: String,
    /// The 64 character public key.
    pub public_key: String,
    /// The nickname currently set on the instance.
    pub nickname: String,
}

impl ToxIdentity {
    /// Whether the address satisfies the Tox checksum rule: the XOR of all 38
    /// bytes is zero.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`] when `address` is not hexadecimal.
    pub fn checksum_is_valid(&self) -> ToxResult<bool> {
        let bytes = decode_hex(&self.address)?;
        Ok(bytes.iter().fold(0_u8, |acc, byte| acc ^ byte) == 0)
    }
}

/// Raw `libtoxcore` bindings.
///
/// Only compiled when `build.rs` located the library. The symbols resolve
/// against the `toxcore` library that the build script links; the block
/// deliberately carries no `#[link]` attribute because the flag is emitted by
/// `cargo::rustc-link-lib` instead.
#[cfg(toxcore_found)]
#[allow(non_camel_case_types)]
mod ffi {
    use std::os::raw::{c_char, c_void};

    /// Opaque `struct Tox_Options`.
    #[repr(C)]
    pub struct ToxOptions {
        _private: [u8; 0],
    }

    /// Opaque `Tox`.
    #[repr(C)]
    pub struct Tox {
        _private: [u8; 0],
    }

    /// `TOX_SAVEDATA_TYPE_TOX_SAVE`.
    pub const SAVEDATA_TYPE_TOX_SAVE: i32 = 1;

    // `TOX_MESSAGE_TYPE` values are mapped in [`ToxMessageKind`], which both the
    // FFI-enabled and the stub build can use.

    pub type SelfConnectionCb = extern "C" fn(*mut Tox, i32, *mut c_void);
    pub type FriendConnectionCb = extern "C" fn(*mut Tox, u32, i32, *mut c_void);
    /// `tox_friend_name_cb`: the friend's nickname, as a pointer plus a length.
    pub type FriendNameCb = extern "C" fn(*mut Tox, u32, *const u8, usize, *mut c_void);
    pub type FriendRequestCb = extern "C" fn(*mut Tox, *const u8, *const u8, usize, *mut c_void);
    pub type FriendMessageCb = extern "C" fn(*mut Tox, u32, i32, *const u8, usize, *mut c_void);
    pub type ConferenceInviteCb = extern "C" fn(*mut Tox, u32, i32, *const u8, usize, *mut c_void);
    pub type ConferenceConnectedCb = extern "C" fn(*mut Tox, u32, *mut c_void);
    pub type ConferenceMessageCb =
        extern "C" fn(*mut Tox, u32, u32, i32, *const u8, usize, *mut c_void);
    pub type ConferencePeersChangedCb = extern "C" fn(*mut Tox, u32, *mut c_void);
    /// `tox_conference_title_cb`: the author's `peer_number` is `UINT32_MAX`
    /// when toxcore does not know it (an initial join reports it that way).
    pub type ConferenceTitleCb = extern "C" fn(*mut Tox, u32, u32, *const u8, usize, *mut c_void);

    extern "C" {
        // --- options -------------------------------------------------------
        pub fn tox_options_new(error: *mut i32) -> *mut ToxOptions;
        pub fn tox_options_free(options: *mut ToxOptions);
        pub fn tox_options_set_start_port(options: *mut ToxOptions, port: u16);
        pub fn tox_options_set_end_port(options: *mut ToxOptions, port: u16);
        pub fn tox_options_set_udp_enabled(options: *mut ToxOptions, enabled: bool);
        pub fn tox_options_set_ipv6_enabled(options: *mut ToxOptions, enabled: bool);
        pub fn tox_options_set_savedata_type(options: *mut ToxOptions, kind: i32);
        pub fn tox_options_set_savedata_data(
            options: *mut ToxOptions,
            data: *const u8,
            length: usize,
        );

        // --- lifecycle -----------------------------------------------------
        pub fn tox_new(options: *const ToxOptions, error: *mut i32) -> *mut Tox;
        pub fn tox_kill(tox: *mut Tox);
        pub fn tox_iteration_interval(tox: *const Tox) -> u32;
        pub fn tox_iterate(tox: *mut Tox, user_data: *mut c_void);

        // --- sizes ---------------------------------------------------------
        pub fn tox_address_size() -> u32;
        pub fn tox_public_key_size() -> u32;

        // --- savedata ------------------------------------------------------
        pub fn tox_get_savedata_size(tox: *const Tox) -> usize;
        pub fn tox_get_savedata(tox: *const Tox, savedata: *mut u8);

        // --- self ----------------------------------------------------------
        pub fn tox_self_get_address(tox: *const Tox, address: *mut u8);
        pub fn tox_self_get_public_key(tox: *const Tox, public_key: *mut u8);
        pub fn tox_self_get_connection_status(tox: *const Tox) -> i32;
        pub fn tox_self_set_name(
            tox: *mut Tox,
            name: *const u8,
            length: usize,
            error: *mut i32,
        ) -> bool;
        pub fn tox_self_set_status_message(
            tox: *mut Tox,
            status: *const u8,
            length: usize,
            error: *mut i32,
        ) -> bool;
        pub fn tox_self_get_name_size(tox: *const Tox) -> usize;
        pub fn tox_self_get_name(tox: *const Tox, name: *mut u8);
        pub fn tox_self_get_friend_list_size(tox: *const Tox) -> usize;
        pub fn tox_self_get_friend_list(tox: *const Tox, list: *mut u32);

        // --- dht -----------------------------------------------------------
        pub fn tox_bootstrap(
            tox: *mut Tox,
            host: *const c_char,
            port: u16,
            public_key: *const u8,
            error: *mut i32,
        ) -> bool;

        // --- friends -------------------------------------------------------
        pub fn tox_friend_add(
            tox: *mut Tox,
            address: *const u8,
            message: *const u8,
            length: usize,
            error: *mut i32,
        ) -> u32;
        pub fn tox_friend_add_norequest(
            tox: *mut Tox,
            public_key: *const u8,
            error: *mut i32,
        ) -> u32;
        pub fn tox_friend_get_connection_status(
            tox: *const Tox,
            friend_number: u32,
            error: *mut i32,
        ) -> i32;
        pub fn tox_friend_get_public_key(
            tox: *const Tox,
            friend_number: u32,
            public_key: *mut u8,
            error: *mut i32,
        ) -> bool;
        pub fn tox_friend_get_name_size(
            tox: *const Tox,
            friend_number: u32,
            error: *mut i32,
        ) -> usize;
        pub fn tox_friend_get_name(
            tox: *const Tox,
            friend_number: u32,
            name: *mut u8,
            error: *mut i32,
        ) -> bool;
        pub fn tox_friend_send_message(
            tox: *mut Tox,
            friend_number: u32,
            kind: i32,
            message: *const u8,
            length: usize,
            error: *mut i32,
        ) -> u32;

        // --- callbacks -----------------------------------------------------
        pub fn tox_callback_self_connection_status(tox: *mut Tox, callback: SelfConnectionCb);
        pub fn tox_callback_friend_connection_status(tox: *mut Tox, callback: FriendConnectionCb);
        pub fn tox_callback_friend_name(tox: *mut Tox, callback: FriendNameCb);
        pub fn tox_callback_friend_request(tox: *mut Tox, callback: FriendRequestCb);
        pub fn tox_callback_friend_message(tox: *mut Tox, callback: FriendMessageCb);
        pub fn tox_callback_conference_invite(tox: *mut Tox, callback: ConferenceInviteCb);
        pub fn tox_callback_conference_connected(tox: *mut Tox, callback: ConferenceConnectedCb);
        pub fn tox_callback_conference_message(tox: *mut Tox, callback: ConferenceMessageCb);
        pub fn tox_callback_conference_peer_list_changed(
            tox: *mut Tox,
            callback: ConferencePeersChangedCb,
        );
        pub fn tox_callback_conference_title(tox: *mut Tox, callback: ConferenceTitleCb);

        // --- conferences ---------------------------------------------------
        pub fn tox_conference_id_size() -> u32;
        pub fn tox_conference_new(tox: *mut Tox, error: *mut i32) -> u32;
        pub fn tox_conference_delete(
            tox: *mut Tox,
            conference_number: u32,
            error: *mut i32,
        ) -> bool;
        pub fn tox_conference_get_id(tox: *const Tox, conference_number: u32, id: *mut u8) -> bool;
        pub fn tox_conference_get_chatlist_size(tox: *const Tox) -> usize;
        pub fn tox_conference_get_chatlist(tox: *const Tox, chatlist: *mut u32);
        pub fn tox_conference_invite(
            tox: *mut Tox,
            friend_number: u32,
            conference_number: u32,
            error: *mut i32,
        ) -> bool;
        pub fn tox_conference_join(
            tox: *mut Tox,
            friend_number: u32,
            cookie: *const u8,
            length: usize,
            error: *mut i32,
        ) -> u32;
        pub fn tox_conference_send_message(
            tox: *mut Tox,
            conference_number: u32,
            kind: i32,
            message: *const u8,
            length: usize,
            error: *mut i32,
        ) -> bool;
        pub fn tox_conference_peer_count(
            tox: *const Tox,
            conference_number: u32,
            error: *mut i32,
        ) -> u32;
        pub fn tox_conference_peer_get_name_size(
            tox: *const Tox,
            conference_number: u32,
            peer_number: u32,
            error: *mut i32,
        ) -> usize;
        pub fn tox_conference_peer_get_name(
            tox: *const Tox,
            conference_number: u32,
            peer_number: u32,
            name: *mut u8,
            error: *mut i32,
        ) -> bool;
        pub fn tox_conference_get_title_size(
            tox: *const Tox,
            conference_number: u32,
            error: *mut i32,
        ) -> usize;
        pub fn tox_conference_get_title(
            tox: *const Tox,
            conference_number: u32,
            title: *mut u8,
            error: *mut i32,
        ) -> bool;
        pub fn tox_conference_set_title(
            tox: *mut Tox,
            conference_number: u32,
            title: *const u8,
            length: usize,
            error: *mut i32,
        ) -> bool;
    }
}

/// Human readable name for a `TOX_ERR_FRIEND_ADD` code.
#[cfg(toxcore_found)]
const fn friend_add_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "NULL",
        2 => "TOO_LONG",
        3 => "NO_MESSAGE",
        4 => "OWN_KEY",
        5 => "ALREADY_SENT",
        6 => "BAD_CHECKSUM",
        7 => "SET_NEW_NOSPAM",
        8 => "MALLOC",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_FRIEND_SEND_MESSAGE` code.
#[cfg(toxcore_found)]
const fn send_message_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "NULL",
        2 => "FRIEND_NOT_FOUND",
        3 => "FRIEND_NOT_CONNECTED",
        4 => "SENDQ",
        5 => "TOO_LONG",
        6 => "EMPTY",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_NEW` code.
#[cfg(toxcore_found)]
const fn conference_new_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "INIT",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_DELETE` code.
#[cfg(toxcore_found)]
const fn conference_delete_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CONFERENCE_NOT_FOUND",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_JOIN` code.
#[cfg(toxcore_found)]
const fn conference_join_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "INVALID_LENGTH",
        2 => "WRONG_TYPE",
        3 => "FRIEND_NOT_FOUND",
        4 => "DUPLICATE",
        5 => "INIT_FAIL",
        6 => "FAIL_SEND",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_INVITE` code.
#[cfg(toxcore_found)]
const fn conference_invite_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CONFERENCE_NOT_FOUND",
        2 => "FAIL_SEND",
        3 => "NO_CONNECTION",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_SEND_MESSAGE` code.
///
/// The conference enums are distinct per function and their codes overlap with
/// *different* meanings (code `3` is `NO_CONNECTION` here and
/// `FRIEND_NOT_FOUND` when joining), which is why each operation has its own
/// table instead of one shared one.
#[cfg(toxcore_found)]
const fn conference_send_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CONFERENCE_NOT_FOUND",
        2 => "TOO_LONG",
        3 => "NO_CONNECTION",
        4 => "FAIL_SEND",
        _ => "UNKNOWN",
    }
}

/// Human readable name for a `TOX_ERR_CONFERENCE_TITLE` code.
#[cfg(toxcore_found)]
const fn conference_title_error_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CONFERENCE_NOT_FOUND",
        2 => "INVALID_LENGTH",
        _ => "UNKNOWN",
    }
}

/// A unit of work for the worker thread.
#[allow(dead_code)] // payloads are only read on the FFI-enabled build
enum ToxCommand {
    /// Change the announced nickname.
    SetName {
        /// New nickname.
        name: String,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Change the status message.
    SetStatusMessage {
        /// New status text.
        text: String,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Send a friend request (`tox_friend_add`).
    AddFriend {
        /// The decoded 38 byte Tox address.
        address: Vec<u8>,
        /// The request message.
        message: String,
        /// Where the new friend number is reported.
        reply: Sender<ToxResult<u32>>,
    },
    /// Accept an incoming request (`tox_friend_add_norequest`).
    AcceptFriend {
        /// The requester's decoded 32 byte public key.
        public_key: Vec<u8>,
        /// Where the new friend number is reported.
        reply: Sender<ToxResult<u32>>,
    },
    /// Send a chat message.
    SendMessage {
        /// Destination friend number.
        friend_number: u32,
        /// Message body; arbitrary bytes so an encrypted envelope survives.
        body: Vec<u8>,
        /// Whether toxcore should tag this as a normal message or an action.
        kind: ToxMessageKind,
        /// Where the message id is reported.
        reply: Sender<ToxResult<u32>>,
    },
    /// Read the current friend list and connection state.
    Snapshot {
        /// Where the snapshot is reported.
        reply: Sender<ToxResult<ToxSnapshot>>,
    },
    /// Create a text conference (`tox_conference_new`).
    ConferenceNew {
        /// The title to give it (may be empty).
        title: String,
        /// Where the new conference is reported.
        reply: Sender<ToxResult<ToxConference>>,
    },
    /// Join a conference we were invited to (`tox_conference_join`).
    ConferenceJoin {
        /// The inviting friend.
        friend_number: u32,
        /// The opaque token from the invitation.
        cookie: Vec<u8>,
        /// Where the joined conference is reported.
        reply: Sender<ToxResult<ToxConference>>,
    },
    /// Invite a friend to a conference (`tox_conference_invite`).
    ConferenceInvite {
        /// The conference to invite them to, by its stable id.
        conference_id: String,
        /// The friend to invite.
        friend_number: u32,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Rename a conference (`tox_conference_set_title`).
    ConferenceSetTitle {
        /// The conference to rename, by its stable id.
        conference_id: String,
        /// The new title.
        title: String,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Send a message to a conference (`tox_conference_send_message`).
    ConferenceSend {
        /// Destination conference, by its stable id.
        conference_id: String,
        /// Message body; arbitrary bytes so an encrypted envelope survives.
        body: Vec<u8>,
        /// Whether toxcore should tag this as a normal message or an action.
        kind: ToxMessageKind,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Leave and delete a conference (`tox_conference_delete`).
    ConferenceLeave {
        /// The conference to leave, by its stable id.
        conference_id: String,
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
    /// Persist the savedata file.
    Save {
        /// Where the written path is reported.
        reply: Sender<ToxResult<PathBuf>>,
    },
    /// Persist and stop the worker.
    Shutdown {
        /// Where the outcome is reported.
        reply: Sender<ToxResult<()>>,
    },
}

/// A running Tox instance driven by a dedicated worker thread.
///
/// The handle is cheap to move but must not be shared: `std::sync::mpsc`
/// receivers are single-consumer. Create one per consumer.
#[derive(Debug)]
#[cfg_attr(not(toxcore_found), allow(dead_code))]
pub struct ToxClient {
    /// Commands sent to the worker thread.
    commands: Sender<ToxCommand>,
    /// Events pushed by the worker's callbacks.
    events: Receiver<ToxEvent>,
    /// Local identity, captured at startup.
    identity: ToxIdentity,
    /// Where the identity is persisted.
    savedata_path: PathBuf,
    /// Events the callbacks had to shed because the hand-off queue was full.
    dropped: Arc<AtomicU64>,
    /// The worker thread, joined on shutdown.
    worker: Option<JoinHandle<()>>,
    /// Set once the worker has been asked to stop, so `Drop` does not repeat it.
    stopped: bool,
}

impl ToxClient {
    /// The local identity (address, public key, nickname).
    #[must_use]
    pub const fn identity(&self) -> &ToxIdentity {
        &self.identity
    }

    /// The 76 character Tox address, to hand to a peer.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.identity.address
    }

    /// Our own 64 character public key.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.identity.public_key
    }

    /// Where the identity is persisted.
    #[must_use]
    pub fn savedata_path(&self) -> &Path {
        &self.savedata_path
    }

    /// Take the next event without waiting.
    #[must_use]
    pub fn try_next_event(&self) -> Option<ToxEvent> {
        self.events.try_recv().ok()
    }

    /// Wait up to `timeout` for the next event.
    ///
    /// # Returns
    ///
    /// Returns `None` on timeout or when the worker has stopped.
    #[must_use]
    pub fn next_event(&self, timeout: Duration) -> Option<ToxEvent> {
        // A timeout and a closed channel both mean "nothing to report".
        self.events.recv_timeout(timeout).ok()
    }
}

/// A cloneable, thread-safe handle for issuing commands to a running instance.
///
/// [`ToxClient`] owns the event stream and therefore cannot be shared. A
/// `ToxSender` only owns the command side, so it is `Clone + Send + Sync` and can
/// be handed to an async task or a worker thread. Every method still blocks
/// until the Tox worker answers, so in an async context call it from
/// `tokio::task::spawn_blocking`.
#[derive(Debug, Clone)]
pub struct ToxSender {
    /// Commands sent to the worker thread.
    commands: Sender<ToxCommand>,

    /// Shared with the callbacks: how many events they had to shed.
    dropped: Arc<AtomicU64>,
}

impl ToxSender {
    /// Events the callbacks had to shed because the hand-off queue was full.
    ///
    /// Non-zero means the consumer could not keep up with the instance; see
    /// [`EVENT_QUEUE_CAPACITY`].
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

/// Send a command and block until the worker answers.
fn round_trip<T>(
    commands: &Sender<ToxCommand>,
    build: impl FnOnce(Sender<ToxResult<T>>) -> ToxCommand,
) -> ToxResult<T> {
    let (reply_tx, reply_rx) = mpsc::channel();
    commands
        .send(build(reply_tx))
        .map_err(|_| ToxError::WorkerStopped)?;
    reply_rx.recv().map_err(|_| ToxError::WorkerStopped)?
}

impl ToxSender {
    /// Send a command and block until the worker answers.
    fn request<T>(&self, build: impl FnOnce(Sender<ToxResult<T>>) -> ToxCommand) -> ToxResult<T> {
        round_trip(&self.commands, build)
    }

    /// Change the nickname announced to peers.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::SetInfo`] when toxcore rejects the value (for
    /// example because it is too long) or [`ToxError::WorkerStopped`].
    pub fn set_nickname(&self, name: &str) -> ToxResult<()> {
        self.request(|reply| ToxCommand::SetName {
            name: name.to_string(),
            reply,
        })
    }

    /// Change the personal status message.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::SetInfo`] or [`ToxError::WorkerStopped`].
    pub fn set_status_message(&self, text: &str) -> ToxResult<()> {
        self.request(|reply| ToxCommand::SetStatusMessage {
            text: text.to_string(),
            reply,
        })
    }

    /// Send a friend request to a 76 character Tox address.
    ///
    /// The address is validated (hexadecimal and exactly [`ADDRESS_SIZE`]
    /// bytes) before it reaches toxcore, which additionally verifies the
    /// checksum.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`], [`ToxError::BadAddressLength`],
    /// [`ToxError::EmptyMessage`], [`ToxError::TooLong`] or
    /// [`ToxError::FriendAdd`] with the raw `TOX_ERR_FRIEND_ADD` code.
    pub fn add_friend(&self, address: &str, message: &str) -> ToxResult<u32> {
        let bytes = decode_hex(address)?;
        if bytes.len() != ADDRESS_SIZE {
            return Err(ToxError::BadAddressLength(bytes.len()));
        }
        // toxcore rejects an empty request (`NO_MESSAGE`) and anything above
        // `TOX_MAX_FRIEND_REQUEST_LENGTH`; report both precisely.
        if message.is_empty() {
            return Err(ToxError::EmptyMessage);
        }
        if message.len() > MAX_FRIEND_REQUEST_LENGTH {
            return Err(ToxError::TooLong {
                field: "friend request",
                length: message.len(),
                limit: MAX_FRIEND_REQUEST_LENGTH,
            });
        }
        self.request(|reply| ToxCommand::AddFriend {
            address: bytes,
            message: message.to_string(),
            reply,
        })
    }

    /// Accept a friend request received through [`ToxEvent::FriendRequest`].
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`], [`ToxError::BadAddressLength`] or
    /// [`ToxError::FriendAdd`].
    pub fn accept_friend(&self, public_key: &str) -> ToxResult<u32> {
        let bytes = decode_hex(public_key)?;
        if bytes.len() != PUBLIC_KEY_SIZE {
            return Err(ToxError::BadKeyLength(bytes.len()));
        }
        self.request(|reply| ToxCommand::AcceptFriend {
            public_key: bytes,
            reply,
        })
    }

    /// Send a chat message to a friend.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::SendMessage`] when the friend is unknown or offline,
    /// or a validation error when `text` is empty or longer than
    /// [`MAX_MESSAGE_LENGTH`].
    pub fn send_message(&self, friend_number: u32, text: &str) -> ToxResult<u32> {
        if text.is_empty() {
            return Err(ToxError::EmptyMessage);
        }
        self.send_message_bytes(friend_number, text.as_bytes())
    }

    /// Send an arbitrary byte payload to a friend.
    ///
    /// Tox messages are opaque byte arrays at the C level, so an encrypted
    /// metaText envelope can travel through them unchanged. The toxcore limit
    /// ([`MAX_MESSAGE_LENGTH`]) is enforced here, before the payload reaches the
    /// worker thread, so an oversized envelope is reported as
    /// [`ToxError::TooLong`] instead of being silently split or rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::EmptyMessage`] for an empty payload,
    /// [`ToxError::TooLong`] when it exceeds [`MAX_MESSAGE_LENGTH`], or
    /// [`ToxError::SendMessage`] when toxcore refuses the send.
    pub fn send_message_bytes(&self, friend_number: u32, body: &[u8]) -> ToxResult<u32> {
        self.send_message_typed(friend_number, body, ToxMessageKind::Normal)
    }

    /// Send an arbitrary byte payload, tagged with its toxcore message type.
    ///
    /// The type is what makes `/me` render as an action on a Tox peer instead of
    /// as an ordinary message.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::send_message_bytes`].
    pub fn send_message_typed(
        &self,
        friend_number: u32,
        body: &[u8],
        kind: ToxMessageKind,
    ) -> ToxResult<u32> {
        if body.is_empty() {
            return Err(ToxError::EmptyMessage);
        }
        if body.len() > MAX_MESSAGE_LENGTH {
            return Err(ToxError::TooLong {
                field: "message",
                length: body.len(),
                limit: MAX_MESSAGE_LENGTH,
            });
        }
        self.request(|reply| ToxCommand::SendMessage {
            friend_number,
            body: body.to_vec(),
            kind,
            reply,
        })
    }

    /// Read the friend list and the current connection state.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::WorkerStopped`] when the worker has exited.
    pub fn snapshot(&self) -> ToxResult<ToxSnapshot> {
        self.request(|reply| ToxCommand::Snapshot { reply })
    }

    /// Create a text conference.
    ///
    /// The returned [`ToxConference`] is not connected yet, because nobody else is
    /// in it — a conference becomes connected once a peer is present (or, for a
    /// conference we joined, once its handshake finishes). Note that toxcore does
    /// **not** raise `conference_connected` for a conference we created.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::TooLong`] when the title exceeds
    /// [`MAX_CONFERENCE_TITLE_LENGTH`], [`ToxError::Conference`] with the raw
    /// `TOX_ERR_CONFERENCE_NEW` code, or [`ToxError::WorkerStopped`].
    pub fn conference_new(&self, title: &str) -> ToxResult<ToxConference> {
        if title.len() > MAX_CONFERENCE_TITLE_LENGTH {
            return Err(ToxError::TooLong {
                field: "conference title",
                length: title.len(),
                limit: MAX_CONFERENCE_TITLE_LENGTH,
            });
        }
        self.request(|reply| ToxCommand::ConferenceNew {
            title: title.to_string(),
            reply,
        })
    }

    /// Join a conference we were invited to, using the token from
    /// [`ToxEvent::ConferenceInvite`].
    ///
    /// Like [`ToxClient::conference_new`], the returned [`ToxConference`] is not
    /// connected yet: the call only starts the handshake, and
    /// [`ToxEvent::ConferenceConnected`] reports when it finished.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::Conference`] with the raw `TOX_ERR_CONFERENCE_JOIN`
    /// code (an invalid or expired token reports `INVALID_LENGTH` or
    /// `FRIEND_NOT_FOUND`), or [`ToxError::WorkerStopped`].
    pub fn conference_join(&self, friend_number: u32, cookie: &[u8]) -> ToxResult<ToxConference> {
        if cookie.is_empty() {
            return Err(ToxError::EmptyMessage);
        }
        self.request(|reply| ToxCommand::ConferenceJoin {
            friend_number,
            cookie: cookie.to_vec(),
            reply,
        })
    }

    /// Invite a friend to a conference, addressed by its stable id.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`], [`ToxError::BadConferenceIdLength`],
    /// [`ToxError::UnknownConference`], [`ToxError::Conference`] or
    /// [`ToxError::WorkerStopped`].
    pub fn conference_invite(&self, conference_id: &str, friend_number: u32) -> ToxResult<()> {
        self.request(|reply| ToxCommand::ConferenceInvite {
            conference_id: conference_id.to_string(),
            friend_number,
            reply,
        })
    }

    /// Rename a conference (`tox_conference_set_title`).
    ///
    /// The title is a conference-wide property: toxcore sends it to every peer in
    /// the conference, so the other side learns it through its `conference_title`
    /// callback ([`ToxEvent::ConferenceTitleChanged`]). Our own callback does
    /// **not** fire for our own change (measured: creating a conference with a
    /// title produces no events), which is why this call reports the title back
    /// through the reply rather than through the event stream.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::TooLong`] when the title exceeds
    /// [`MAX_CONFERENCE_TITLE_LENGTH`], [`ToxError::NotHex`],
    /// [`ToxError::BadConferenceIdLength`], [`ToxError::UnknownConference`],
    /// [`ToxError::Conference`] with the raw `TOX_ERR_CONFERENCE_TITLE` code, or
    /// [`ToxError::WorkerStopped`].
    pub fn conference_set_title(&self, conference_id: &str, title: &str) -> ToxResult<()> {
        if title.len() > MAX_CONFERENCE_TITLE_LENGTH {
            return Err(ToxError::TooLong {
                field: "conference title",
                length: title.len(),
                limit: MAX_CONFERENCE_TITLE_LENGTH,
            });
        }
        self.request(|reply| ToxCommand::ConferenceSetTitle {
            conference_id: conference_id.to_string(),
            title: title.to_string(),
            reply,
        })
    }

    /// Send a message to a conference.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::EmptyMessage`], [`ToxError::TooLong`],
    /// [`ToxError::NotHex`], [`ToxError::BadConferenceIdLength`],
    /// [`ToxError::UnknownConference`], [`ToxError::Conference`] with the raw
    /// `TOX_ERR_CONFERENCE_SEND_MESSAGE` code, or [`ToxError::WorkerStopped`].
    pub fn conference_send(
        &self,
        conference_id: &str,
        body: &[u8],
        kind: ToxMessageKind,
    ) -> ToxResult<()> {
        if body.is_empty() {
            return Err(ToxError::EmptyMessage);
        }
        if body.len() > MAX_MESSAGE_LENGTH {
            return Err(ToxError::TooLong {
                field: "conference message",
                length: body.len(),
                limit: MAX_MESSAGE_LENGTH,
            });
        }
        self.request(|reply| ToxCommand::ConferenceSend {
            conference_id: conference_id.to_string(),
            body: body.to_vec(),
            kind,
            reply,
        })
    }

    /// Leave a conference. toxcore then removes it from the savedata, so it is
    /// not rejoined on the next start.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::NotHex`], [`ToxError::BadConferenceIdLength`],
    /// [`ToxError::UnknownConference`], [`ToxError::Conference`] or
    /// [`ToxError::WorkerStopped`].
    pub fn conference_leave(&self, conference_id: &str) -> ToxResult<()> {
        self.request(|reply| ToxCommand::ConferenceLeave {
            conference_id: conference_id.to_string(),
            reply,
        })
    }

    /// Persist the identity, friend list and their state to disk.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::Io`] or [`ToxError::WorkerStopped`].
    pub fn save(&self) -> ToxResult<PathBuf> {
        self.request(|reply| ToxCommand::Save { reply })
    }

    /// Persist the identity and stop the worker.
    fn save_and_stop(&self) -> ToxResult<()> {
        self.request(|reply| ToxCommand::Shutdown { reply })
    }
}

impl ToxClient {
    /// A cloneable handle for issuing commands from another thread or task.
    #[must_use]
    pub fn sender(&self) -> ToxSender {
        ToxSender {
            commands: self.commands.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }

    /// Change the nickname announced to peers.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::set_nickname`].
    pub fn set_nickname(&self, name: &str) -> ToxResult<()> {
        self.sender().set_nickname(name)
    }

    /// Change the personal status message.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::set_status_message`].
    pub fn set_status_message(&self, text: &str) -> ToxResult<()> {
        self.sender().set_status_message(text)
    }

    /// Send a friend request to a 76 character Tox address.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::add_friend`].
    pub fn add_friend(&self, address: &str, message: &str) -> ToxResult<u32> {
        self.sender().add_friend(address, message)
    }

    /// Accept a friend request.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::accept_friend`].
    pub fn accept_friend(&self, public_key: &str) -> ToxResult<u32> {
        self.sender().accept_friend(public_key)
    }

    /// Send a chat message to a friend.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::send_message`].
    pub fn send_message(&self, friend_number: u32, text: &str) -> ToxResult<u32> {
        self.sender().send_message(friend_number, text)
    }

    /// Send an arbitrary byte payload to a friend.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::send_message_bytes`].
    pub fn send_message_bytes(&self, friend_number: u32, body: &[u8]) -> ToxResult<u32> {
        self.sender().send_message_bytes(friend_number, body)
    }

    /// Send an arbitrary byte payload, tagged with its toxcore message type.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::send_message_typed`].
    pub fn send_message_typed(
        &self,
        friend_number: u32,
        body: &[u8],
        kind: ToxMessageKind,
    ) -> ToxResult<u32> {
        self.sender().send_message_typed(friend_number, body, kind)
    }

    /// Read the friend list and the current connection state.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::snapshot`].
    pub fn snapshot(&self) -> ToxResult<ToxSnapshot> {
        self.sender().snapshot()
    }

    /// Create a text conference.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_new`].
    pub fn conference_new(&self, title: &str) -> ToxResult<ToxConference> {
        self.sender().conference_new(title)
    }

    /// Join a conference we were invited to.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_join`].
    pub fn conference_join(&self, friend_number: u32, cookie: &[u8]) -> ToxResult<ToxConference> {
        self.sender().conference_join(friend_number, cookie)
    }

    /// Invite a friend to a conference.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_invite`].
    pub fn conference_invite(&self, conference_id: &str, friend_number: u32) -> ToxResult<()> {
        self.sender()
            .conference_invite(conference_id, friend_number)
    }

    /// Rename a conference.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_set_title`].
    pub fn conference_set_title(&self, conference_id: &str, title: &str) -> ToxResult<()> {
        self.sender().conference_set_title(conference_id, title)
    }

    /// Send a message to a conference.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_send`].
    pub fn conference_send(
        &self,
        conference_id: &str,
        body: &[u8],
        kind: ToxMessageKind,
    ) -> ToxResult<()> {
        self.sender().conference_send(conference_id, body, kind)
    }

    /// Leave a conference.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::conference_leave`].
    pub fn conference_leave(&self, conference_id: &str) -> ToxResult<()> {
        self.sender().conference_leave(conference_id)
    }

    /// Persist the identity, friend list and their state to disk.
    ///
    /// # Errors
    ///
    /// See [`ToxSender::save`].
    pub fn save(&self) -> ToxResult<PathBuf> {
        self.sender().save()
    }

    /// Persist the identity and stop the worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::Io`] or [`ToxError::WorkerStopped`].
    pub fn shutdown(mut self) -> ToxResult<()> {
        let result = self.sender().save_and_stop();
        self.stopped = true;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        result
    }
}

impl Drop for ToxClient {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        // Best effort: ask the worker to persist and stop, then join. The reply
        // channel is dropped immediately because nobody is waiting here.
        let (reply, _) = mpsc::channel();
        let _ = self.commands.send(ToxCommand::Shutdown { reply });
        self.stopped = true;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Conference ids whose handshake has completed.
///
/// toxcore has no getter for "is this conference connected": the only signal is
/// the `conference_connected` callback, so it has to be remembered. Both the
/// writer (the callback, inside `tox_iterate`) and the reader (the snapshot
/// command) run on the worker thread, so the mutex is never contended; it exists
/// because the callback must reach state that outlives the `UserData` pointer.
#[cfg(toxcore_found)]
type ConnectedConferences = Arc<Mutex<HashSet<String>>>;

/// State handed to the toxcore callbacks through `tox_iterate`'s `user_data`.
#[cfg(toxcore_found)]
struct CallbackCtx {
    /// Sink for everything the callbacks observe.
    events: SyncSender<ToxEvent>,

    /// How many events the sink refused because it was full.
    dropped: Arc<AtomicU64>,

    /// Conferences whose `conference_connected` has fired (hex ids).
    connected: ConnectedConferences,
}

/// Hand one event to the bridge, counting a shed event instead of blocking.
///
/// A toxcore callback must return promptly, so a full queue loses the event
/// (the newest one) and the loss is visible in the metrics rather than being
/// hidden as an unbounded buffer.
#[cfg(toxcore_found)]
fn send_event(ctx: &CallbackCtx, event: ToxEvent) {
    match ctx.events.try_send(event) {
        Err(TrySendError::Full(_)) => {
            ctx.dropped.fetch_add(1, Ordering::SeqCst);
        }
        // A successful hand-off needs no bookkeeping, and a disconnected bridge
        // means the worker is shutting down, so there is nobody left to report
        // the loss to.
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
    }
}

/// `tox_self_connection_status` callback.
#[cfg(toxcore_found)]
extern "C" fn on_self_connection_status(
    _tox: *mut ffi::Tox,
    status: i32,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` is the `*mut CallbackCtx` the worker passes to
    // `tox_iterate`; it outlives every callback because the worker owns it.
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::SelfConnection {
            status: ToxConnection::from_raw(status),
        },
    );
}

/// `tox_friend_connection_status` callback.
#[cfg(toxcore_found)]
extern "C" fn on_friend_connection_status(
    tox: *mut ffi::Tox,
    friend_number: u32,
    status: i32,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see `on_self_connection_status`.
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::FriendConnection {
            friend_number,
            status: ToxConnection::from_raw(status),
            name: friend_name(tox, friend_number),
        },
    );
}

/// `tox_friend_name` callback.
///
/// The nickname is copied out here (toxcore only guarantees the buffer for the
/// duration of the callback) and the friend's current connection state is *not*
/// assumed: a name can also change while the friend is offline.
#[cfg(toxcore_found)]
extern "C" fn on_friend_name(
    _tox: *mut ffi::Tox,
    friend_number: u32,
    name: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    let body = if name.is_null() || length == 0 {
        &[][..]
    } else {
        // SAFETY: `name` is `length` readable bytes for this callback.
        unsafe { std::slice::from_raw_parts(name, length) }
    };
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::FriendName {
            friend_number,
            name: String::from_utf8_lossy(body).into_owned(),
        },
    );
}

/// `tox_friend_request` callback.
#[cfg(toxcore_found)]
extern "C" fn on_friend_request(
    _tox: *mut ffi::Tox,
    public_key: *const u8,
    message: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() || public_key.is_null() {
        return;
    }
    // SAFETY: toxcore guarantees `public_key` is 32 readable bytes and
    // `message` is `length` readable bytes for the duration of the callback.
    let key = unsafe { std::slice::from_raw_parts(public_key, PUBLIC_KEY_SIZE) };
    let body = if message.is_null() || length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(message, length) }
    };
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::FriendRequest {
            public_key: encode_hex(key),
            message: String::from_utf8_lossy(body).into_owned(),
        },
    );
}

/// `tox_friend_message` callback.
#[cfg(toxcore_found)]
extern "C" fn on_friend_message(
    _tox: *mut ffi::Tox,
    friend_number: u32,
    kind: i32,
    message: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    let body = if message.is_null() || length == 0 {
        &[][..]
    } else {
        // SAFETY: `message` is `length` readable bytes for this callback.
        unsafe { std::slice::from_raw_parts(message, length) }
    };
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::Message {
            friend_number,
            kind: ToxMessageKind::from_raw(kind),
            body: body.to_vec(),
        },
    );
}

/// `tox_conference_invite` callback.
#[cfg(toxcore_found)]
extern "C" fn on_conference_invite(
    _tox: *mut ffi::Tox,
    friend_number: u32,
    kind: i32,
    cookie: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: toxcore guarantees `cookie` is `length` readable bytes for the
    // duration of the callback, and copies are made before returning.
    let cookie = if cookie.is_null() || length == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(cookie, length) }.to_vec()
    };
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::ConferenceInvite {
            friend_number,
            kind: ToxConferenceType::from_raw(kind),
            cookie,
        },
    );
}

/// `tox_conference_connected` callback.
#[cfg(toxcore_found)]
extern "C" fn on_conference_connected(
    tox: *mut ffi::Tox,
    conference_number: u32,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    // The conference is described from the callback itself: the id is stable and
    // the number is not, so only the id is allowed to leave this module.
    let conference_id = conference_id(tox, conference_number);
    // Remember that the handshake finished: toxcore cannot be asked, and a
    // snapshot has to answer `connected` from something.
    if let Ok(mut connected) = ctx.connected.lock() {
        connected.insert(conference_id.clone());
    }
    send_event(
        ctx,
        ToxEvent::ConferenceConnected {
            conference_id,
            title: conference_title(tox, conference_number),
            peers: conference_peer_count(tox, conference_number),
        },
    );
}

/// `tox_conference_peer_list_changed` callback.
#[cfg(toxcore_found)]
extern "C" fn on_conference_peers_changed(
    tox: *mut ffi::Tox,
    conference_number: u32,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::ConferencePeersChanged {
            conference_id: conference_id(tox, conference_number),
            title: conference_title(tox, conference_number),
            peers: conference_peer_count(tox, conference_number),
        },
    );
}

/// `tox_conference_title` callback.
///
/// The author's `peer_number` is deliberately discarded: toxcore passes
/// `UINT32_MAX` when it does not know who renamed the conference (the initial
/// title arrives that way), so a front-end cannot rely on it. The new title is
/// what it can act on.
#[cfg(toxcore_found)]
extern "C" fn on_conference_title(
    tox: *mut ffi::Tox,
    conference_number: u32,
    _peer_number: u32,
    title: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `title` is `length` readable bytes for the duration of this
    // callback, and the string is copied before returning.
    let title = if title.is_null() || length == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(title, length) }).into_owned()
    };
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::ConferenceTitleChanged {
            conference_id: conference_id(tox, conference_number),
            // The callback payload is the authoritative new title; the getter is
            // only a fallback for a library that reports an empty payload.
            title: if title.is_empty() {
                conference_title(tox, conference_number)
            } else {
                title
            },
            peers: conference_peer_count(tox, conference_number),
        },
    );
}

/// `tox_conference_message` callback.
#[cfg(toxcore_found)]
extern "C" fn on_conference_message(
    tox: *mut ffi::Tox,
    conference_number: u32,
    peer_number: u32,
    kind: i32,
    message: *const u8,
    length: usize,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `message` is `length` readable bytes for this callback; the body is
    // copied below, so nothing outlives the call.
    let body = if message.is_null() || length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(message, length) }
    };
    // SAFETY: the pointer is non-null (checked above) and is the `*mut CallbackCtx`
    // the worker owns for as long as `tox_iterate` can run (see the canonical
    // argument in `on_self_connection_status`).
    let ctx = unsafe { &*user_data.cast::<CallbackCtx>() };
    send_event(
        ctx,
        ToxEvent::ConferenceMessage {
            conference_id: conference_id(tox, conference_number),
            peer_number,
            // Resolved here so the bridge does not have to ask toxcore again and
            // the message carries the name it had when it was sent.
            peer_name: conference_peer_name(tox, conference_number, peer_number),
            kind: ToxMessageKind::from_raw(kind),
            body: body.to_vec(),
        },
    );
}

/// Read a friend's announced nickname (empty when unknown).
#[cfg(toxcore_found)]
fn friend_name(tox: *const ffi::Tox, friend_number: u32) -> String {
    let mut err = -1;
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe { ffi::tox_friend_get_name_size(tox, friend_number, &raw mut err) };
    if err != 0 || size == 0 || size > 128 {
        return String::new();
    }
    let mut buffer = vec![0_u8; size];
    let mut get_err = -1;
    // SAFETY: `buffer` has room for exactly `size` bytes.
    let ok = unsafe {
        ffi::tox_friend_get_name(tox, friend_number, buffer.as_mut_ptr(), &raw mut get_err)
    };
    if ok {
        // A name is chosen by the *friend* and printed by a front-end, so it is held to
        // the label rule (`validation::nickname`): an unusable name is reported as no
        // name — the state a friend that has not announced one is already in — instead of
        // being repaired, because a sanitised name would display something the peer did
        // not send. The friend's connection, its table entry and the counters are
        // untouched: only the label is refused.
        validation::peer_label(&String::from_utf8_lossy(&buffer)).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Read a friend's public key (hex, empty on failure).
#[cfg(toxcore_found)]
fn friend_public_key(tox: *const ffi::Tox, friend_number: u32) -> String {
    let mut buffer = [0_u8; PUBLIC_KEY_SIZE];
    let mut err = -1;
    // SAFETY: `buffer` is exactly `PUBLIC_KEY_SIZE` bytes.
    let ok = unsafe {
        ffi::tox_friend_get_public_key(tox, friend_number, buffer.as_mut_ptr(), &raw mut err)
    };
    if ok {
        encode_hex(&buffer)
    } else {
        String::new()
    }
}

/// Read our own announced nickname (empty when unset).
#[cfg(toxcore_found)]
fn self_name(tox: *const ffi::Tox) -> String {
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe { ffi::tox_self_get_name_size(tox) };
    if size == 0 || size > 128 {
        return String::new();
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: `buffer` has room for exactly `size` bytes.
    unsafe { ffi::tox_self_get_name(tox, buffer.as_mut_ptr()) };
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Read a conference's title (empty when unknown).
#[cfg(toxcore_found)]
fn conference_title(tox: *const ffi::Tox, conference_number: u32) -> String {
    let mut err = -1;
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe { ffi::tox_conference_get_title_size(tox, conference_number, &raw mut err) };
    if err != 0 || size == 0 || size > MAX_CONFERENCE_TITLE_LENGTH {
        return String::new();
    }
    let mut buffer = vec![0_u8; size];
    let mut get_err = -1;
    // SAFETY: `buffer` has room for exactly `size` bytes.
    let ok = unsafe {
        ffi::tox_conference_get_title(
            tox,
            conference_number,
            buffer.as_mut_ptr(),
            &raw mut get_err,
        )
    };
    if ok {
        // A title is chosen by whoever renamed the conference — a peer — and a front-end
        // prints it as the group's name, so it goes through the same label rule as a
        // nickname.
        validation::peer_label(&String::from_utf8_lossy(&buffer)).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Read the number of online peers in a conference (0 when unknown).
#[cfg(toxcore_found)]
fn conference_peer_count(tox: *const ffi::Tox, conference_number: u32) -> usize {
    let mut err = -1;
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let count = unsafe { ffi::tox_conference_peer_count(tox, conference_number, &raw mut err) };
    if err == 0 {
        count as usize
    } else {
        0
    }
}

/// Read a conference peer's name (empty when unknown).
#[cfg(toxcore_found)]
fn conference_peer_name(tox: *const ffi::Tox, conference_number: u32, peer_number: u32) -> String {
    let mut err = -1;
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe {
        ffi::tox_conference_peer_get_name_size(tox, conference_number, peer_number, &raw mut err)
    };
    if err != 0 || size == 0 || size > MAX_NAME_LENGTH {
        return String::new();
    }
    let mut buffer = vec![0_u8; size];
    let mut get_err = -1;
    // SAFETY: `buffer` has room for exactly `size` bytes.
    let ok = unsafe {
        ffi::tox_conference_peer_get_name(
            tox,
            conference_number,
            peer_number,
            buffer.as_mut_ptr(),
            &raw mut get_err,
        )
    };
    if ok {
        // A participant names itself, and that name is attributed to its messages in the
        // group pane: the same label rule as everywhere else a peer picks a string.
        validation::peer_label(&String::from_utf8_lossy(&buffer)).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Read a conference's stable identifier (hex, empty on failure).
///
/// The identifier is what the core knows a group by: a `conference_number` is
/// only meaningful inside one instance and can change when the instance is
/// rebuilt from savedata. The buffer is sized by toxcore itself (rather than by
/// [`CONFERENCE_ID_SIZE`], which is the value used to validate a *typed*
/// identifier), so a library that reports a different width still produces a
/// usable id.
#[cfg(toxcore_found)]
fn conference_id(tox: *const ffi::Tox, conference_number: u32) -> String {
    // SAFETY: worker thread, live instance.
    let size = unsafe { ffi::tox_conference_id_size() } as usize;
    if size == 0 {
        return String::new();
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: `buffer` has room for exactly `size` bytes, which is what toxcore
    // just reported.
    let ok = unsafe { ffi::tox_conference_get_id(tox, conference_number, buffer.as_mut_ptr()) };
    if ok {
        encode_hex(&buffer)
    } else {
        String::new()
    }
}

/// Describe one conference, deriving `connected` from the handshake and peers.
///
/// `handshake_finished` is whether `conference_connected` has fired for this
/// conference. That callback is **not** the whole story: tox.h documents it as
/// "triggered when the client successfully connects to a conference after joining
/// it with `tox_conference_join`", so it never fires for a conference this
/// instance *created*. Measured against toxcore 0.2.18: the side that joins gets
/// the callback, the side that created the conference does not, even once both
/// report each other as peers. So:
///
/// - a joined conference is connected once its handshake finished;
/// - a created conference is connected once somebody else is in it, which the
///   peer count already shows (the count includes ourselves);
/// - a conference with nobody else in it is not connected, which is exactly the
///   case where a send fails with `NO_CONNECTION`.
#[cfg(toxcore_found)]
fn conference_info(
    tox: *const ffi::Tox,
    conference_number: u32,
    handshake_finished: bool,
) -> ToxConference {
    let peers = conference_peer_count(tox, conference_number);
    ToxConference {
        id: conference_id(tox, conference_number),
        title: conference_title(tox, conference_number),
        connected: handshake_finished || peers > 1,
        peers,
    }
}

/// Set a conference's title, mapping the raw `TOX_ERR_CONFERENCE_TITLE` code.
#[cfg(toxcore_found)]
fn set_conference_title(tox: *mut ffi::Tox, number: u32, title: &str) -> ToxResult<()> {
    let mut error = -1;
    // SAFETY: worker thread, live instance; `title` outlives the call.
    let ok = unsafe {
        ffi::tox_conference_set_title(tox, number, title.as_ptr(), title.len(), &raw mut error)
    };
    if ok {
        Ok(())
    } else {
        Err(ToxError::Conference {
            operation: "set the title of",
            code: error,
            name: conference_title_error_name(error),
        })
    }
}

/// Resolve a stable conference id to the number toxcore currently uses.
///
/// The comparison is done on decoded bytes, so an uppercase id resolves just as
/// well as the lowercase form this module produces.
#[cfg(toxcore_found)]
fn conference_number_for(tox: *const ffi::Tox, group_id: &str) -> ToxResult<u32> {
    let wanted = decode_hex(group_id)?;
    if wanted.len() != CONFERENCE_ID_SIZE {
        return Err(ToxError::BadConferenceIdLength(wanted.len()));
    }
    let current = conference_numbers(tox);
    current
        .into_iter()
        .find(|number| {
            decode_hex(&conference_id(tox, *number)).is_ok_and(|candidate| candidate == wanted)
        })
        .ok_or_else(|| ToxError::UnknownConference(group_id.to_string()))
}

/// List the conference numbers this instance knows about.
#[cfg(toxcore_found)]
fn conference_numbers(tox: *const ffi::Tox) -> Vec<u32> {
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe { ffi::tox_conference_get_chatlist_size(tox) };
    if size == 0 {
        return Vec::new();
    }
    let mut list = vec![0_u32; size];
    // SAFETY: `list` has room for exactly `size` entries, which is what toxcore
    // just reported.
    unsafe { ffi::tox_conference_get_chatlist(tox, list.as_mut_ptr()) };
    list
}

/// Write the savedata next to the config's data directory (atomic replace).
/// Write the savedata next to the config's data directory (atomic replace).
#[cfg(toxcore_found)]
fn save_savedata(tox: *const ffi::Tox, path: &Path) -> ToxResult<PathBuf> {
    // SAFETY: `tox` is a live instance owned by the worker thread.
    let size = unsafe { ffi::tox_get_savedata_size(tox) };
    let mut data = vec![0_u8; size];
    // SAFETY: `data` has room for exactly `size` bytes.
    unsafe { ffi::tox_get_savedata(tox, data.as_mut_ptr()) };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, &data)?;
    std::fs::rename(&temporary, path)?;
    Ok(path.to_path_buf())
}

/// Create the `Tox_Options` and the instance itself.
///
/// # Safety
///
/// The returned pointer must only be used from a single thread and must be
/// released with `tox_kill`.
#[cfg(toxcore_found)]
unsafe fn create_tox(config: &ToxConfig, savedata: Option<&[u8]>) -> ToxResult<*mut ffi::Tox> {
    let options = ffi::tox_options_new(std::ptr::null_mut());
    if options.is_null() {
        return Err(ToxError::New(1));
    }
    ffi::tox_options_set_start_port(options, config.start_port);
    ffi::tox_options_set_end_port(options, config.end_port);
    ffi::tox_options_set_udp_enabled(options, config.udp_enabled);
    ffi::tox_options_set_ipv6_enabled(options, config.ipv6_enabled);
    if let Some(data) = savedata {
        ffi::tox_options_set_savedata_type(options, ffi::SAVEDATA_TYPE_TOX_SAVE);
        // toxcore copies the savedata during `tox_new`, so `data` only has to
        // live until the call below.
        ffi::tox_options_set_savedata_data(options, data.as_ptr(), data.len());
    }

    let mut error = -1;
    let tox = ffi::tox_new(options, &raw mut error);
    ffi::tox_options_free(options);
    if tox.is_null() {
        return Err(ToxError::New(error));
    }
    Ok(tox)
}

/// Apply the configured name/status and read back the local identity.
///
/// # Safety
///
/// `tox` must be a live instance.
#[cfg(toxcore_found)]
unsafe fn initialize_identity(tox: *mut ffi::Tox, config: &ToxConfig) -> ToxIdentity {
    if !config.nickname.is_empty() {
        let mut error = -1;
        let _ = ffi::tox_self_set_name(
            tox,
            config.nickname.as_ptr(),
            config.nickname.len(),
            &raw mut error,
        );
    }
    if !config.status_message.is_empty() {
        let mut error = -1;
        let _ = ffi::tox_self_set_status_message(
            tox,
            config.status_message.as_ptr(),
            config.status_message.len(),
            &raw mut error,
        );
    }

    let mut address = vec![0_u8; ffi::tox_address_size() as usize];
    ffi::tox_self_get_address(tox, address.as_mut_ptr());
    let mut public_key = vec![0_u8; ffi::tox_public_key_size() as usize];
    ffi::tox_self_get_public_key(tox, public_key.as_mut_ptr());

    ToxIdentity {
        address: encode_hex(&address),
        public_key: encode_hex(&public_key),
        nickname: config.nickname.clone(),
    }
}

/// Dial every configured DHT node.
///
/// # Safety
///
/// `tox` must be a live instance.
#[cfg(toxcore_found)]
unsafe fn bootstrap(tox: *mut ffi::Tox, nodes: &[BootstrapNode]) {
    for node in nodes {
        let Ok(key) = decode_hex(&node.public_key) else {
            continue;
        };
        let Ok(host) = std::ffi::CString::new(node.host.as_str()) else {
            continue;
        };
        let mut error = -1;
        let _ = ffi::tox_bootstrap(tox, host.as_ptr(), node.port, key.as_ptr(), &raw mut error);
    }
}

/// Owner of one `Tox *` inside its worker thread.
///
/// The raw pointer never leaves the thread: `run_worker` creates the instance
/// and drops it in the same closure, so no `Send` contract has to be invented.
#[cfg(toxcore_found)]
struct Worker {
    /// The instance.
    tox: *mut ffi::Tox,
    /// Callback state; its address is passed to `tox_iterate`.
    ctx: Box<CallbackCtx>,
    /// Where the identity is persisted.
    savedata_path: PathBuf,
}

#[cfg(toxcore_found)]
impl Worker {
    /// Drive one `tox_iterate` pass, which fires any pending callbacks.
    fn iterate(&self) {
        // SAFETY: `self.ctx` is owned by this struct and lives for the whole
        // worker, so the pointer stays valid across `tox_iterate`.
        let user_data = std::ptr::from_ref(&*self.ctx).cast_mut().cast();
        // SAFETY: called from the worker thread only, with a live instance.
        unsafe { ffi::tox_iterate(self.tox, user_data) };
    }

    /// How long toxcore wants us to sleep before the next iteration.
    fn interval(&self) -> Duration {
        // SAFETY: called from the worker thread only, with a live instance.
        let millis = unsafe { ffi::tox_iteration_interval(self.tox) }.clamp(20, 1000);
        Duration::from_millis(u64::from(millis))
    }
}

/// Turn a `tox_friend_add*` outcome into a result.
#[cfg(toxcore_found)]
const fn friend_add_result(number: u32, error: i32) -> ToxResult<u32> {
    if error == 0 {
        Ok(number)
    } else {
        Err(ToxError::FriendAdd(error, friend_add_error_name(error)))
    }
}

#[cfg(toxcore_found)]
impl Worker {
    /// Execute one command.
    ///
    /// # Returns
    ///
    /// Returns `true` when the worker must stop.
    ///
    /// One arm per command rather than a table of handlers: each arm is a direct
    /// FFI call with its own error mapping, and a test asserts the `bool` contract
    /// (`false` for work, `true` for `Shutdown`).
    #[allow(clippy::too_many_lines)]
    fn handle(&self, command: ToxCommand) -> bool {
        match command {
            ToxCommand::SetName { name, reply } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; `name` outlives the call.
                let ok = unsafe {
                    ffi::tox_self_set_name(self.tox, name.as_ptr(), name.len(), &raw mut error)
                };
                let _ = reply.send(if ok {
                    Ok(())
                } else {
                    Err(ToxError::SetInfo(error))
                });
                false
            }
            ToxCommand::SetStatusMessage { text, reply } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; `text` outlives the call.
                let ok = unsafe {
                    ffi::tox_self_set_status_message(
                        self.tox,
                        text.as_ptr(),
                        text.len(),
                        &raw mut error,
                    )
                };
                let _ = reply.send(if ok {
                    Ok(())
                } else {
                    Err(ToxError::SetInfo(error))
                });
                false
            }
            ToxCommand::AddFriend {
                address,
                message,
                reply,
            } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; both slices outlive the call.
                let number = unsafe {
                    ffi::tox_friend_add(
                        self.tox,
                        address.as_ptr(),
                        message.as_ptr(),
                        message.len(),
                        &raw mut error,
                    )
                };
                let _ = reply.send(friend_add_result(number, error));
                false
            }
            ToxCommand::AcceptFriend { public_key, reply } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; the slice outlives the call.
                let number = unsafe {
                    ffi::tox_friend_add_norequest(self.tox, public_key.as_ptr(), &raw mut error)
                };
                let _ = reply.send(friend_add_result(number, error));
                false
            }
            ToxCommand::SendMessage {
                friend_number,
                body,
                kind,
                reply,
            } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; `body` outlives the call.
                let id = unsafe {
                    ffi::tox_friend_send_message(
                        self.tox,
                        friend_number,
                        kind.as_raw(),
                        body.as_ptr(),
                        body.len(),
                        &raw mut error,
                    )
                };
                let outcome = if error == 0 {
                    Ok(id)
                } else {
                    Err(ToxError::SendMessage(error, send_message_error_name(error)))
                };
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::Snapshot { reply } => {
                // Reading state cannot fail, but the reply type is uniform.
                let _ = reply.send(Ok(self.snapshot()));
                false
            }
            ToxCommand::ConferenceNew { title, reply } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance.
                let number = unsafe { ffi::tox_conference_new(self.tox, &raw mut error) };
                let outcome = if error == 0 {
                    // A title is optional, and a failure to set it is not a failure
                    // to create the group: the group is reported with whatever title
                    // toxcore accepted, and the reason is logged rather than hidden.
                    if !title.is_empty() {
                        if let Err(title_error) = set_conference_title(self.tox, number, &title) {
                            warn!("⚠️ Could not title the new conference: {title_error}");
                        }
                    }
                    // The conference starts unreachable: nobody else is in it yet.
                    Ok(conference_info(self.tox, number, false))
                } else {
                    Err(ToxError::Conference {
                        operation: "new",
                        code: error,
                        name: conference_new_error_name(error),
                    })
                };
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::ConferenceJoin {
                friend_number,
                cookie,
                reply,
            } => {
                let mut error = -1;
                // SAFETY: worker thread, live instance; `cookie` outlives the call.
                let number = unsafe {
                    ffi::tox_conference_join(
                        self.tox,
                        friend_number,
                        cookie.as_ptr(),
                        cookie.len(),
                        &raw mut error,
                    )
                };
                // The join only *starts* the handshake; `conference_connected` is
                // what reports it as done, so the immediate reply is not connected.
                let outcome = if error == 0 {
                    Ok(conference_info(self.tox, number, false))
                } else {
                    Err(ToxError::Conference {
                        operation: "join",
                        code: error,
                        name: conference_join_error_name(error),
                    })
                };
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::ConferenceInvite {
                conference_id,
                friend_number,
                reply,
            } => {
                let outcome = conference_number_for(self.tox, &conference_id).and_then(|number| {
                    let mut error = -1;
                    // SAFETY: worker thread, live instance.
                    let ok = unsafe {
                        ffi::tox_conference_invite(self.tox, friend_number, number, &raw mut error)
                    };
                    if ok {
                        Ok(())
                    } else {
                        Err(ToxError::Conference {
                            operation: "invite",
                            code: error,
                            name: conference_invite_error_name(error),
                        })
                    }
                });
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::ConferenceSetTitle {
                conference_id,
                title,
                reply,
            } => {
                let outcome = conference_number_for(self.tox, &conference_id)
                    .and_then(|number| set_conference_title(self.tox, number, &title));
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::ConferenceSend {
                conference_id,
                body,
                kind,
                reply,
            } => {
                let outcome = conference_number_for(self.tox, &conference_id).and_then(|number| {
                    let mut error = -1;
                    // SAFETY: worker thread, live instance; `body` outlives the call.
                    let ok = unsafe {
                        ffi::tox_conference_send_message(
                            self.tox,
                            number,
                            kind.as_raw(),
                            body.as_ptr(),
                            body.len(),
                            &raw mut error,
                        )
                    };
                    if ok {
                        Ok(())
                    } else {
                        Err(ToxError::Conference {
                            operation: "send_message",
                            code: error,
                            name: conference_send_error_name(error),
                        })
                    }
                });
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::ConferenceLeave {
                conference_id,
                reply,
            } => {
                let outcome = conference_number_for(self.tox, &conference_id).and_then(|number| {
                    let mut error = -1;
                    // SAFETY: worker thread, live instance.
                    let ok =
                        unsafe { ffi::tox_conference_delete(self.tox, number, &raw mut error) };
                    if ok {
                        // The conference is gone, so its handshake record is too:
                        // a later conference must never inherit it.
                        if let Ok(mut connected) = self.ctx.connected.lock() {
                            connected.remove(&conference_id);
                        }
                        Ok(())
                    } else {
                        Err(ToxError::Conference {
                            operation: "delete",
                            code: error,
                            name: conference_delete_error_name(error),
                        })
                    }
                });
                let _ = reply.send(outcome);
                false
            }
            ToxCommand::Save { reply } => {
                let _ = reply.send(save_savedata(self.tox, &self.savedata_path));
                false
            }
            ToxCommand::Shutdown { reply } => {
                // Persist first, then report; the savedata path itself is not
                // interesting to a caller that is shutting down.
                let result = save_savedata(self.tox, &self.savedata_path).map(|_| ());
                let _ = reply.send(result);
                true
            }
        }
    }

    /// Read the friend list, every friend's state, and our own connection state.
    fn snapshot(&self) -> ToxSnapshot {
        // SAFETY: worker thread, live instance.
        let count = unsafe { ffi::tox_self_get_friend_list_size(self.tox) };
        let mut list = vec![0_u32; count];
        if count > 0 {
            // SAFETY: `list` has room for exactly `count` entries.
            unsafe { ffi::tox_self_get_friend_list(self.tox, list.as_mut_ptr()) };
        }

        let friends = list
            .into_iter()
            .map(|number| {
                let mut error = -1;
                // SAFETY: worker thread, live instance.
                let status = unsafe {
                    ffi::tox_friend_get_connection_status(self.tox, number, &raw mut error)
                };
                ToxFriend {
                    number,
                    public_key: friend_public_key(self.tox, number),
                    name: friend_name(self.tox, number),
                    status: ToxConnection::from_raw(status),
                }
            })
            .collect();

        // SAFETY: worker thread, live instance.
        let self_connection =
            ToxConnection::from_raw(unsafe { ffi::tox_self_get_connection_status(self.tox) });

        // A conference is listed by toxcore as soon as it exists; whether it is
        // *connected* is decided by `conference_info` from the recorded handshake
        // and the peer count (see its documentation for why both are needed).
        let mut handshakes = self.ctx.connected.lock();
        let conferences: Vec<ToxConference> = conference_numbers(self.tox)
            .into_iter()
            .map(|number| {
                let handshake_finished = handshakes
                    .as_ref()
                    .is_ok_and(|connected| connected.contains(&conference_id(self.tox, number)));
                conference_info(self.tox, number, handshake_finished)
            })
            .collect();

        // Drop handshake records for conferences that are gone: a conference can
        // disappear without a `ConferenceLeave` command (it is deleted on the other
        // side, or dropped from the savedata), and the set must not become a record
        // of every conference ever joined in this process.
        if let Ok(known) = handshakes.as_mut() {
            let live: Vec<&str> = conferences
                .iter()
                .map(|conference| conference.id.as_str())
                .collect();
            known.retain(|id| live.iter().any(|live| live.eq_ignore_ascii_case(id)));
        }

        ToxSnapshot {
            self_connection,
            friends,
            conferences,
            nickname: self_name(self.tox),
        }
    }
}

/// The worker thread: create the instance, report readiness, then loop.
#[cfg(toxcore_found)]
fn run_worker(
    config: &ToxConfig,
    commands: &Receiver<ToxCommand>,
    events: SyncSender<ToxEvent>,
    dropped: Arc<AtomicU64>,
    ready: &Sender<ToxResult<ToxIdentity>>,
) {
    let savedata_path = config.savedata_path();
    // A zero byte file is not valid Tox data; treat it as "no identity yet" so
    // an interrupted write cannot make the transport unusable.
    let savedata = std::fs::read(&savedata_path)
        .ok()
        .filter(|data| !data.is_empty());

    // SAFETY: every toxcore call for this instance happens on this thread, and
    // the pointer never leaves it.
    let tox = match unsafe { create_tox(config, savedata.as_deref()) } {
        Ok(tox) => tox,
        Err(error) => {
            // Savedata that toxcore refuses to load is reported as such instead
            // of being silently replaced by a fresh identity. This arm returns,
            // so the path can be moved rather than cloned.
            let error = match (savedata.is_some(), error) {
                (true, ToxError::New(code)) => ToxError::Savedata {
                    path: savedata_path,
                    code,
                },
                (_, other) => other,
            };
            let _ = ready.send(Err(error));
            return;
        }
    };

    // SAFETY: single-threaded ownership was established above.
    let identity = unsafe { initialize_identity(tox, config) };
    unsafe { bootstrap(tox, &config.bootstrap_nodes) };
    unsafe {
        ffi::tox_callback_self_connection_status(tox, on_self_connection_status);
        ffi::tox_callback_friend_connection_status(tox, on_friend_connection_status);
        ffi::tox_callback_friend_name(tox, on_friend_name);
        ffi::tox_callback_friend_request(tox, on_friend_request);
        ffi::tox_callback_friend_message(tox, on_friend_message);
        ffi::tox_callback_conference_invite(tox, on_conference_invite);
        ffi::tox_callback_conference_connected(tox, on_conference_connected);
        ffi::tox_callback_conference_message(tox, on_conference_message);
        ffi::tox_callback_conference_peer_list_changed(tox, on_conference_peers_changed);
        ffi::tox_callback_conference_title(tox, on_conference_title);
    }

    let worker = Worker {
        tox,
        ctx: Box::new(CallbackCtx {
            events,
            dropped,
            connected: Arc::new(Mutex::new(HashSet::new())),
        }),
        savedata_path,
    };

    // The transport is up; hand the identity back to `ToxClient::start`.
    let _ = ready.send(Ok(identity));

    loop {
        worker.iterate();
        match commands.recv_timeout(worker.interval()) {
            Ok(command) => {
                if worker.handle(command) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Every handle is gone: persist before leaving.
                let _ = save_savedata(worker.tox, &worker.savedata_path);
                break;
            }
        }
    }

    // SAFETY: the instance is not touched after this point.
    unsafe { ffi::tox_kill(worker.tox) };
}

#[cfg(toxcore_found)]
impl ToxClient {
    /// Create a Tox instance and start its worker thread.
    ///
    /// The identity is loaded from `<data_dir>/tox-savedata.bin` when present,
    /// so the Tox address is stable across restarts. Without a savedata file a
    /// fresh identity is generated and persisted on save or shutdown.
    ///
    /// # Errors
    ///
    /// Returns [`ToxError::New`] when toxcore cannot create the instance, or
    /// [`ToxError::Io`] when the worker thread cannot be spawned.
    pub fn start(config: ToxConfig) -> ToxResult<Self> {
        // Fail before spawning a thread when the configuration is unusable.
        config.validate()?;
        let savedata_path = config.savedata_path();
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let worker_dropped = Arc::clone(&dropped);
        let sender_dropped = Arc::clone(&dropped);
        let handle = thread::Builder::new()
            .name("tox-worker".to_string())
            .spawn(move || run_worker(&config, &command_rx, event_tx, worker_dropped, &ready_tx))
            .map_err(ToxError::Io)?;

        let identity = match ready_rx.recv() {
            Ok(Ok(identity)) => identity,
            Ok(Err(error)) => {
                let _ = handle.join();
                return Err(error);
            }
            Err(_) => {
                let _ = handle.join();
                return Err(ToxError::WorkerStopped);
            }
        };

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            identity,
            savedata_path,
            dropped: sender_dropped,
            worker: Some(handle),
            stopped: false,
        })
    }
}

#[cfg(not(toxcore_found))]
impl ToxClient {
    /// Always fails: this build did not find `libtoxcore`, so the Tox transport
    /// is unavailable.
    ///
    /// Install the library and rebuild with `--features tox-protocol` to enable
    /// it (see `build.rs`).
    ///
    /// # Errors
    ///
    /// Always returns [`ToxError::Unavailable`].
    pub fn start(_config: ToxConfig) -> ToxResult<Self> {
        Err(ToxError::Unavailable)
    }
}

#[cfg(test)]
mod hex_tests {
    use super::*;

    /// Hex encoding/decoding round-trips and rejects malformed input.
    #[test]
    fn test_hex_roundtrip_and_errors() {
        let bytes: Vec<u8> = (0..=255_u8).collect();
        let encoded = encode_hex(&bytes);
        assert_eq!(encoded.len(), bytes.len() * 2);
        assert_eq!(decode_hex(&encoded).expect("decode"), bytes);

        assert!(matches!(decode_hex("abc"), Err(ToxError::NotHex(_))));
        assert!(matches!(decode_hex("zz"), Err(ToxError::NotHex(_))));
    }

    /// Bootstrap node keys are validated at construction time.
    #[test]
    fn test_bootstrap_node_validation() {
        let key = "F404ABAA1C99A9D37D61AB54898F56793E1DEF8BD46B1038B9D822E8460FAB67";
        let node = BootstrapNode::new("node.example.org", 33445, key).expect("valid node");
        assert_eq!(node.port, 33445);
        assert_eq!(node.public_key, key);

        // A short key is rejected.
        assert!(matches!(
            BootstrapNode::new("node.example.org", 33445, "AABB"),
            Err(ToxError::BadKeyLength(2))
        ));
    }

    /// A `host:port:PUBLIC_KEY` bootstrap entry is parsed, and every malformed
    /// shape is rejected with the same error variant.
    #[test]
    fn test_bootstrap_node_parse() {
        let key = "F404ABAA1C99A9D37D61AB54898F56793E1DEF8BD46B1038B9D822E8460FAB67";

        let node = BootstrapNode::parse(&format!("node.example.org:33445:{key}")).expect("parsed");
        assert_eq!(node.host, "node.example.org");
        assert_eq!(node.port, 33445);
        assert_eq!(node.public_key, key);

        // Bracketed IPv6 keeps its colons.
        let node = BootstrapNode::parse(&format!("[::1]:33445:{key}")).expect("parsed");
        assert_eq!(node.host, "::1");
        assert_eq!(node.port, 33445);

        // Surrounding whitespace is ignored.
        assert!(BootstrapNode::parse(&format!(" node.example.org:33445:{key} ")).is_ok());

        for malformed in [
            "node.example.org:33445".to_string(),      // key missing
            "node.example.org:33445:AABB".to_string(), // key too short
            format!("node.example.org:0:{key}"),       // port 0
            format!("node.example.org:99999:{key}"),   // port out of range
            format!(":{key}"),                         // host missing
            format!("node.example.org:33445:{}", "Z".repeat(64)), // not hex
        ] {
            assert!(
                matches!(
                    BootstrapNode::parse(&malformed),
                    Err(ToxError::MalformedBootstrap(_))
                ),
                "'{malformed}' must be rejected"
            );
        }
    }

    /// The lengths `cli` validates against agree with the ones toxcore reports.
    #[test]
    fn test_cli_and_tox_lengths_agree() {
        assert_eq!(crate::cli::TOX_ADDRESS_HEX_LEN, ADDRESS_SIZE * 2);
        assert_eq!(crate::cli::PUBLIC_KEY_HEX_LEN, PUBLIC_KEY_SIZE * 2);
    }

    /// The message type round-trips through its raw toxcore code, and an unknown
    /// code degrades to a normal message instead of failing.
    #[test]
    fn test_message_kind_round_trips() {
        for kind in [ToxMessageKind::Normal, ToxMessageKind::Action] {
            assert_eq!(ToxMessageKind::from_raw(kind.as_raw()), kind);
        }
        // `TOX_MESSAGE_TYPE_NORMAL = 0`, `TOX_MESSAGE_TYPE_ACTION = 1`; anything
        // else is treated as normal text rather than dropped.
        assert_eq!(ToxMessageKind::from_raw(0), ToxMessageKind::Normal);
        assert_eq!(ToxMessageKind::from_raw(1), ToxMessageKind::Action);
        assert_eq!(ToxMessageKind::from_raw(7), ToxMessageKind::Normal);
        assert_eq!(ToxMessageKind::from_raw(-1), ToxMessageKind::Normal);
        assert_eq!(ToxMessageKind::Normal.as_raw(), 0);
        assert_eq!(ToxMessageKind::Action.as_raw(), 1);
    }

    /// The callback hand-off is bounded, but generously enough that a normal
    /// burst never loses an event.
    ///
    /// Asserted at compile time: both operands are constants, so the compiler is
    /// the only thing that can be wrong about them, and an `assert!` that can
    /// never be false would not be testing anything at run time.
    #[test]
    fn test_event_queue_capacity_is_bounded_but_generous() {
        const _: () = assert!(
            EVENT_QUEUE_CAPACITY >= 1024,
            "a small queue would shed ordinary bursts"
        );
        const _: () = assert!(
            EVENT_QUEUE_CAPACITY <= 1_048_576,
            "an enormous queue defeats the point of bounding it"
        );
    }

    /// A freshly generated Tox address satisfies the checksum rule.
    #[cfg(toxcore_found)]
    #[test]
    fn test_generated_address_checksum_is_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "checksum-test");
        let client = ToxClient::start(config).expect("start");

        let address = client.address().to_string();
        assert_eq!(address.len(), ADDRESS_SIZE * 2);
        assert!(
            client.identity().checksum_is_valid().expect("valid hex"),
            "the address toxcore generated must satisfy the checksum rule: {address}"
        );

        client.shutdown().expect("shutdown");
    }

    /// Without libtoxcore the module is a stub that fails clearly.
    #[cfg(not(toxcore_found))]
    #[test]
    fn test_start_reports_unavailable_without_libtoxcore() {
        assert!(!is_linked(), "the stub build must report itself unlinked");
        let dir = tempfile::tempdir().expect("tempdir");
        let error = ToxClient::start(ToxConfig::new(dir.path(), "stub"))
            .expect_err("the stub must refuse to start");
        assert!(matches!(error, ToxError::Unavailable));
    }
}

#[cfg(all(test, toxcore_found))]
mod tests {
    use super::*;

    /// Start a client in its own temporary directory.
    fn client(name: &str) -> (ToxClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), name);
        let client = ToxClient::start(config).expect("start");
        (client, dir)
    }

    /// Change the last hex digit, which must break the checksum.
    fn corrupt(address: &str) -> String {
        let mut chars: Vec<char> = address.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        chars.into_iter().collect()
    }

    /// This build really did link the Tox library.
    #[test]
    fn test_library_is_linked() {
        assert!(is_linked(), "toxcore_found but is_linked() is false");
    }

    /// The `[network] enable_ipv6` switch reaches toxcore.
    ///
    /// `tox_options_set_ipv6_enabled` is the only way that documented key can mean
    /// anything for the Tox transport, so this pins both that the default matches
    /// toxcore (on) and that an instance created with it off still comes up — a
    /// missing symbol or a wrong signature fails here instead of at run time.
    #[test]
    fn test_ipv6_switch_is_applied_to_the_instance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = ToxConfig::new(dir.path(), "no-ipv6");
        assert!(config.ipv6_enabled, "IPv6 is on by default, like toxcore");

        config.ipv6_enabled = false;
        let client = ToxClient::start(config).expect("start with IPv6 disabled");
        assert!(!client.identity().address.is_empty());
        client.shutdown().expect("shutdown");
    }

    /// A conference can be created, listed, described and left without a network.
    ///
    /// Creating a conference is a local toxcore operation, so this covers the FFI
    /// binding, the identifier handling and the command round trip on a machine
    /// with no DHT access.
    #[test]
    fn test_conference_lifecycle_without_a_dht() {
        let (client, _dir) = client("conference");

        assert!(
            client.snapshot().expect("snapshot").conferences.is_empty(),
            "a fresh instance is in no conference"
        );

        let created = client.conference_new("Team").expect("create");
        assert_eq!(created.title, "Team");
        assert_eq!(created.id.len(), CONFERENCE_ID_SIZE * 2);
        assert!(
            created.id.chars().all(|c| c.is_ascii_hexdigit()),
            "the id must be hexadecimal: {created:?}"
        );
        // A locally created conference is not connected until a peer joins, and its
        // peer count includes **us**: nobody else is in the group yet, but the
        // participant list is not empty. (A front-end that reads `peers` as "other
        // people" would be off by one.)
        assert!(!created.connected);
        assert_eq!(created.peers, 1, "only we are in a freshly created group");

        let snapshot = client.snapshot().expect("snapshot");
        assert_eq!(snapshot.conferences.len(), 1, "{snapshot:?}");
        assert_eq!(snapshot.conferences[0].id, created.id);
        assert_eq!(snapshot.conferences[0].title, "Team");
        // Reading the conference back must agree with the reply that created it.
        // toxcore lists a conference as soon as it exists, and it never raises
        // `conference_connected` for a conference we created, so "connected" cannot
        // be a constant: a solo conference is genuinely not reachable (a send to it
        // fails with `NO_CONNECTION`). Reporting `true` here would contradict
        // `conference_new` and make every "connecting" state unreachable.
        assert!(
            !snapshot.conferences[0].connected,
            "a peerless conference must not report itself connected: {snapshot:?}"
        );

        // The same identifier, in another case, still resolves.
        client
            .conference_leave(&created.id.to_uppercase())
            .expect("an uppercase id must resolve too");
        assert!(
            client.snapshot().expect("snapshot").conferences.is_empty(),
            "leaving must remove the conference"
        );

        client.shutdown().expect("shutdown");
    }
    /// A conference title a peer chose is held to the label rule, like every other string
    /// a peer picks and this instance prints.
    ///
    /// A title is read from toxcore (`conference_title`), not echoed back from the command
    /// that set it, and it is what a front-end prints as the group's name. toxcore accepts
    /// any bytes within its length limit, so a conference can be named with something that
    /// clears the reader's screen. The rule is applied in the reader, so every consumer
    /// (`/group list`, the group pane, a group message's label, the title cache) sees the
    /// same refusal.
    #[test]
    fn test_a_conference_title_that_cannot_be_displayed_is_refused() {
        let (client, _dir) = client("titles");

        let hostile = client.conference_new("Team\x1b[2J").expect("create");
        assert!(
            hostile.title.is_empty(),
            "the reply that created it already reports no title: {:?}",
            hostile.title
        );

        let snapshot = client.snapshot().expect("snapshot");
        assert_eq!(snapshot.conferences.len(), 1);
        assert_eq!(snapshot.conferences[0].id, hostile.id);
        assert!(
            snapshot.conferences[0].title.is_empty(),
            "an undisplayable title must be reported as no title: {:?}",
            snapshot.conferences[0].title
        );
        // The raw string did reach toxcore (a conference exists), so the refusal is ours
        // and not toxcore's: the id and the membership are unaffected, only the label is.
        assert!(!snapshot.conferences[0].id.is_empty());

        // A well-formed title still comes through, so the rule refuses the label and not
        // the mechanism.
        client.conference_new("Team").expect("create");
        let snapshot = client.snapshot().expect("snapshot");
        let titles: Vec<&str> = snapshot
            .conferences
            .iter()
            .map(|conference| conference.title.as_str())
            .collect();
        assert!(titles.contains(&"Team"), "{titles:?}");

        client.shutdown().expect("shutdown");
    }

    /// A rename is reported as its own event and is never mistaken for a chat
    /// message.
    ///
    /// `tox_conference_title` only fires when a *peer* renames the conference,
    /// so the callback itself needs a second participant and therefore the DHT.
    /// What can be checked without a network is the vocabulary the transport
    /// consumes: a title change carries no body, so it must never be decoded as
    /// text (`ToxEvent::text`), which is what would turn a rename into a message
    /// with an empty body.
    #[test]
    fn test_conference_title_change_is_not_a_message() {
        let event = ToxEvent::ConferenceTitleChanged {
            conference_id: "ab".repeat(CONFERENCE_ID_SIZE),
            title: "Renamed".to_string(),
            peers: 2,
        };

        assert_eq!(event.text(), None, "a rename carries no message body");
        assert!(!matches!(
            event,
            ToxEvent::Message { .. } | ToxEvent::ConferenceMessage { .. }
        ));
        // The event is part of the public equality contract the transport relies
        // on when it folds the three group events into one `AppEvent`.
        assert_eq!(
            event,
            ToxEvent::ConferenceTitleChanged {
                conference_id: "ab".repeat(CONFERENCE_ID_SIZE),
                title: "Renamed".to_string(),
                peers: 2,
            }
        );
    }

    /// Every group operation validates its input before it reaches toxcore.
    #[test]
    fn test_conference_input_is_validated() {
        let (client, _dir) = client("conference-validation");
        let created = client.conference_new("Validated").expect("create");

        // A title longer than toxcore allows is refused with the field named.
        let error = client
            .conference_new(&"x".repeat(MAX_CONFERENCE_TITLE_LENGTH + 1))
            .expect_err("an over-long title must be refused");
        assert!(matches!(error, ToxError::TooLong { field, .. } if field == "conference title"));

        // An unknown conference is reported as such, not as a silent no-op.
        let unknown = "ab".repeat(CONFERENCE_ID_SIZE);
        let error = client
            .conference_leave(&unknown)
            .expect_err("an unknown conference must be refused");
        assert!(matches!(error, ToxError::UnknownConference(_)), "{error:?}");
        let error = client
            .conference_send(&unknown, b"hi", ToxMessageKind::Normal)
            .expect_err("an unknown conference must be refused");
        assert!(matches!(error, ToxError::UnknownConference(_)), "{error:?}");
        let error = client
            .conference_invite(&unknown, 0)
            .expect_err("an unknown conference must be refused");
        assert!(matches!(error, ToxError::UnknownConference(_)), "{error:?}");

        // A malformed identifier is distinguished from an unknown one.
        let error = client
            .conference_leave("not-hex")
            .expect_err("a non-hex id must be refused");
        assert!(matches!(error, ToxError::NotHex(_)), "{error:?}");
        let error = client
            .conference_leave("ab")
            .expect_err("a short id must be refused");
        assert!(
            matches!(error, ToxError::BadConferenceIdLength(1)),
            "{error:?}"
        );

        // The body rules are the same as for a friend message.
        let error = client
            .conference_send(&created.id, b"", ToxMessageKind::Normal)
            .expect_err("an empty message must be refused");
        assert!(matches!(error, ToxError::EmptyMessage), "{error:?}");
        let error = client
            .conference_send(
                &created.id,
                &vec![0_u8; MAX_MESSAGE_LENGTH + 1],
                ToxMessageKind::Normal,
            )
            .expect_err("an over-long message must be refused");
        assert!(matches!(error, ToxError::TooLong { field, .. } if field == "conference message"));

        // Joining needs a token; an empty one is refused before toxcore sees it.
        let error = client
            .conference_join(0, &[])
            .expect_err("an empty token must be refused");
        assert!(matches!(error, ToxError::EmptyMessage), "{error:?}");

        client.shutdown().expect("shutdown");
    }

    /// The identity survives a save/shutdown/restart cycle.
    #[test]
    fn test_identity_is_stable_across_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "stable");

        let first = ToxClient::start(config.clone()).expect("first start");
        let address = first.address().to_string();
        assert_eq!(address.len(), ADDRESS_SIZE * 2);
        let savedata = first.savedata_path().to_path_buf();
        first.shutdown().expect("shutdown");
        assert!(savedata.exists(), "shutdown must persist the savedata");

        let second = ToxClient::start(config).expect("second start");
        assert_eq!(
            second.address(),
            address,
            "the Tox address must be stable when the savedata is reused"
        );
        second.shutdown().expect("shutdown");
    }

    /// An empty instance reports no friends.
    #[test]
    fn test_snapshot_of_fresh_instance() {
        let (client, _dir) = client("snapshot");
        let snapshot = client.snapshot().expect("snapshot");
        assert!(snapshot.friends.is_empty());
        assert_eq!(snapshot.self_connection, ToxConnection::Offline);
        assert_eq!(snapshot.nickname, "snapshot");
        client.shutdown().expect("shutdown");
    }

    /// A quiet instance has shed nothing, so the drop counter starts at zero and
    /// stays there without overload.
    #[test]
    fn test_a_quiet_instance_sheds_nothing() {
        let (client, _dir) = client("quiet");

        // Both handles observe the same counter.
        let sender = client.sender();
        assert_eq!(sender.dropped_events(), 0);

        // Drive a few snapshot round trips: they must not shed anything either.
        for _ in 0..5 {
            client.snapshot().expect("snapshot");
        }
        assert_eq!(sender.dropped_events(), 0);

        client.shutdown().expect("shutdown");
    }

    /// A corrupted address is rejected by toxcore's checksum check.
    #[test]
    fn test_corrupt_address_is_rejected() {
        let (client, _dir) = client("corrupt");
        let broken = corrupt(client.address());
        let error = client
            .add_friend(&broken, "hello")
            .expect_err("a corrupted address must be rejected");
        match error {
            ToxError::FriendAdd(code, name) => {
                assert_eq!(code, 6, "expected TOX_ERR_FRIEND_ADD_BAD_CHECKSUM");
                assert_eq!(name, "BAD_CHECKSUM");
            }
            other => panic!("expected a friend-add error, got {other:?}"),
        }
        client.shutdown().expect("shutdown");
    }

    /// Adding our own address is refused with `OWN_KEY`.
    #[test]
    fn test_adding_own_address_is_rejected() {
        let (client, _dir) = client("own");
        let own = client.address().to_string();
        let error = client
            .add_friend(&own, "talking to myself")
            .expect_err("own key must be refused");
        match error {
            ToxError::FriendAdd(code, name) => {
                assert_eq!(code, 4, "expected TOX_ERR_FRIEND_ADD_OWN_KEY");
                assert_eq!(name, "OWN_KEY");
            }
            other => panic!("expected a friend-add error, got {other:?}"),
        }
        client.shutdown().expect("shutdown");
    }

    /// Two instances can be linked locally: A adds B and sees the friend.
    #[test]
    fn test_two_instances_can_be_linked() {
        let (alice, _dir_a) = client("alice");
        let (bob, _dir_b) = client("bob");

        let number = alice
            .add_friend(bob.address(), "hi from alice")
            .expect("friend request must be accepted locally");
        assert_eq!(number, 0, "the first friend gets number 0");

        let snapshot = alice.snapshot().expect("snapshot");
        assert_eq!(snapshot.friends.len(), 1);
        assert_eq!(snapshot.friends[0].public_key, bob.public_key());
        // Without a network the friend is not connected yet.
        assert_eq!(snapshot.friends[0].status, ToxConnection::Offline);

        alice.shutdown().expect("shutdown");
        bob.shutdown().expect("shutdown");
    }

    /// Sending to an unknown friend number reports `FRIEND_NOT_FOUND`.
    #[test]
    fn test_send_to_unknown_friend_fails() {
        let (client, _dir) = client("sender");
        let error = client
            .send_message(999, "anybody there?")
            .expect_err("unknown friend must fail");
        match error {
            ToxError::SendMessage(code, name) => {
                assert_eq!(
                    code, 2,
                    "expected TOX_ERR_FRIEND_SEND_MESSAGE_FRIEND_NOT_FOUND"
                );
                assert_eq!(name, "FRIEND_NOT_FOUND");
            }
            other => panic!("expected a send error, got {other:?}"),
        }

        // Empty and oversized bodies are rejected before reaching toxcore.
        assert!(matches!(
            client.send_message(0, ""),
            Err(ToxError::EmptyMessage)
        ));
        let too_long = "x".repeat(MAX_MESSAGE_LENGTH + 1);
        assert!(matches!(
            client.send_message(0, &too_long),
            Err(ToxError::TooLong {
                field: "message",
                limit: MAX_MESSAGE_LENGTH,
                ..
            })
        ));
        // Exactly at the limit still reaches toxcore (which then reports the
        // unknown friend rather than a length problem).
        let at_limit = "x".repeat(MAX_MESSAGE_LENGTH);
        assert!(matches!(
            client.send_message(999, &at_limit),
            Err(ToxError::SendMessage(2, "FRIEND_NOT_FOUND"))
        ));

        client.shutdown().expect("shutdown");
    }

    /// Invalid addresses never reach toxcore.
    #[test]
    fn test_address_validation() {
        let (client, _dir) = client("validate");
        assert!(matches!(
            client.add_friend("not hex", "hi"),
            Err(ToxError::NotHex(_))
        ));
        assert!(matches!(
            client.add_friend("AABB", "hi"),
            Err(ToxError::BadAddressLength(2))
        ));
        assert!(matches!(
            client.add_friend(client.address(), ""),
            Err(ToxError::EmptyMessage)
        ));
        let long_request = "x".repeat(MAX_FRIEND_REQUEST_LENGTH + 1);
        assert!(matches!(
            client.add_friend(client.address(), &long_request),
            Err(ToxError::TooLong {
                field: "friend request",
                limit: MAX_FRIEND_REQUEST_LENGTH,
                ..
            })
        ));
        assert!(matches!(
            client.accept_friend("AABB"),
            Err(ToxError::BadKeyLength(2))
        ));
        client.shutdown().expect("shutdown");
    }

    /// The handle can be moved onto another thread, which is what an async
    /// consumer needs.
    #[test]
    fn test_client_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ToxClient>();
    }

    /// The command handle is shareable: `Send + Sync` and `Clone`.
    #[test]
    fn test_sender_is_send_sync_and_cloneable() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<ToxSender>();
    }

    /// A `ToxSender` drives the instance from another thread while the client
    /// keeps consuming events.
    #[test]
    fn test_sender_is_usable_from_another_thread() {
        let (client, _dir) = client("sender-thread");
        let sender = client.sender();

        let handle = std::thread::spawn(move || -> ToxResult<usize> {
            sender.set_nickname("from-thread")?;
            sender.set_status_message("set elsewhere")?;
            Ok(sender.snapshot()?.friends.len())
        });
        assert_eq!(handle.join().expect("join").expect("commands"), 0);

        let snapshot = client.snapshot().expect("snapshot");
        assert_eq!(snapshot.nickname, "from-thread");

        client.shutdown().expect("shutdown");
    }

    /// The CLI's Tox address length and this module's agree.
    #[test]
    fn test_address_length_matches_the_cli_constant() {
        assert_eq!(ADDRESS_SIZE * 2, crate::cli::TOX_ADDRESS_HEX_LEN);
    }

    /// The carried-over bootstrap node list is well formed.
    #[test]
    fn test_default_bootstrap_nodes_are_valid() {
        let nodes = default_bootstrap_nodes();
        assert!(nodes.len() >= 5, "expected the C reference node set");
        for node in &nodes {
            assert!(!node.host.is_empty());
            assert_ne!(node.port, 0);
            assert_eq!(node.public_key.len(), PUBLIC_KEY_SIZE * 2);
        }
    }

    /// Accepting a requester's public key creates the friend locally.
    #[test]
    fn test_accept_friend_creates_the_contact() {
        let (alice, _dir_a) = client("requester");
        let (bob, _dir_b) = client("acceptor");

        let number = bob
            .accept_friend(alice.public_key())
            .expect("accepting a public key must work");
        assert_eq!(number, 0);

        let snapshot = bob.snapshot().expect("snapshot");
        assert_eq!(snapshot.friends.len(), 1);
        assert_eq!(snapshot.friends[0].public_key, alice.public_key());

        alice.shutdown().expect("shutdown");
        bob.shutdown().expect("shutdown");
    }

    /// Dropping a handle without `shutdown` still persists the identity.
    #[test]
    fn test_drop_without_shutdown_persists_the_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "dropped");
        let savedata = config.savedata_path();

        let client = ToxClient::start(config).expect("start");
        let address = client.address().to_string();
        drop(client);

        assert!(savedata.exists(), "drop must persist the savedata");
        let raw = std::fs::read(&savedata).expect("read");
        assert!(!raw.is_empty(), "the savedata must not be empty");

        // And the identity is reusable afterwards.
        let again = ToxClient::start(ToxConfig::new(dir.path(), "dropped")).expect("restart");
        assert_eq!(again.address(), address);
        again.shutdown().expect("shutdown");
    }

    /// Many sequential commands keep working (the worker stays responsive).
    #[test]
    fn test_repeated_commands_are_served() {
        let (client, _dir) = client("busy");
        for _ in 0..25 {
            let snapshot = client.snapshot().expect("snapshot");
            assert!(snapshot.friends.is_empty());
        }
        client.set_status_message("busy but alive").expect("status");
        client.shutdown().expect("shutdown");
    }

    /// Savedata toxcore refuses to load is reported, not silently replaced.
    ///
    /// toxcore is lenient: a truncated or bit-flipped savedata is usually
    /// loaded on a best-effort basis. It does refuse obviously malformed input
    /// (zero length, or a few bytes), which is what this exercises.
    #[test]
    fn test_unloadable_savedata_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "bad-savedata");
        std::fs::write(config.savedata_path(), [0xDE_u8, 0xAD, 0xBE, 0xEF])
            .expect("seed the bad file");

        let error = ToxClient::start(config).expect_err("unloadable savedata must be reported");
        match error {
            ToxError::Savedata { path, code } => {
                assert!(
                    path.ends_with(SAVEDATA_FILE),
                    "unexpected path {}",
                    path.display()
                );
                assert_ne!(code, 0, "toxcore must report a failure code");
            }
            other => panic!("expected a savedata error, got {other:?}"),
        }

        // The message tells the operator what to do.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "bad-savedata");
        std::fs::write(config.savedata_path(), [0x00_u8, 0x01, 0x02, 0x03]).expect("seed");
        let message = ToxClient::start(config).expect_err("must fail").to_string();
        assert!(
            message.contains("could not be loaded") && message.contains("move it aside"),
            "unhelpful message: {message}"
        );
    }

    /// A zero byte savedata file is treated as "no identity yet".
    #[test]
    fn test_empty_savedata_creates_a_new_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ToxConfig::new(dir.path(), "empty-savedata");
        std::fs::write(config.savedata_path(), b"").expect("seed the empty file");

        let client = ToxClient::start(config).expect("an empty savedata must not be fatal");
        assert_eq!(client.address().len(), ADDRESS_SIZE * 2);
        client.shutdown().expect("shutdown");
    }

    /// Configuration is validated before a thread is spawned.
    #[test]
    fn test_config_validation() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut inverted = ToxConfig::new(dir.path(), "ports");
        inverted.start_port = 40_000;
        inverted.end_port = 30_000;
        assert!(matches!(
            inverted.validate(),
            Err(ToxError::PortRange {
                start: 40_000,
                end: 30_000
            })
        ));
        assert!(matches!(
            ToxClient::start(inverted),
            Err(ToxError::PortRange { .. })
        ));

        let long_name = ToxConfig::new(dir.path(), "x".repeat(MAX_NAME_LENGTH + 1));
        assert!(matches!(
            long_name.validate(),
            Err(ToxError::TooLong {
                field: "nickname",
                ..
            })
        ));

        let mut long_status = ToxConfig::new(dir.path(), "ok");
        long_status.status_message = "x".repeat(MAX_STATUS_LENGTH + 1);
        assert!(matches!(
            long_status.validate(),
            Err(ToxError::TooLong {
                field: "status message",
                ..
            })
        ));

        // A nickname exactly at the limit is accepted.
        let at_limit = ToxConfig::new(dir.path(), "x".repeat(MAX_NAME_LENGTH));
        assert!(at_limit.validate().is_ok());

        // `end_port == 0` means "any port" and never conflicts.
        let mut any_port = ToxConfig::new(dir.path(), "ok");
        any_port.start_port = 40_000;
        any_port.end_port = 0;
        assert!(any_port.validate().is_ok());
    }

    /// Reaching the real DHT needs the internet, so it is opt-in.
    #[test]
    #[ignore = "requires internet access to the public Tox DHT"]
    fn test_bootstrap_reaches_the_dht() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = ToxConfig::new(dir.path(), "bootstrap");
        config.bootstrap_nodes = vec![
            BootstrapNode::new(
                "node.tox.biribiri.org",
                33445,
                "F404ABAA1C99A9D37D61AB54898F56793E1DEF8BD46B1038B9D822E8460FAB67",
            )
            .expect("node"),
            BootstrapNode::new(
                "144.217.167.73",
                33445,
                "7E5668E0EE09E19F320AD47902419331FFEE147BB3606769CFBE921A2A2FD34C",
            )
            .expect("node"),
        ];

        let client = ToxClient::start(config).expect("start");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut online = false;
        while std::time::Instant::now() < deadline && !online {
            if let Some(ToxEvent::SelfConnection { status }) =
                client.next_event(Duration::from_secs(1))
            {
                online = status != ToxConnection::Offline;
            }
            if client
                .snapshot()
                .is_ok_and(|snapshot| snapshot.self_connection != ToxConnection::Offline)
            {
                online = true;
            }
        }
        client.shutdown().expect("shutdown");
        assert!(online, "the DHT was not reached within 30 seconds");
    }
}
