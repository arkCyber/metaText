/*!
 * network.rs
 *
 * Encrypted peer to peer transport for metaText
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Real TCP transport with a length-prefixed framing protocol
 * - Authenticated encryption of every payload via [`CryptoManager`]
 * - Peer registry with nicknames announced during the handshake
 * - Graceful startup, connection and shutdown of all I/O tasks
 */

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use socket2::{Domain, Protocol, Socket, Type};

use crate::crypto::CryptoManager;
use crate::error::{MetaTextError, MetaTextResult};
use crate::identity::{self, EphemeralKey, NetworkIdentity};
use crate::trust::{PeerIdentity, PinStore};
use crate::types::{AppEvent, ContentType, MessageKind, NetworkEvent};
use meta_text_proto::ipc::validation;

/// Backlog passed to `listen`, matching what tokio uses for `TcpListener::bind`.
const LISTEN_BACKLOG: i32 = 1024;

/// Frame kind carrying the peer's nickname (plain text).
const FRAME_HELLO: u8 = 1;

/// Frame kind carrying an encrypted chat message.
const FRAME_MESSAGE: u8 = 2;

/// Frame kind carrying an encrypted third-person action (`/me`).
///
/// A separate kind keeps the frame body layout identical to [`FRAME_MESSAGE`],
/// so the message id can still be read from the first bytes; only the payload
/// meaning differs. An older peer that does not know this kind reports it as
/// unknown instead of misreading it as a chat message.
const FRAME_ACTION: u8 = 4;

/// Frame kind carrying an encrypted binary body.
///
/// Like [`FRAME_ACTION`] this only changes the meaning of an otherwise identical
/// body, so a binary payload is never mistaken for text (and therefore never
/// reported as undecodable) by a peer that understands it.
const FRAME_BINARY: u8 = 5;

/// Frame kind acknowledging a received message.
const FRAME_ACK: u8 = 3;

/// Frame kind carrying the peer's session identity (plain text).
///
/// Sent right after [`FRAME_HELLO`]. A peer that does not know this kind logs it as
/// unknown and keeps working: without an identity a peer simply falls back to the
/// session key (see [`NetworkManager::contact_key`]).
const FRAME_IDENTITY: u8 = 6;

/// Frame kind carrying this connection's ephemeral public key (plain text).
///
/// Sent right after [`FRAME_IDENTITY`], once per connection. It is what turns the
/// announced identities into a **key agreement** ([`identity::pair_key`]): the key a
/// directed message is sealed under then depends on a secret that lives only on this
/// connection, so knowing the session key and both announced identities — what a
/// passive observer of the handshake has — is no longer enough to derive it.
///
/// A peer that does not know this kind logs it as unknown and keeps working: the two
/// then use the static derivation of [`crypto::contact_key`], exactly as they did
/// before the agreement existed.
const FRAME_EPHEMERAL: u8 = 7;

/// Frame kind carrying a keepalive: an empty frame whose only content is its
/// existence.
///
/// TCP reports nothing on a read while no data arrives, so a peer that vanished
/// without a FIN — a machine that lost power, a NAT that dropped its mapping — looks
/// **identical to an idle one**: it stays in the registry, `/peers` keeps listing it,
/// and a message addressed to it is written into a black hole until the kernel's
/// retransmit timer gives up (minutes later). Each connection now sends this frame on
/// a fixed cadence ([`KEEPALIVE_INTERVAL`], which `[network] keepalive_interval` may
/// shorten) and drops a connection from which it has received nothing for
/// [`IDLE_TIMEOUT`].
///
/// The deadline arms only once the peer has **shown** that it sends keepalives, so a
/// peer built before this frame kind — which ignores it, exactly as the comment above
/// describes for the other late additions, and therefore never sends one — keeps the
/// old behaviour instead of being dropped for silence. Half-open detection is
/// consequently a property of a *modern* pair, which §6.20 of the architecture
/// document states.
///
/// The kind carries no sequence number and expects no reply: any received frame
/// (a keepalive, a message, an ack) is equally good proof that the peer is alive, so
/// a request/response handshake would add state without adding evidence.
const FRAME_KEEPALIVE: u8 = 8;

/// Default cadence for [`FRAME_KEEPALIVE`]; `[network] keepalive_interval` may shorten
/// it but never lengthen it.
///
/// A fixed cadence rather than "only when idle": being idle would need a per-peer sent
/// timestamp for no functional gain, because the receiver uses the frame only as a
/// liveness proof and the frame costs five bytes. It is defined from
/// [`crate::config::DEFAULT_KEEPALIVE_INTERVAL`] so the documented default, the
/// configuration default and the wire behaviour cannot drift apart, and a configured
/// value above it is rejected by [`crate::config::AppConfig::problems`] — probing
/// *faster* is compatible with every peer, probing *slower* than a peer's deadline is
/// not.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(crate::config::DEFAULT_KEEPALIVE_INTERVAL);

/// How long a connection may receive nothing before it is dropped.
///
/// Three times the default [`KEEPALIVE_INTERVAL`], so that one delayed keepalive — a
/// scheduling stall, a retransmission — cannot tear down a healthy connection.
///
/// Deliberately **not** derived from the *configured* cadence: this deadline bounds the
/// *peer's* silence, and the peer may be a default one probing every 20 s. Scaling it
/// with a shortened local cadence would drop exactly the peer the default exists to
/// keep, which is the mismatch the one-way validation rule in the configuration
/// prevents. A shorter cadence therefore only ever adds evidence; it never weakens what
/// counts as silence.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes reserved at the front of message/ack frames for the message id.
const MESSAGE_ID_LEN: usize = 8;

/// Largest frame accepted from a peer.
///
/// The length prefix is attacker controlled, so it is validated against this
/// bound before any allocation happens.
const MAX_FRAME_SIZE: u32 = 64 * 1024;

/// How many frames may wait for one peer connection before the transport sheds.
///
/// The writer task drains this queue into the socket. A peer that stops reading —
/// a frozen process, a NAT that swallowed its window, or a hostile one — would
/// otherwise let the queue grow with whatever the core keeps sending, which is
/// remote-driven memory (R7 in the architecture document). Shedding is counted
/// ([`NetworkManager::dropped_frames`]) and reported as `payloads_dropped`, so an
/// overload is visible instead of silent. The capacity is deliberately generous:
/// normal chat bursts never reach it, and a dropped frame is lost, not buffered.
const PEER_QUEUE_CAPACITY: usize = 256;

/// Fallback connect timeout, used when the configuration asks for zero.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many transport events may wait for the core before the transport is
/// throttled.
///
/// The channel is bounded so that a peer sending frames faster than the core
/// can absorb them slows the peer down instead of growing the queue. The value
/// is generous enough that normal bursts never block.
pub const EVENT_INBOX_CAPACITY: usize = 1024;

/// How often the supervisor retries desired peers that are not connected.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// How many messages may wait for a single nickname.
const OUTBOX_CAPACITY: usize = 32;

/// How long a queued message is kept before it is dropped.
const OUTBOX_TTL: Duration = Duration::from_secs(300);

/// A message waiting for its destination to become reachable.
#[derive(Debug, Clone)]
struct OutboxEntry {
    /// Message id echoed back in acknowledgements
    message_id: u64,

    /// Encrypted payload
    ciphertext: Vec<u8>,

    /// Whether this is a chat message or a third-person action
    kind: MessageKind,

    /// How the payload bytes must be interpreted
    content_type: ContentType,

    /// When the message was queued
    queued_at: std::time::Instant,
}

/// Outcome of addressing a message to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The message was handed to a connected peer
    Sent(DeliveryReceipt),

    /// The peer is not connected, so the message is buffered until it appears
    Queued {
        /// Identifier echoed back in acknowledgements
        message_id: u64,

        /// One-based position in that peer's queue
        position: usize,
    },

    /// The queue for that peer is full, so the message was not accepted
    Dropped,
}

/// A peer the manager wants to stay connected to.
#[derive(Debug, Clone, Copy, Default)]
struct DesiredPeer {
    /// Resolved remote address of the live connection, if any
    resolved: Option<SocketAddr>,

    /// Consecutive failed connection attempts
    attempts: u32,
}

/// Addresses the manager keeps connected, keyed by the configured address.
type DesiredPeers = Arc<Mutex<HashMap<String, DesiredPeer>>>;

/// Messages waiting for a nickname to become reachable.
type Outbox = Arc<Mutex<HashMap<String, VecDeque<OutboxEntry>>>>;

/// Outcome of queueing a message for delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReceipt {
    /// Identifier that peers echo back when they acknowledge the message
    pub message_id: u64,

    /// How many peers the frame was queued for
    pub peers: usize,
}

/// A connected peer.
#[derive(Debug)]
struct Peer {
    /// Outbound queue drained by the peer's writer task.
    ///
    /// Bounded ([`PEER_QUEUE_CAPACITY`]) so that a peer which stops reading cannot
    /// make the transport hold an unbounded number of frames: the queue sheds and
    /// counts instead (see `NetworkManager::dropped_frames`).
    sender: mpsc::Sender<Vec<u8>>,

    /// Nickname announced by the peer during the handshake, if seen yet
    nickname: Option<String>,

    /// Static identity announced by the peer, if it announced one.
    ///
    /// This is what a pair key is derived from together with our own identity, and
    /// what is pinned (`crate::trust`). It is a *public* value.
    identity: Option<String>,

    /// The ephemeral key pair of this connection.
    ///
    /// One per connection, never re-announced, never persisted: the pair key of this
    /// connection is a function of it (see [`identity::pair_key`]), so a reconnect —
    /// and therefore a fresh pair — gets a different key.
    ephemeral: EphemeralKey,

    /// The ephemeral public key the peer announced on this connection, if it did.
    ///
    /// `None` means the peer announced no ephemeral, which is what a build from before
    /// the key agreement does (and what a peer whose announcement was unusable gets):
    /// the pair then keeps the static derivation it had.
    remote_ephemeral: Option<String>,

    /// Whether this peer has sent a keepalive on this connection.
    ///
    /// This is what *arms* the idle deadline ([`IDLE_TIMEOUT`]): a peer that has shown
    /// it sends keepalives is expected to keep sending them, so silence becomes
    /// evidence of a dead connection. A peer that never sends one (a build from before
    /// [`FRAME_KEEPALIVE`]) is never armed, which is what keeps the new deadline from
    /// dropping a perfectly healthy older peer.
    keepalive_seen: bool,
}

/// Shared state handed to every per-connection task.
#[derive(Debug, Clone)]
struct Shared {
    /// Encryption used for both directions
    crypto: Arc<CryptoManager>,

    /// Application event channel.
    ///
    /// This is a *bounded* channel: when the core is busy the transport is
    /// slowed down instead of accumulating events without limit. Sends must be
    /// awaited, which is why [`Shared::dispatch`] is asynchronous.
    event_sender: mpsc::Sender<AppEvent>,

    /// Nickname announced to peers
    nickname: Arc<RwLock<String>>,

    /// The identity announced to peers
    ///
    /// The transport holds the whole key pair, not just the announced string: the
    /// secret half is what a key agreement needs ([`identity::pair_key`]), and it never
    /// leaves this process. `None` until the core sets one, in which case no pair key
    /// can be derived and the session key is the only option.
    identity: Arc<RwLock<Option<NetworkIdentity>>>,

    /// Identities peers announced, so a change under a known nickname is reported
    pins: PinStore,

    /// Registry of connected peers
    peers: Arc<Mutex<HashMap<SocketAddr, Peer>>>,

    /// Background tasks owned by the manager, shared so connection tasks can
    /// register themselves for shutdown
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,

    /// Addresses this manager keeps connected
    desired_peers: DesiredPeers,

    /// Messages waiting for a peer to become reachable
    outbox: Outbox,

    /// How many queued messages were dropped because they expired
    outbox_expired: Arc<AtomicU64>,

    /// Upper bound on concurrent inbound connections
    max_connections: u32,

    /// How long a single outbound connection attempt may take
    connect_timeout: Duration,

    /// How long a peer has to announce itself before its slot is reclaimed
    handshake_timeout: Duration,

    /// How long a connection may receive nothing before it is dropped
    idle_timeout: Duration,

    /// How often keepalives are sent to greeted peers
    keepalive_interval: Duration,

    /// Frames shed because a peer's queue was full
    frames_dropped: Arc<AtomicU64>,

    /// Greetings whose nickname this instance could not display
    greetings_refused: Arc<AtomicU64>,
}

/// Network manager for handling P2P communication
#[derive(Debug)]
pub struct NetworkManager {
    /// Network port
    port: u16,

    /// Bootstrap nodes
    bootstrap_nodes: Vec<String>,

    /// Maximum connections
    max_connections: u32,

    /// How long a single outbound connection attempt may take
    connect_timeout: Duration,

    /// How long a peer has to announce itself before its slot is reclaimed
    handshake_timeout: Duration,

    /// How long a connection may receive nothing before it is dropped
    idle_timeout: Duration,

    /// How often keepalives are sent to greeted peers
    keepalive_interval: Duration,

    /// Frames shed because a peer's queue was full
    frames_dropped: Arc<AtomicU64>,

    /// Greetings whose nickname this instance could not display
    ///
    /// Counted rather than only logged: a peer that greets badly on purpose does it once
    /// per connection, so a reconnect loop would fill the log while telling nobody
    /// anything, and a counter is what makes the pattern visible in `/metrics` — the rule
    /// A36 established for the bounded request lists.
    greetings_refused: Arc<AtomicU64>,

    /// Whether the inbound listener is bound as a dual-stack IPv6 wildcard
    enable_ipv6: bool,

    /// Whether the network is started
    started: bool,

    /// Encryption shared with the application
    crypto: Arc<CryptoManager>,

    /// Application event channel
    event_sender: mpsc::Sender<AppEvent>,

    /// Nickname announced to peers
    nickname: Arc<RwLock<String>>,

    /// Session identity announced to peers
    identity: Arc<RwLock<Option<NetworkIdentity>>>,

    /// Identities peers announced, remembered in a file so a restart does not
    /// forget what was pinned
    pins: PinStore,

    /// Connected peers keyed by their remote address
    peers: Arc<Mutex<HashMap<SocketAddr, Peer>>>,

    /// Address the listener is bound to, if started
    listener_addr: Option<SocketAddr>,

    /// Background tasks owned by the manager
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,

    /// Addresses this manager keeps connected
    desired_peers: DesiredPeers,

    /// Messages waiting for a peer to become reachable
    outbox: Outbox,

    /// How many queued messages were dropped because they expired
    outbox_expired: Arc<AtomicU64>,

    /// Next outgoing message identifier
    next_message_id: Arc<AtomicU64>,
}

impl NetworkManager {
    /// Create a new network manager
    ///
    /// # Arguments
    ///
    /// * `config` - Network configuration
    /// * `crypto_manager` - Encryption shared with the application
    /// * `event_sender` - Bounded channel used to deliver events to the core.
    ///   The transport awaits it, so a slow consumer throttles the transport
    ///   instead of growing an unbounded queue.
    /// * `pins` - The identities peers announced, remembered across runs so a
    ///   change under a known nickname can be reported. A transport with no data
    ///   directory gets [`PinStore::volatile`], which still detects a change for
    ///   as long as the process runs.
    ///
    /// # Returns
    ///
    /// Returns an initialised [`NetworkManager`]; no socket is opened until
    /// [`NetworkManager::start`] is called.
    ///
    /// # Errors
    ///
    /// Currently this never fails, but the `Result` is kept so that future
    /// transports can validate the configuration first.
    ///
    /// `async` for the same reason, and because the Tox transport it will be
    /// selected *with* has a worker to start.
    #[allow(clippy::unused_async)]
    pub async fn new(
        config: &crate::config::NetworkConfig,
        crypto_manager: Arc<CryptoManager>,
        event_sender: mpsc::Sender<AppEvent>,
        pins: PinStore,
    ) -> MetaTextResult<Self> {
        let timestamp = chrono::Utc::now();
        info!(
            "🌐 [{}] Initializing network manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        let manager = Self {
            port: config.port,
            bootstrap_nodes: config.bootstrap_nodes.clone(),
            max_connections: config.max_connections,
            connect_timeout: if config.connection_timeout == 0 {
                DEFAULT_CONNECT_TIMEOUT
            } else {
                Duration::from_secs(config.connection_timeout)
            },
            // One knob, two deadlines: how long a dial may take, and how long a
            // peer that has connected has to announce itself. Both answer "how long
            // may this connection stay useless", and a peer that never greets is
            // exactly as useless as one that never answers.
            handshake_timeout: if config.connection_timeout == 0 {
                DEFAULT_CONNECT_TIMEOUT
            } else {
                Duration::from_secs(config.connection_timeout)
            },
            // Liveness, two halves with two different rules. The *probe cadence* is
            // configurable (`[network] keepalive_interval`), because a NAT that expires
            // an idle mapping sooner than the default 20 s is a real deployment and
            // probing faster is compatible with everybody. The *deadline* is not: it
            // bounds the peer's silence, and the peer may be a default one probing every
            // `KEEPALIVE_INTERVAL`, so it stays at three default cadences no matter how
            // fast this end probes. `problems()` rejects `0` (never probe) and anything
            // above the default; a caller that builds a manager directly, without
            // validating, gets the default rather than a zero interval.
            idle_timeout: IDLE_TIMEOUT,
            keepalive_interval: if config.keepalive_interval == 0 {
                KEEPALIVE_INTERVAL
            } else {
                Duration::from_secs(config.keepalive_interval)
            },
            frames_dropped: Arc::new(AtomicU64::new(0)),
            greetings_refused: Arc::new(AtomicU64::new(0)),
            enable_ipv6: config.enable_ipv6,
            started: false,
            crypto: crypto_manager,
            event_sender,
            nickname: Arc::new(RwLock::new(String::new())),
            identity: Arc::new(RwLock::new(None)),
            pins,
            peers: Arc::new(Mutex::new(HashMap::new())),
            listener_addr: None,
            tasks: Arc::new(Mutex::new(Vec::new())),
            desired_peers: Arc::new(Mutex::new(HashMap::new())),
            outbox: Arc::new(Mutex::new(HashMap::new())),
            outbox_expired: Arc::new(AtomicU64::new(0)),
            // Ids start at 1 so that 0 can be used as "no message".
            next_message_id: Arc::new(AtomicU64::new(1)),
        };

        info!(
            "✅ [{}] Network manager initialized on port {}",
            timestamp.format("%Y-%m-%d %H:%M:%S"),
            manager.port
        );

        Ok(manager)
    }

    /// Bind the listener and start accepting inbound connections
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] when the socket cannot be bound.
    ///
    /// `async` by contract: the Tox transport's twin awaits its worker, and the
    /// actor drives both through the same `CoreTransport::start`.
    #[allow(clippy::unused_async)]
    pub async fn start(&mut self) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "🌐 [{}] Starting network manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        if self.started {
            warn!("⚠️ Network manager already started");
            return Ok(());
        }

        let std_listener = bind_listener(self.port, self.enable_ipv6)?;
        // `from_std` registers the socket with the reactor; it also requires a
        // non-blocking socket, which every branch of `bind_listener` sets.
        let listener =
            TcpListener::from_std(std_listener).map_err(|error| MetaTextError::Network {
                message: format!("Failed to register the listener: {error}"),
                operation: "listen".to_string(),
                source: Some(Box::new(error)),
            })?;

        let local = listener
            .local_addr()
            .map_err(|error| MetaTextError::Network {
                message: format!("Failed to read the local address: {error}"),
                operation: "local_addr".to_string(),
                source: Some(Box::new(error)),
            })?;

        self.listener_addr = Some(local);
        // A configured port of `0` means "let the OS choose", so mirror back
        // the port actually in use.
        self.port = local.port();
        self.started = true;

        let shared = self.shared();
        let handle = tokio::spawn(async move { shared.accept_loop(listener).await });
        track_task(&self.tasks, handle);

        // Keep the desired peers connected, including ones that come up later.
        let supervisor = self.shared();
        let desired = Arc::clone(&self.desired_peers);
        let handle = tokio::spawn(async move { supervisor.supervise(desired).await });
        track_task(&self.tasks, handle);

        // Liveness on established connections: one task, one tiny frame per greeted
        // peer per interval, so a peer that vanished without a FIN is noticed instead
        // of being written to forever.
        let keepalive = self.shared();
        let handle = tokio::spawn(async move { keepalive.keepalive_loop().await });
        track_task(&self.tasks, handle);

        info!(
            "✅ [{}] Network manager listening on {local}",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Shutdown the network manager
    ///
    /// Aborts every I/O task and drops all connections.
    ///
    /// # Errors
    ///
    /// This implementation never fails, but keeps the `Result` return type so
    /// that future transports can report errors.
    ///
    /// `async` by contract, like `start`: the actor awaits every transport's
    /// shutdown in the same place.
    #[allow(clippy::unused_async)]
    pub async fn shutdown(&mut self) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "🌐 [{}] Shutting down network manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }

        if let Ok(mut peers) = self.peers.lock() {
            peers.clear();
        }

        if let Ok(mut desired) = self.desired_peers.lock() {
            desired.clear();
        }

        if let Ok(mut outbox) = self.outbox.lock() {
            outbox.clear();
        }

        self.listener_addr = None;
        self.started = false;

        info!(
            "✅ [{}] Network manager shutdown completed",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Check if network is started
    #[must_use]
    pub const fn is_started(&self) -> bool {
        self.started
    }

    /// Get the current port
    ///
    /// After [`NetworkManager::start`] this reflects the port actually bound,
    /// which matters when port `0` was requested.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Address the listener is bound to, if started
    #[must_use]
    pub const fn local_addr(&self) -> Option<SocketAddr> {
        self.listener_addr
    }

    /// Number of currently connected peers
    #[must_use]
    pub fn connected_peers(&self) -> usize {
        self.peers.lock().map(|peers| peers.len()).unwrap_or(0)
    }

    /// Nicknames announced by the connected peers
    #[must_use]
    pub fn peer_nicknames(&self) -> Vec<String> {
        self.peers
            .lock()
            .map(|peers| {
                peers
                    .values()
                    .filter_map(|peer| peer.nickname.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The identities peers have announced, with a change flagged per nickname.
    ///
    /// This is what a front-end shows next to a nickname so a user can compare
    /// fingerprints with the peer out of band; the announced value is not verified
    /// (see [`crate::trust`]), which is exactly why the comparison is what makes it
    /// mean something.
    #[must_use]
    pub fn peer_identities(&self) -> Vec<PeerIdentity> {
        self.pins.identities()
    }

    /// Identities that changed under a nickname that was already pinned.
    #[must_use]
    pub fn pinned_identity_changes(&self) -> u64 {
        self.pins.changed()
    }

    /// Announcements that could not be pinned (unusable, or the table is full).
    #[must_use]
    pub fn pinned_identity_refusals(&self) -> u64 {
        self.pins.refused()
    }

    /// Set the identity announced to peers.
    ///
    /// The identity is not a secret in its *public* half (it is the DID every
    /// front-end prints); the secret half stays in the transport and is what the key
    /// agreement uses. It is announced to every connected peer right away, like the
    /// nickname, so a connection that was already up when the identity was set gains
    /// per-pair encryption without being re-established.
    pub async fn set_identity(&self, identity: &NetworkIdentity) {
        *self.identity.write().await = Some(identity.clone());

        let frame = self.shared().identity_frame().await;
        let senders: Vec<mpsc::Sender<Vec<u8>>> = self
            .peers
            .lock()
            .map(|peers| peers.values().map(|peer| peer.sender.clone()).collect())
            .unwrap_or_default();

        for sender in senders {
            queue_frame(&sender, frame.clone(), &self.frames_dropped);
        }
    }

    /// The key a directed message to `nickname` is sealed under, if the pair has one.
    ///
    /// `None` means "use the session key": the peer is not connected, it announced no
    /// identity, or this session has no identity of its own. Which key the pair has is
    /// decided by `pair_keys`: the **agreed** one when both ends announced an
    /// ephemeral for this connection, otherwise the static derivation, which is what a
    /// peer from before the agreement uses.
    ///
    /// A message that has to be buffered (the peer is away) therefore travels under
    /// the session key once it is flushed — there is no connection to agree a key on.
    /// The alternative would be to hold the plaintext, which is worse.
    #[must_use]
    pub fn contact_key(&self, nickname: &str) -> Option<Vec<u8>> {
        let wanted = nickname.trim().to_ascii_lowercase();

        // Both guards are scoped to the derivation and released before the caller
        // encrypts: the returned key is owned, and a `set_identity` arriving meanwhile
        // is never blocked by a send in progress.
        let keys = {
            let peers = self.peers.lock().ok()?;
            let peer = peers.values().find(|peer| {
                peer.nickname
                    .as_deref()
                    .is_some_and(|name| name.to_ascii_lowercase() == wanted)
            })?;
            let identity = self.identity.try_read().ok()?;
            let own = identity.as_ref()?;

            let keys = pair_keys(
                self.crypto.key(),
                own,
                Some(&peer.ephemeral),
                peer.identity.as_deref()?,
                peer.remote_ephemeral.as_deref(),
            );
            // Both guards end here: the derived key is owned, so a caller that encrypts
            // with it does not hold the peer table or the identity read lock.
            drop(identity);
            drop(peers);
            keys
        };

        keys.into_iter().next()
    }

    /// Set (or update) the nickname announced to peers
    ///
    /// The new name is pushed to every connected peer immediately.
    pub async fn set_nickname(&self, nickname: &str) {
        *self.nickname.write().await = nickname.to_string();

        let hello = self.shared().hello_frame().await;
        let senders: Vec<mpsc::Sender<Vec<u8>>> = self
            .peers
            .lock()
            .map(|peers| peers.values().map(|peer| peer.sender.clone()).collect())
            .unwrap_or_default();

        for sender in senders {
            queue_frame(&sender, hello.clone(), &self.frames_dropped);
        }
    }

    /// Get the configured bootstrap node addresses
    ///
    /// # Returns
    ///
    /// Returns the list of `host:port` bootstrap nodes used for peer discovery.
    #[must_use]
    pub fn bootstrap_nodes(&self) -> &[String] {
        &self.bootstrap_nodes
    }

    /// Get the maximum number of concurrent connections
    #[must_use]
    pub const fn max_connections(&self) -> u32 {
        self.max_connections
    }

    /// Connect to a single peer address (`host:port`)
    ///
    /// # Returns
    ///
    /// Returns the resolved remote address of the new connection.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] when the connection cannot be
    /// established within the configured timeout.
    pub async fn connect(&self, address: &str) -> MetaTextResult<SocketAddr> {
        // Explicitly requested peers become "desired" so the supervisor
        // re-establishes them if the connection drops.
        self.desire_peer(address);

        let stream = dial(address, self.connect_timeout).await?;

        let remote = stream.peer_addr().map_err(|error| MetaTextError::Network {
            message: format!("Failed to read the peer address: {error}"),
            operation: "peer_addr".to_string(),
            source: Some(Box::new(error)),
        })?;

        self.shared().register(stream, remote).await?;
        mark_resolved(&self.desired_peers, address, Some(remote));
        info!("🔗 Connected to peer {remote} ({address})");
        Ok(remote)
    }

    /// Connect to several peers, reporting (but not failing on) errors
    ///
    /// Addresses that cannot be reached are registered as desired peers and
    /// retried by the supervisor.
    ///
    /// # Returns
    ///
    /// Returns how many connections were established right away.
    pub async fn connect_all(&self, addresses: &[String]) -> usize {
        let mut connected = 0;
        for address in addresses {
            match self.connect(address).await {
                Ok(_) => connected += 1,
                Err(error) => note_failure(&self.desired_peers, address, &error),
            }
        }
        connected
    }

    /// Register `address` as a peer to keep connected
    ///
    /// The supervisor retries the address every `RECONNECT_INTERVAL` until a
    /// connection is established, and re-establishes it after a drop. Blank
    /// addresses are ignored.
    pub fn desire_peer(&self, address: impl Into<String>) {
        let address = address.into();
        if address.trim().is_empty() {
            return;
        }

        if let Ok(mut desired) = self.desired_peers.lock() {
            desired.entry(address).or_default();
        }
    }

    /// Addresses this manager is keeping connected, sorted for stable output
    #[must_use]
    pub fn desired_peers(&self) -> Vec<String> {
        let mut addresses: Vec<String> = self
            .desired_peers
            .lock()
            .map(|desired| desired.keys().cloned().collect())
            .unwrap_or_default();
        addresses.sort();
        addresses
    }

    /// How many desired peers are currently not connected
    #[must_use]
    pub fn pending_peers(&self) -> usize {
        self.desired_peers
            .lock()
            .map(|desired| {
                desired
                    .values()
                    .filter(|peer| peer.resolved.is_none())
                    .count()
            })
            .unwrap_or(0)
    }

    /// Encrypt `plaintext` and queue it for every connected peer
    ///
    /// # Returns
    ///
    /// Returns a [`DeliveryReceipt`] with the message id and the number of
    /// peers the frame was queued for.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if encryption fails, or
    /// [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn broadcast(
        &self,
        plaintext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<DeliveryReceipt> {
        let ciphertext = self.crypto.encrypt(plaintext)?;
        self.broadcast_ciphertext(&ciphertext, kind, content_type)
    }

    /// Queue an already encrypted payload for every connected peer
    ///
    /// Callers that encrypt themselves (for example to reuse the ciphertext
    /// for local statistics) use this to avoid encrypting twice.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn broadcast_ciphertext(
        &self,
        ciphertext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<DeliveryReceipt> {
        let senders = self.peer_senders(|_| true)?;
        Ok(self.dispatch_to(senders, ciphertext, kind, content_type))
    }

    /// Encrypt `plaintext` and send it to a single peer by nickname
    ///
    /// The nickname is compared case-insensitively against the names announced
    /// during the handshake. When no such peer is connected the message is
    /// buffered in that peer's outbox.
    ///
    /// A directed message is sealed under the **pair's** key when the connection has
    /// one (`pair_keys`: the agreed key, else the static derivation) and under the
    /// **session** key otherwise — a peer that has not identified itself, or one that
    /// is not connected at all. That is the same rule the core applies before it hands
    /// the payload over, so a caller of this convenience gets the documented
    /// behaviour rather than a session-key frame.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if encryption fails, or
    /// [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn send_to_peer(
        &self,
        nickname: &str,
        plaintext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<SendOutcome> {
        let ciphertext = match self.contact_key(nickname).as_deref() {
            Some(key) => self.crypto.encrypt_with(key, plaintext)?,
            None => self.crypto.encrypt(plaintext)?,
        };
        self.send_ciphertext_to_peer(nickname, &ciphertext, kind, content_type)
    }

    /// Send an already encrypted payload to a single peer by nickname
    ///
    /// When the peer is not connected the frame is queued and flushed by the
    /// transport as soon as a peer announcing that nickname connects, so a
    /// message sent while the other side is offline is not lost. Queues are
    /// bounded by `OUTBOX_CAPACITY` and entries expire after `OUTBOX_TTL`.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn send_ciphertext_to_peer(
        &self,
        nickname: &str,
        ciphertext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<SendOutcome> {
        let trimmed = nickname.trim();
        if trimmed.is_empty() {
            return Ok(SendOutcome::Dropped);
        }

        let wanted = trimmed.to_ascii_lowercase();
        let senders = self.peer_senders(|peer| {
            peer.nickname
                .as_deref()
                .is_some_and(|name| name.to_ascii_lowercase() == wanted)
        })?;

        if !senders.is_empty() {
            return Ok(SendOutcome::Sent(self.dispatch_to(
                senders,
                ciphertext,
                kind,
                content_type,
            )));
        }

        // Nobody is announcing that name right now: buffer the message.
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        Ok(self.enqueue(&wanted, message_id, ciphertext, kind, content_type))
    }

    /// Append a message to a nickname's outbox
    fn enqueue(
        &self,
        nickname: &str,
        message_id: u64,
        ciphertext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> SendOutcome {
        let Ok(mut outbox) = self.outbox.lock() else {
            return SendOutcome::Dropped;
        };

        let queue = outbox.entry(nickname.to_string()).or_default();
        if queue.len() >= OUTBOX_CAPACITY {
            return SendOutcome::Dropped;
        }

        queue.push_back(OutboxEntry {
            message_id,
            ciphertext: ciphertext.to_vec(),
            kind,
            content_type,
            queued_at: std::time::Instant::now(),
        });

        SendOutcome::Queued {
            message_id,
            position: queue.len(),
        }
    }

    /// Total number of messages waiting for a peer to connect
    #[must_use]
    pub fn queued_messages(&self) -> usize {
        self.outbox
            .lock()
            .map(|outbox| outbox.values().map(VecDeque::len).sum())
            .unwrap_or(0)
    }

    /// How many queued messages were dropped because they expired
    #[must_use]
    pub fn expired_messages(&self) -> u64 {
        self.outbox_expired.load(Ordering::SeqCst)
    }

    /// Frames shed because a peer's outbound queue was full.
    ///
    /// Non-zero means a connected peer stopped reading while the core kept sending:
    /// the frames were dropped rather than queued without bound (the queue holds
    /// `PEER_QUEUE_CAPACITY` frames). `/metrics` reports the same value as
    /// `payloads_dropped`, so the condition is observable instead of silent.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.frames_dropped.load(Ordering::SeqCst)
    }

    /// Greetings this instance refused because the announced nickname could not be shown.
    #[must_use]
    pub fn refused_greetings(&self) -> u64 {
        self.greetings_refused.load(Ordering::SeqCst)
    }

    /// Allocate an id, frame `ciphertext` and queue it for `senders`
    fn dispatch_to(
        &self,
        senders: Vec<mpsc::Sender<Vec<u8>>>,
        ciphertext: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> DeliveryReceipt {
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        let frame = encode_message_frame(message_id, ciphertext, kind, content_type);

        let mut peers = 0;
        for sender in senders {
            // A shed frame is *not* counted as a peer the ciphertext was queued for, so
            // the report says `queued_for: 0` ("queued for no peers connected") rather
            // than claiming a delivery that a full queue swallowed.
            if queue_frame(&sender, frame.clone(), &self.frames_dropped) {
                peers += 1;
            }
        }

        DeliveryReceipt { message_id, peers }
    }

    /// Clone the outbound queues of the peers matching `keep`
    ///
    /// The registry lock is released before the senders are used so that a
    /// slow peer cannot block the registry.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] if the peer registry is unusable.
    fn peer_senders(
        &self,
        keep: impl Fn(&Peer) -> bool,
    ) -> MetaTextResult<Vec<mpsc::Sender<Vec<u8>>>> {
        let peers = self.peers.lock().map_err(|_| MetaTextError::Network {
            message: "Peer registry lock poisoned".to_string(),
            operation: "send".to_string(),
            source: None,
        })?;

        Ok(peers
            .values()
            .filter(|peer| keep(peer))
            .map(|peer| peer.sender.clone())
            .collect())
    }

    /// Build the shared state used by the connection tasks
    fn shared(&self) -> Shared {
        Shared {
            crypto: Arc::clone(&self.crypto),
            event_sender: self.event_sender.clone(),
            nickname: Arc::clone(&self.nickname),
            identity: Arc::clone(&self.identity),
            pins: self.pins.clone(),
            peers: Arc::clone(&self.peers),
            tasks: Arc::clone(&self.tasks),
            desired_peers: Arc::clone(&self.desired_peers),
            outbox: Arc::clone(&self.outbox),
            outbox_expired: Arc::clone(&self.outbox_expired),
            max_connections: self.max_connections,
            connect_timeout: self.connect_timeout,
            handshake_timeout: self.handshake_timeout,
            idle_timeout: self.idle_timeout,
            keepalive_interval: self.keepalive_interval,
            frames_dropped: Arc::clone(&self.frames_dropped),
            greetings_refused: Arc::clone(&self.greetings_refused),
        }
    }
}

/// Resolve `address` and open a TCP connection
///
/// # Errors
///
/// Returns [`MetaTextError::Network`] on resolution, timeout or connection
/// failure.
async fn dial(address: &str, connect_timeout: Duration) -> MetaTextResult<TcpStream> {
    let stream = tokio::time::timeout(connect_timeout, TcpStream::connect(address))
        .await
        .map_err(|_| MetaTextError::Network {
            message: format!("Timed out connecting to {address}"),
            operation: "connect".to_string(),
            source: None,
        })?
        .map_err(|error| MetaTextError::Network {
            message: format!("Failed to connect to {address}: {error}"),
            operation: "connect".to_string(),
            source: Some(Box::new(error)),
        })?;

    Ok(stream)
}

/// Record the resolved remote address of a desired peer
fn mark_resolved(desired_peers: &DesiredPeers, address: &str, resolved: Option<SocketAddr>) {
    if let Ok(mut desired) = desired_peers.lock() {
        let entry = desired.entry(address.to_string()).or_default();
        entry.resolved = resolved;
        if resolved.is_some() {
            entry.attempts = 0;
        }
    }
}

/// Record a failed connection attempt, warning only on the first failure
fn note_failure(desired_peers: &DesiredPeers, address: &str, error: &MetaTextError) {
    let attempts = desired_peers
        .lock()
        .ok()
        .and_then(|mut desired| {
            desired.get_mut(address).map(|peer| {
                peer.attempts = peer.attempts.saturating_add(1);
                peer.attempts
            })
        })
        .unwrap_or(1);

    if attempts <= 1 {
        warn!("⚠️ Could not connect to {address}: {error} (retrying in the background)");
    } else {
        debug!("Could not connect to {address} (attempt {attempts}): {error}");
    }
}

impl Shared {
    /// Accept inbound connections until the listener is dropped
    async fn accept_loop(&self, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, remote)) => {
                    let connected = self.peers.lock().map(|peers| peers.len()).unwrap_or(0);
                    if connected >= self.max_connections as usize {
                        warn!(
                            "⚠️ Rejecting {remote}: connection limit of {} reached",
                            self.max_connections
                        );
                        continue;
                    }

                    if let Err(error) = self.register(stream, remote).await {
                        warn!("⚠️ Failed to register peer {remote}: {error}");
                    }
                }
                Err(error) => {
                    warn!("⚠️ Accept failed: {error}");
                    // Back off so a persistent error cannot spin the CPU.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Keep the desired peers connected, retrying on a fixed interval
    ///
    /// Runs until the task is aborted during shutdown.
    async fn supervise(&self, desired: DesiredPeers) {
        let mut ticker = tokio::time::interval(RECONNECT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; skip it so an explicit startup dial wins.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            // Drop queued messages that have been waiting too long.
            self.prune_outbox();

            let pending: Vec<String> = desired
                .lock()
                .map(|desired| {
                    desired
                        .iter()
                        .filter(|(_, peer)| peer.resolved.is_none())
                        .map(|(address, _)| address.clone())
                        .collect()
                })
                .unwrap_or_default();

            for address in pending {
                match dial(&address, self.connect_timeout).await {
                    Ok(stream) => match stream.peer_addr() {
                        Ok(remote) => match self.register(stream, remote).await {
                            Ok(()) => {
                                mark_resolved(&desired, &address, Some(remote));
                                info!("🔗 Connected to peer {remote} ({address})");
                            }
                            Err(error) => {
                                debug!("Could not register {address}: {error}");
                            }
                        },
                        Err(error) => debug!("Could not resolve remote for {address}: {error}"),
                    },
                    Err(error) => note_failure(&desired, &address, &error),
                }
            }
        }
    }

    /// Register a connected stream and start its reader/writer tasks
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] if the peer registry is unusable.
    async fn register(&self, stream: TcpStream, remote: SocketAddr) -> MetaTextResult<()> {
        // Chat traffic is latency sensitive and consists of small frames.
        if let Err(error) = stream.set_nodelay(true) {
            debug!("Could not disable Nagle for {remote}: {error}");
        }

        let (reader, writer) = tokio::io::split(stream);
        // Bounded: a peer that stops reading must not be able to make this queue
        // grow without limit (see `PEER_QUEUE_CAPACITY`).
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(PEER_QUEUE_CAPACITY);

        {
            let mut peers = self.peers.lock().map_err(|_| MetaTextError::Network {
                message: "Peer registry lock poisoned".to_string(),
                operation: "register".to_string(),
                source: None,
            })?;
            peers.insert(
                remote,
                Peer {
                    sender: sender.clone(),
                    nickname: None,
                    identity: None,
                    // One per connection: this is what makes the pair key of this
                    // connection unusable for any other, including a recorded replay
                    // of this handshake.
                    ephemeral: EphemeralKey::generate(),
                    remote_ephemeral: None,
                    keepalive_seen: false,
                },
            );
        }

        let read_shared = self.clone();
        track_task(
            &self.tasks,
            tokio::spawn(async move { read_shared.read_loop(reader, remote).await }),
        );

        let write_shared = self.clone();
        track_task(
            &self.tasks,
            tokio::spawn(async move { write_shared.write_loop(writer, receiver, remote).await }),
        );

        // Announce our nickname and identity immediately so the other side can show
        // the name and derive the pair's key.
        self.announce(remote).await;

        let _ = self
            .event_sender
            .send(AppEvent::NetworkEvent(NetworkEvent::PeerConnected {
                peer_id: remote.to_string(),
                metadata: HashMap::new(),
            }))
            .await;

        Ok(())
    }

    /// Encode our current nickname as a hello frame
    async fn hello_frame(&self) -> Vec<u8> {
        let nickname = self.nickname.read().await.clone();
        encode_frame(FRAME_HELLO, nickname.as_bytes())
    }

    /// Encode our session identity as an identity frame
    async fn identity_frame(&self) -> Vec<u8> {
        let identity = self
            .identity
            .read()
            .await
            .as_ref()
            .map_or_else(String::new, NetworkIdentity::public_hex);
        encode_frame(FRAME_IDENTITY, identity.as_bytes())
    }

    /// Announce the three halves of our greeting: nickname, identity, ephemeral key.
    ///
    /// The order matters only in that the nickname is what a peer matches an outbox
    /// against; the identity frame that follows carries the static key a peer pins,
    /// and the ephemeral frame is this connection's contribution to the key agreement.
    ///
    /// The ephemeral is sent here and nowhere else: it belongs to the connection, so a
    /// reconnect announces a fresh one, and re-announcing it (a nickname or identity
    /// change) would only pretend that an existing connection had been re-keyed.
    async fn announce(&self, remote: SocketAddr) {
        let _ = self.queue_to(remote, self.hello_frame().await);
        let identity = self.identity_frame().await;
        // An unset identity is sent as an empty frame, which the peer treats as "no
        // identity announced": it is one frame per connection either way, and it
        // means a peer that sets its identity later is understood without a restart.
        let _ = self.queue_to(remote, identity);
        if let Some(ephemeral) = self.ephemeral_frame(remote) {
            let _ = self.queue_to(remote, ephemeral);
        }
    }

    /// Encode this connection's ephemeral public key as a frame.
    ///
    /// The frame is built before the peer registry lock is released, and queued after:
    /// [`Shared::queue_to`] takes the same lock, so holding it across the call would
    /// deadlock.
    fn ephemeral_frame(&self, remote: SocketAddr) -> Option<Vec<u8>> {
        // The guard is released before the caller queues the frame: `queue_to` takes the
        // same lock, so holding it here would only delay the write.
        let frame = {
            let peers = self.peers.lock().ok()?;
            let peer = peers.get(&remote)?;
            let frame = encode_frame(FRAME_EPHEMERAL, peer.ephemeral.public_hex().as_bytes());
            drop(peers);
            frame
        };
        Some(frame)
    }
}

/// The pair keys a directed message to one peer may be sealed under, narrowest first.
///
/// The first entry is the key the two ends **agreed** ([`identity::pair_key`]): it
/// depends on a value only this connection holds, so a peer that has seen the
/// handshake — even one that holds the session key, as every member does — cannot
/// reproduce it. The second is the **static derivation** [`crypto::contact_key`],
/// which is what a peer from before the agreement (or one whose ephemeral
/// announcement was unusable) still uses.
///
/// The list is empty when the pair has no key of its own: the peer announced no
/// identity, or this session has none. The session key is then the answer, and it is
/// also the *last* candidate a receiver tries, because it is the only one that needs
/// no per-pair state at all.
///
/// # Arguments
///
/// * `session_key` - The session key, the HKDF salt of both derivations
/// * `own` - Our own identity (its secret half is what the agreement uses)
/// * `own_ephemeral` - The ephemeral pair of the connection, when there is one
/// * `peer_identity` - The static identity the peer announced
/// * `peer_ephemeral` - The ephemeral the peer announced for that connection
fn pair_keys(
    session_key: &[u8],
    own: &NetworkIdentity,
    own_ephemeral: Option<&EphemeralKey>,
    peer_identity: &str,
    peer_ephemeral: Option<&str>,
) -> Vec<Vec<u8>> {
    let own_public = own.public_hex();
    let mut keys = Vec::new();

    if let (Some(own_ephemeral), Some(peer_static), Some(peer_ephemeral)) = (
        own_ephemeral,
        identity::public_key_from_hex(peer_identity),
        peer_ephemeral.and_then(identity::public_key_from_hex),
    ) {
        match identity::pair_key(session_key, own, own_ephemeral, peer_static, peer_ephemeral) {
            Ok(key) => keys.push(key.to_vec()),
            Err(error) => debug!("Could not agree a pair key: {error}"),
        }
    }

    match crate::crypto::contact_key(session_key, &own_public, peer_identity) {
        Ok(key) => keys.push(key),
        Err(error) => debug!("Could not derive a static pair key: {error}"),
    }

    keys
}

/// Queue one frame for a connected peer, counting a shed frame.
///
/// `try_send` rather than `send().await`: the callers hold the peer registry (a
/// `std::sync::Mutex`), so they are on a synchronous path and must not block. A full
/// queue means the writer task cannot keep up with what the core produces — the frame
/// is dropped and counted, which `/metrics` reports as `payloads_dropped`. A closed
/// channel is *not* a shed but a dead writer, and neither case is reported as a
/// delivery: the caller's report then carries `queued_for: 0`, which a front-end
/// renders as "queued for no peers connected". The frame itself is lost — the
/// transport does not hold a copy — so the count and that report are the only trace.
fn queue_frame(sender: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>, dropped: &AtomicU64) -> bool {
    match sender.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::SeqCst);
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

/// I/O half of the peer connection, split out to keep the impl blocks small.
impl Shared {
    /// Read frames until the peer disconnects
    ///
    /// Two deadlines live here, and they answer two different questions.
    ///
    /// Until the peer has announced a nickname it is *useless*: it cannot be addressed
    /// by name, and nothing can be delivered to it. [`PEER_QUEUE_CAPACITY`] bounds what
    /// it can make us hold, and the **greeting deadline** bounds how long it can hold
    /// the slot itself — without it, a socket that connects and says nothing (or stalls
    /// mid-handshake) occupies one of `max_connections` forever, which is a slot the
    /// endpoint can never reuse.
    ///
    /// Once the peer has greeted, the question becomes liveness: TCP reports nothing on
    /// a read while no data arrives, so a peer that vanished without a FIN looks exactly
    /// like an idle one. The **idle deadline** ([`IDLE_TIMEOUT`]) drops a connection
    /// that has received nothing, which turns silence into evidence. It is armed only
    /// after the peer has *sent* a keepalive ([`FRAME_KEEPALIVE`]), because a peer built
    /// before that frame kind never sends one and would otherwise be dropped for being
    /// quiet — see the note on the constant.
    async fn read_loop(&self, mut reader: ReadHalf<TcpStream>, remote: SocketAddr) {
        let mut greeting_deadline = Some(tokio::time::Instant::now() + self.handshake_timeout);
        let mut last_received = tokio::time::Instant::now();
        let mut armed_with_keepalive = false;
        // Named in the disconnect event, so an operator can tell a peer that hung up
        // from one that was reclaimed.
        let mut reason = "connection closed";

        loop {
            let deadline = greeting_deadline
                .or_else(|| armed_with_keepalive.then(|| last_received + self.idle_timeout));

            let frame = match deadline {
                Some(at) => {
                    let Ok(frame) = tokio::time::timeout_at(at, read_frame(&mut reader)).await
                    else {
                        if greeting_deadline.is_some() {
                            warn!(
                                "⚠️ Peer {remote} did not announce itself within {:?}; reclaiming the slot",
                                self.handshake_timeout
                            );
                            reason = "no greeting within the deadline";
                        } else {
                            warn!(
                                "⚠️ Peer {remote} sent nothing for {:?}; dropping the connection",
                                self.idle_timeout
                            );
                            reason = "no traffic within the idle timeout";
                        }
                        break;
                    };
                    frame
                }
                None => read_frame(&mut reader).await,
            };

            match frame {
                Ok(Some((kind, payload))) => {
                    // Any frame is a liveness proof; the keepalive arm below only
                    // decides whether silence is *interpreted* as a fault.
                    last_received = tokio::time::Instant::now();
                    self.dispatch(kind, &payload, remote).await;
                    if greeting_deadline.is_some() && self.peer_greeted(remote) {
                        greeting_deadline = None;
                    }
                    if !armed_with_keepalive {
                        armed_with_keepalive = self.keepalive_seen(remote);
                    }
                }
                // Clean end of stream: the peer closed the connection.
                Ok(None) => break,
                Err(error) => {
                    debug!("Peer {remote} read error: {error}");
                    reason = "read error";
                    break;
                }
            }
        }

        if let Ok(mut peers) = self.peers.lock() {
            peers.remove(&remote);
        }

        // Let the supervisor know this desired peer needs reconnecting.
        if let Ok(mut desired) = self.desired_peers.lock() {
            for peer in desired.values_mut() {
                if peer.resolved == Some(remote) {
                    peer.resolved = None;
                }
            }
        }

        let _ = self
            .event_sender
            .send(AppEvent::NetworkEvent(NetworkEvent::PeerDisconnected {
                peer_id: remote.to_string(),
                reason: reason.to_string(),
            }))
            .await;
    }

    /// Write queued frames until the channel closes
    async fn write_loop(
        &self,
        mut writer: WriteHalf<TcpStream>,
        mut receiver: mpsc::Receiver<Vec<u8>>,
        remote: SocketAddr,
    ) {
        while let Some(frame) = receiver.recv().await {
            if let Err(error) = writer.write_all(&frame).await {
                debug!("Peer {remote} write error: {error}");
                break;
            }
        }
        let _ = writer.shutdown().await;
    }

    /// Handle a single inbound frame
    ///
    /// Asynchronous because the event channel applies backpressure: a peer
    /// flooding frames is throttled to the rate the core can absorb instead of
    /// growing a queue without bound.
    async fn dispatch(&self, kind: u8, payload: &[u8], remote: SocketAddr) {
        match kind {
            FRAME_HELLO => {
                let announced = String::from_utf8_lossy(payload);
                // A nickname is chosen by the *peer* and printed by a front-end, so it is
                // held to the same rule as every other label this instance displays
                // (`validation::nickname`: 1..=64 characters, no control character).
                // Nothing checked it before, which made the greeting the one place a peer
                // could put an escape sequence on the user's terminal — and a 64 KiB
                // "name" the key of the outbox, of the pin table, of a pane row and of a
                // log line.
                //
                // A greeting that fails the rule is refused, not repaired: the peer keeps
                // its connection and its frames, but it has no name, which is exactly the
                // state a peer that has not greeted yet is in (it is reached by address,
                // its inbound messages are attributed to `nickname_of`, and nothing can be
                // addressed *to* it). Repairing the string would display something the
                // peer did not send.
                let Some(nickname) = validation::peer_label(&announced) else {
                    self.greetings_refused.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "⚠️ Peer {remote} announced a nickname this instance cannot display \
                         ({} byte(s)); the connection stays up without a name for it",
                        payload.len()
                    );
                    return;
                };
                if let Ok(mut peers) = self.peers.lock() {
                    if let Some(peer) = peers.get_mut(&remote) {
                        peer.nickname = Some(nickname.clone());
                    }
                }
                info!("👤 Peer {remote} identifies as '{nickname}'");

                // The greeting order puts the nickname first, so an identity that
                // arrived before it had no name to be pinned under; pin it now.
                if let Some(identity) = self.peer_identity(remote) {
                    self.pin_identity(&nickname, &identity);
                }

                // Deliver anything that was written while this peer was away.
                let flushed = self.flush_outbox(&nickname, remote);
                if flushed > 0 {
                    info!("📬 Flushed {flushed} queued message(s) to '{nickname}'");
                }

                let _ = self
                    .event_sender
                    .send(AppEvent::NetworkEvent(NetworkEvent::PeerConnected {
                        peer_id: remote.to_string(),
                        metadata: HashMap::from([("nickname".to_string(), nickname)]),
                    }))
                    .await;
            }
            FRAME_IDENTITY => self.accept_identity(remote, payload),
            FRAME_EPHEMERAL => self.accept_ephemeral(remote, payload),
            frame @ (FRAME_MESSAGE | FRAME_ACTION | FRAME_BINARY) => {
                let Some((kind, content_type)) = message_attributes_of(frame) else {
                    return;
                };
                let Some(message_id) = decode_message_id(payload) else {
                    warn!("⚠️ Message frame without an id from {remote}");
                    return;
                };
                // `decode_message_id` has just proved the id prefix is present, so
                // this cannot fail; it is written as a checked split anyway, because
                // an index here would panic the connection's reader task if that
                // guard ever changed (peer control reaches this line).
                let Some(ciphertext) = payload.get(MESSAGE_ID_LEN..) else {
                    warn!("⚠️ Message frame shorter than its id from {remote}");
                    return;
                };

                match self.decrypt_from(remote, ciphertext) {
                    Ok(plaintext) => {
                        let _ = self
                            .event_sender
                            .send(AppEvent::MessageReceived {
                                peer: self.nickname_of(remote),
                                peer_id: remote.to_string(),
                                payload: plaintext,
                                kind,
                                content_type,
                            })
                            .await;

                        // Acknowledge receipt so the sender can report delivery.
                        let ack = encode_frame(FRAME_ACK, &message_id.to_be_bytes());
                        if !self.queue_to(remote, ack) {
                            debug!("Could not acknowledge message {message_id} to {remote}");
                        }
                    }
                    Err(error) => warn!("⚠️ Could not decrypt a message from {remote}: {error}"),
                }
            }
            FRAME_ACK => {
                let Some(message_id) = decode_message_id(payload) else {
                    warn!("⚠️ Ack frame without an id from {remote}");
                    return;
                };
                let _ = self
                    .event_sender
                    .send(AppEvent::MessageDelivered {
                        peer: self.nickname_of(remote),
                        peer_id: remote.to_string(),
                        message_id,
                    })
                    .await;
            }
            // Liveness proof; the frame carries nothing. Logged at `debug` because it
            // arrives every `KEEPALIVE_INTERVAL` for as long as the pair is up.
            FRAME_KEEPALIVE => {
                if let Ok(mut peers) = self.peers.lock() {
                    if let Some(peer) = peers.get_mut(&remote) {
                        peer.keepalive_seen = true;
                    }
                }
                debug!("💓 Keepalive from {remote}");
            }
            other => warn!("⚠️ Unknown frame kind {other} from {remote}"),
        }
    }

    /// Nickname announced by the peer, or an empty string before the handshake
    fn nickname_of(&self, remote: SocketAddr) -> String {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).and_then(|peer| peer.nickname.clone()))
            .unwrap_or_default()
    }

    /// Decrypt a frame from `remote` under the key it was written with.
    ///
    /// A directed message arrives under one of the pair's keys; a broadcast — and
    /// anything from a peer that announced no identity — under the session key. The
    /// candidates are tried narrowest first ([`Shared::connection_keys`]), and the
    /// session key last, which is the documented fallback that keeps a mixed-version
    /// pair (and a payload buffered before the peer was reachable) talking.
    ///
    /// # Errors
    ///
    /// Returns the *session-key* failure when no key opens the frame, so the caller's
    /// message names the key it expected rather than the pair key it happened to try
    /// first.
    fn decrypt_from(&self, remote: SocketAddr, ciphertext: &[u8]) -> MetaTextResult<Vec<u8>> {
        for key in self.connection_keys(remote) {
            if let Ok(plaintext) = self.crypto.decrypt_with(&key, ciphertext) {
                return Ok(plaintext);
            }
        }
        self.crypto.decrypt(ciphertext)
    }

    /// The pair keys for one connection, narrowest first (see [`pair_keys`]).
    ///
    /// Empty when the pair has no key of its own: the peer is gone, it announced no
    /// identity, or this session has none. The guards are held for the derivation only,
    /// and released before the caller decrypts.
    fn connection_keys(&self, remote: SocketAddr) -> Vec<Vec<u8>> {
        let Ok(identity) = self.identity.try_read() else {
            return Vec::new();
        };
        let Some(own) = identity.as_ref() else {
            return Vec::new();
        };
        let Ok(peers) = self.peers.lock() else {
            return Vec::new();
        };
        let Some(peer) = peers.get(&remote) else {
            return Vec::new();
        };
        let Some(peer_identity) = peer.identity.as_deref() else {
            return Vec::new();
        };

        pair_keys(
            self.crypto.key(),
            own,
            Some(&peer.ephemeral),
            peer_identity,
            peer.remote_ephemeral.as_deref(),
        )
    }

    /// The identity a peer announced, if it did.
    fn peer_identity(&self, remote: SocketAddr) -> Option<String> {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).and_then(|peer| peer.identity.clone()))
    }

    /// Whether the peer has announced a nickname on this connection.
    ///
    /// This is what the greeting deadline in `read_loop` watches: a nickname is what
    /// makes a peer addressable ([`NetworkManager::peer_nicknames`]), so a connection
    /// that has not produced one yet is not yet useful.
    fn peer_greeted(&self, remote: SocketAddr) -> bool {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).map(|peer| peer.nickname.is_some()))
            .unwrap_or(false)
    }

    /// Whether the peer has sent a keepalive on this connection.
    ///
    /// What the idle deadline in `read_loop` is armed by: see [`FRAME_KEEPALIVE`].
    fn keepalive_seen(&self, remote: SocketAddr) -> bool {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).map(|peer| peer.keepalive_seen))
            .unwrap_or(false)
    }

    /// Queue a keepalive for every greeted peer, once per `keepalive_interval`.
    ///
    /// One task for the whole manager rather than a timer per connection: the frame is
    /// five bytes and the cadence is the same for everybody, so a per-connection task
    /// would only add bookkeeping. A peer that has not greeted yet is skipped — it is
    /// on the greeting deadline, and a keepalive would not make it addressable.
    async fn keepalive_loop(&self) {
        let mut ticker = tokio::time::interval(self.keepalive_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick completes immediately; skip it so startup is not followed by
        // a keepalive before a single peer has connected.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            self.send_keepalives();
        }
    }

    /// Queue one keepalive per greeted peer.
    ///
    /// A shed keepalive counts like any other shed frame: the queue it could not enter
    /// is full, which is the condition `payloads_dropped` exists to report.
    fn send_keepalives(&self) {
        let frame = encode_frame(FRAME_KEEPALIVE, &[]);
        let senders: Vec<mpsc::Sender<Vec<u8>>> = match self.peers.lock() {
            Ok(peers) => peers
                .values()
                .filter(|peer| peer.nickname.is_some())
                .map(|peer| peer.sender.clone())
                .collect(),
            Err(_) => return,
        };

        for sender in senders {
            queue_frame(&sender, frame.clone(), &self.frames_dropped);
        }
    }

    /// The ephemeral key a peer announced for this connection, if it did.
    fn peer_ephemeral(&self, remote: SocketAddr) -> Option<String> {
        self.peers.lock().ok().and_then(|peers| {
            peers
                .get(&remote)
                .and_then(|peer| peer.remote_ephemeral.clone())
        })
    }

    /// Record the identity a peer announced, and pin it under its nickname.
    ///
    /// The identity travels in the clear, like the nickname: it is a *public* value
    /// whose secret half never leaves its owner. What it is good for is being pinned —
    /// a change under a known nickname is reported, see [`crate::trust`] — and for
    /// being one input of the key agreement; on its own it does not authenticate the
    /// peer, which is what the out-of-band fingerprint comparison is for.
    ///
    /// A peer may announce its identity before its nickname (the greeting is ours to
    /// order, not theirs), in which case there is no name to pin it under yet; the
    /// `hello` frame pins it then.
    fn accept_identity(&self, remote: SocketAddr, payload: &[u8]) {
        let identity = String::from_utf8_lossy(payload).trim().to_string();
        if let Ok(mut peers) = self.peers.lock() {
            if let Some(peer) = peers.get_mut(&remote) {
                peer.identity = (!identity.is_empty()).then(|| identity.clone());
            }
        }

        if identity.is_empty() {
            debug!("Peer {remote} announced no identity; using the session key");
            return;
        }
        debug!(
            "🔑 Peer {remote} announced identity {}; its frames use a pair key",
            crate::utils::abbreviate(&identity)
        );

        let nickname = self.nickname_of(remote);
        if nickname.is_empty() {
            debug!(
                "📌 Peer {remote} announced an identity before its nickname; \
                 it will be pinned once the nickname arrives"
            );
        } else {
            self.pin_identity(&nickname, &identity);
        }
    }

    /// Record the ephemeral key a peer announced for this connection.
    ///
    /// The ephemeral is a public value too, and the *secret* half of it is what makes
    /// the pair key an agreement instead of a derivation. A malformed announcement is
    /// treated as no announcement: the pair then keeps the static key it had, which is
    /// the same downgrade a peer could cause by not sending the frame at all.
    ///
    /// Only the **first** usable announcement of a connection is taken. A second frame
    /// would re-key a connection the peer is still using the first key on, so it is
    /// ignored: an exchange whose failure modes are closed rather than quiet is the
    /// point of the agreement, and a tampered frame then costs the pair key (the frames
    /// from that peer stop decrypting, visibly) instead of silently changing it.
    fn accept_ephemeral(&self, remote: SocketAddr, payload: &[u8]) {
        let announced = String::from_utf8_lossy(payload).trim().to_string();
        let parsed = identity::public_key_from_hex(&announced).map(hex::encode_upper);
        if parsed.is_none() && !announced.is_empty() {
            warn!(
                "⚠️ Peer {remote} announced an unusable ephemeral key; the pair keeps its \
                 static key"
            );
        }

        let stored = {
            let Ok(mut peers) = self.peers.lock() else {
                return;
            };
            let Some(peer) = peers.get_mut(&remote) else {
                return;
            };
            if peer.remote_ephemeral.is_none() {
                peer.remote_ephemeral = parsed;
                true
            } else {
                false
            }
        };

        if !stored {
            debug!("🔁 Peer {remote} announced a second ephemeral key; the first one is kept");
            return;
        }
        if let Some(ephemeral) = self.peer_ephemeral(remote) {
            debug!(
                "🤝 Peer {remote} announced ephemeral {}; the pair key is now agreed",
                crate::utils::abbreviate(&ephemeral)
            );
        }
    }

    /// Record the identity a peer announced under the nickname it announced.
    ///
    /// Every outcome is handled inside the store: a first sighting is written to the
    /// pin file, an unchanged identity does nothing, a change under a known nickname
    /// is warned about (with both fingerprints) and counted, and an announcement
    /// that cannot be pinned is counted — see [`crate::trust`]. The transport only
    /// has to feed it the announcement.
    fn pin_identity(&self, nickname: &str, public_key: &str) {
        let _ = self.pins.observe(nickname, public_key);
    }

    /// Queue a frame for an already known peer
    ///
    /// # Returns
    ///
    /// Returns `true` when the frame was queued for writing.
    fn queue_to(&self, remote: SocketAddr, frame: Vec<u8>) -> bool {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).map(|peer| peer.sender.clone()))
            .is_some_and(|sender| queue_frame(&sender, frame, &self.frames_dropped))
    }

    /// Send every message queued for `nickname` to `remote`
    ///
    /// Expired entries are discarded on the way. Messages that cannot be queued
    /// (because the peer vanished) stay in the outbox for the next attempt.
    ///
    /// # Returns
    ///
    /// Returns how many messages were handed to the peer.
    fn flush_outbox(&self, nickname: &str, remote: SocketAddr) -> usize {
        let key = nickname.to_ascii_lowercase();
        let now = std::time::Instant::now();

        let mut entries = {
            let Ok(mut outbox) = self.outbox.lock() else {
                return 0;
            };

            let queue = outbox.remove(&key).unwrap_or_default();
            let (fresh, expired): (VecDeque<OutboxEntry>, VecDeque<OutboxEntry>) = queue
                .into_iter()
                .partition(|entry| now.duration_since(entry.queued_at) < OUTBOX_TTL);

            if !expired.is_empty() {
                self.outbox_expired
                    .fetch_add(expired.len() as u64, Ordering::SeqCst);
            }

            fresh
        };

        let mut flushed = 0;
        let mut remaining: VecDeque<OutboxEntry> = VecDeque::new();
        while let Some(entry) = entries.pop_front() {
            let frame = encode_message_frame(
                entry.message_id,
                &entry.ciphertext,
                entry.kind,
                entry.content_type,
            );
            if self.queue_to(remote, frame) {
                flushed += 1;
            } else {
                // The peer vanished: keep this and every later message.
                remaining.push_back(entry);
                remaining.append(&mut entries);
                break;
            }
        }

        if !remaining.is_empty() {
            if let Ok(mut outbox) = self.outbox.lock() {
                outbox.entry(key).or_default().extend(remaining);
            }
        }

        flushed
    }

    /// Drop queued messages whose time to live has expired
    fn prune_outbox(&self) {
        let now = std::time::Instant::now();
        let mut dropped = 0_u64;

        if let Ok(mut outbox) = self.outbox.lock() {
            outbox.retain(|_, queue| {
                let before = queue.len();
                queue.retain(|entry| now.duration_since(entry.queued_at) < OUTBOX_TTL);
                dropped += (before - queue.len()) as u64;
                !queue.is_empty()
            });
        }

        if dropped > 0 {
            self.outbox_expired.fetch_add(dropped, Ordering::SeqCst);
        }
    }
}

/// Encode a frame as `[kind][u32 big-endian length][payload]`
fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    // Saturating conversion keeps the length prefix well defined even for an
    // absurdly large payload; the reader rejects anything above the limit.
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Encode a message frame: `[MESSAGE][len][u64 id][ciphertext]`
/// Encode a message frame, tagged with its kind and content type.
fn encode_message_frame(
    message_id: u64,
    ciphertext: &[u8],
    kind: MessageKind,
    content_type: ContentType,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(MESSAGE_ID_LEN + ciphertext.len());
    payload.extend_from_slice(&message_id.to_be_bytes());
    payload.extend_from_slice(ciphertext);

    // `(kind, content_type)` maps to one frame kind; a binary action is not a
    // meaningful combination and is rejected before it reaches this function.
    let frame = match (kind, content_type) {
        (MessageKind::Text, ContentType::Text) => FRAME_MESSAGE,
        (MessageKind::Action, _) => FRAME_ACTION,
        (MessageKind::Text, ContentType::Binary) => FRAME_BINARY,
    };
    encode_frame(frame, &payload)
}

/// Map a frame kind back to the message attributes it carries.
const fn message_attributes_of(frame: u8) -> Option<(MessageKind, ContentType)> {
    match frame {
        FRAME_MESSAGE => Some((MessageKind::Text, ContentType::Text)),
        FRAME_ACTION => Some((MessageKind::Action, ContentType::Text)),
        FRAME_BINARY => Some((MessageKind::Text, ContentType::Binary)),
        _ => None,
    }
}

/// Decode the 8 byte message identifier prefix of a frame payload
///
/// # Returns
///
/// Returns `None` when the payload is shorter than [`MESSAGE_ID_LEN`].
fn decode_message_id(payload: &[u8]) -> Option<u64> {
    let bytes = payload.get(..MESSAGE_ID_LEN)?;
    let mut id = [0_u8; MESSAGE_ID_LEN];
    id.copy_from_slice(bytes);
    Some(u64::from_be_bytes(id))
}

/// Read one frame
///
/// # Returns
///
/// Returns `Ok(None)` on a clean end of stream.
///
/// # Errors
///
/// Returns an I/O error on transport failures, or one carrying
/// [`std::io::ErrorKind::InvalidData`] when a peer announces an oversized
/// frame.
async fn read_frame(reader: &mut ReadHalf<TcpStream>) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0_u8; 1];
    match reader.read_exact(&mut kind).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length);

    // Validate before allocating: the length prefix is peer controlled.
    if length > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {length} bytes exceeds the {MAX_FRAME_SIZE} byte limit"),
        ));
    }

    let mut payload = vec![0_u8; length as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some((kind[0], payload)))
}

/// Store a spawned task so it can be aborted on shutdown
fn track_task(
    tasks: &Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    handle: tokio::task::JoinHandle<()>,
) {
    match tasks.lock() {
        Ok(mut tasks) => tasks.push(handle),
        Err(_) => handle.abort(),
    }
}

/// Bind the inbound listener according to `enable_ipv6`.
///
/// With IPv6 enabled this is a **dual-stack** wildcard: one `[::]` socket with
/// `IPV6_V6ONLY` cleared, so both families reach the same port. `socket2` is used
/// because `std` cannot clear that option and a default IPv6 wildcard is
/// IPv6-only on Windows — which would silently stop the transport from accepting
/// its IPv4 peers. A host with no usable IPv6 stack falls back to the IPv4
/// wildcard rather than refusing to start.
///
/// # Errors
///
/// Returns [`MetaTextError::Network`] when neither the dual-stack nor the IPv4
/// socket can be bound.
fn bind_listener(port: u16, enable_ipv6: bool) -> MetaTextResult<std::net::TcpListener> {
    if enable_ipv6 {
        match dual_stack_listener(port) {
            Ok(listener) => return Ok(listener),
            Err(error) => warn!(
                "⚠️ IPv6 is enabled but a dual-stack listener could not be created \
                 ({error}); falling back to the IPv4 wildcard"
            ),
        }
    }

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener =
        std::net::TcpListener::bind(bind_addr).map_err(|error| MetaTextError::Network {
            message: format!("Failed to bind {bind_addr}: {error}"),
            operation: "bind".to_string(),
            source: Some(Box::new(error)),
        })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| MetaTextError::Network {
            message: format!("Failed to make the listener non-blocking: {error}"),
            operation: "bind".to_string(),
            source: Some(Box::new(error)),
        })?;
    Ok(listener)
}

/// Build the dual-stack IPv6 wildcard socket described on [`bind_listener`].
fn dual_stack_listener(port: u16) -> std::io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    // Cleared so the socket also accepts IPv4-mapped peers. Best effort: a kernel
    // without the option (or with IPv6 disabled) is handled by the caller's
    // fallback instead of failing the transport.
    socket.set_only_v6(false)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;

    let bind_addr = SocketAddr::from(([0_u16, 0, 0, 0, 0, 0, 0, 0], port));
    socket.bind(&bind_addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkConfig;

    /// Build a loopback network config with an OS-assigned port
    fn loopback_config() -> NetworkConfig {
        NetworkConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            connection_timeout: 5,
            max_connections: 8,
            // The shipped cadence: tests that need another one either build the config
            // through this field or override the manager's timing (see
            // `manager_with_liveness`).
            keepalive_interval: crate::config::DEFAULT_KEEPALIVE_INTERVAL,
            enable_upnp: false,
            enable_ipv6: false,
        }
    }

    /// A manager using a passphrase derived key, plus its event receiver
    async fn test_manager(passphrase: &str) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        test_manager_on(passphrase, 0).await
    }

    /// Like [`test_manager`] but bound to a specific port
    async fn test_manager_on(
        passphrase: &str,
        port: u16,
    ) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(&crate::config::CryptoConfig::default(), passphrase)
                .expect("derive key"),
        );
        let (sender, receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.port = port;

        let manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");
        (manager, receiver)
    }

    /// Wait until `condition` holds or the deadline expires
    async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        condition()
    }

    /// A manager whose dial and greeting deadlines are `seconds` long.
    ///
    /// The tests below need a deadline they can wait out; the default 30 s would make
    /// them slow, and the point is the *existence* of the deadline, not its value.
    async fn manager_with_timeouts(seconds: u64) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(
                &crate::config::CryptoConfig::default(),
                "deadline-test",
            )
            .expect("derive key"),
        );
        let (sender, receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.connection_timeout = seconds;

        let manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");
        (manager, receiver)
    }

    /// A peer that connects and says nothing loses its slot when the greeting
    /// deadline expires.
    ///
    /// This is the pre-authentication half of "one connection cannot occupy the
    /// endpoint forever". Before the deadline existed the connection stayed in the
    /// registry until the socket closed, so `max_connections` silent sockets — no
    /// passphrase, no greeting, no traffic — were enough to make the instance
    /// unreachable to everybody else. The peer here is a raw socket that sends
    /// nothing, which is exactly that fault.
    #[tokio::test]
    async fn test_a_peer_that_never_greets_loses_its_slot() {
        let (mut manager, _receiver) = manager_with_timeouts(1).await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let _silent = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        assert!(
            wait_for(|| manager.connected_peers() == 1).await,
            "the connection must be registered before it can be reclaimed"
        );

        // The deadline is one second; `wait_for` allows six, which absorbs a loaded
        // machine without hiding a missing deadline (that would never release).
        assert!(
            wait_for(|| manager.connected_peers() == 0).await,
            "a peer that never announced itself must lose its slot"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A peer that stops reading cannot make the transport hold an unbounded number
    /// of frames: the queue sheds, the shed is counted, and the first shed happens
    /// only once the queue is genuinely full.
    ///
    /// The frames are large on purpose. The kernel's socket buffer has to fill before
    /// the writer task blocks, and only then does the queue itself fill, so small
    /// frames would need thousands of iterations to reach the bound. The loop is
    /// allowed eight times the capacity because how much the kernel absorbs before
    /// the writer blocks depends on the host's send buffer and the peer's advertised
    /// window; the assertion below is what has to hold on every host, and it is
    /// stricter than "a shed happened at some point": it pins *where* the shedding
    /// starts, which is what proves the capacity — not the producer — bounds memory.
    #[tokio::test]
    async fn test_a_peer_that_stops_reading_sheds_instead_of_growing() {
        // A long greeting deadline: this test is about the queue, not the deadline.
        let (mut manager, _receiver) = manager_with_timeouts(60).await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        // Connected, and never read from.
        let _silent = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        assert!(wait_for(|| manager.connected_peers() == 1).await);

        let payload = vec![0xAB_u8; 16 * 1024];
        let mut shed_after = None;
        for attempt in 0..(PEER_QUEUE_CAPACITY * 8) {
            manager
                .broadcast_ciphertext(&payload, MessageKind::Text, ContentType::Text)
                .expect("broadcast");
            if manager.dropped_frames() > 0 {
                shed_after = Some(attempt);
                break;
            }
        }

        let shed_after =
            shed_after.expect("a peer that never reads must not grow its queue without bound");
        assert!(
            shed_after >= PEER_QUEUE_CAPACITY,
            "the queue shed after {shed_after} frames, before its {PEER_QUEUE_CAPACITY} frame \
             capacity was reached: the bound is not what limits growth"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A message body reaches the core **verbatim**: which bodies may be shown is the
    /// core's decision, not the transport's.
    ///
    /// The two layers have one job each. The transport carries bytes: it must not edit a
    /// payload (a sanitising transport would hide the fact from the layer that has to
    /// report it) and must not drop one quietly either. The core is where the rule lives
    /// — `core::body_of` refuses a body that is not UTF-8 or that carries a control
    /// character — and this test pins the boundary from below, by writing a frame by hand
    /// the way a peer (or another implementation) would: the escape sequence arrives
    /// intact as `AppEvent::MessageReceived`, so the refusal is provably *not* happening
    /// here.
    #[tokio::test]
    async fn test_a_message_body_reaches_the_core_verbatim() {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(
                &crate::config::CryptoConfig::default(),
                "verbatim-body",
            )
            .expect("derive key"),
        );
        let (sender, mut events) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut manager = NetworkManager::new(
            &loopback_config(),
            Arc::clone(&crypto),
            sender,
            PinStore::volatile(),
        )
        .await
        .expect("network manager");
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let mut peer = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        send_raw_frame(&mut peer, FRAME_HELLO, b"Mallory").await;

        // A body the send path can never produce (`validation::message` refuses it), sent
        // the way an implementation that skipped that check would: message id then
        // ciphertext.
        let body: &[u8] = b"owned\x1b[2J";
        let ciphertext = crypto.encrypt(body).expect("encrypt");
        let mut payload = 1_u64.to_be_bytes().to_vec();
        payload.extend_from_slice(&ciphertext);
        send_raw_frame(&mut peer, FRAME_MESSAGE, &payload).await;

        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(AppEvent::MessageReceived { payload, .. }) => break payload,
                    Some(_) => {}
                    None => panic!("the event channel must stay open"),
                }
            }
        })
        .await
        .expect("the message event must arrive");

        assert_eq!(
            received, body,
            "the transport must carry the body unchanged; refusing it is the core's job"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A manager with explicit liveness timing.
    ///
    /// The production cadence (20 s / 60 s) would make these tests minutes long, and
    /// what is being tested is the mechanism rather than the constants.
    async fn manager_with_liveness(
        handshake_seconds: u64,
        keepalive: Duration,
        idle: Duration,
    ) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(
                &crate::config::CryptoConfig::default(),
                "liveness-test",
            )
            .expect("derive key"),
        );
        let (sender, receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.connection_timeout = handshake_seconds;

        let mut manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");
        manager.keepalive_interval = keepalive;
        manager.idle_timeout = idle;
        (manager, receiver)
    }

    /// Write one frame by hand, the way a peer on the wire would.
    async fn send_raw_frame(socket: &mut TcpStream, kind: u8, payload: &[u8]) {
        socket
            .write_all(&encode_frame(kind, payload))
            .await
            .expect("write frame");
    }

    /// Once a peer has proved it sends keepalives, silence is evidence of a dead
    /// connection: the slot is released and the disconnect names the reason.
    ///
    /// This is the half-open case a chat peer meets in the field — a machine that lost
    /// power, or a NAT that dropped the mapping, leaves a socket that looks perfectly
    /// healthy to a reader and never delivers another byte. Before the keepalive the
    /// connection was written to until the kernel's retransmit timer gave up.
    #[tokio::test]
    async fn test_a_silent_peer_is_dropped_once_it_has_proved_it_sends_keepalives() {
        let (mut manager, mut events) =
            manager_with_liveness(60, Duration::from_millis(50), Duration::from_millis(300)).await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let mut peer = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        // Greet, then send one keepalive: that is what *arms* the idle deadline.
        send_raw_frame(&mut peer, FRAME_HELLO, b"ghost").await;
        send_raw_frame(&mut peer, FRAME_KEEPALIVE, &[]).await;
        assert!(wait_for(|| manager.connected_peers() == 1).await);

        // Now say nothing at all, while the manager keeps writing keepalives into the
        // void. That is exactly what a peer that lost power looks like from here.
        assert!(
            wait_for(|| manager.connected_peers() == 0).await,
            "a peer that stopped sending must be dropped after the idle timeout"
        );

        // The disconnect carries the reason, so an operator can tell a reclaimed
        // half-open connection from a peer that hung up politely.
        let reason = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(AppEvent::NetworkEvent(NetworkEvent::PeerDisconnected {
                        reason, ..
                    })) => {
                        break reason;
                    }
                    Some(_) => {
                        // Not this test's subject (the connect, a message): keep
                        // reading until the disconnect arrives.
                    }
                    None => break String::new(),
                }
            }
        })
        .await
        .expect("the disconnect event must arrive");
        assert_eq!(reason, "no traffic within the idle timeout");
        manager.shutdown().await.expect("shutdown");
    }

    /// A peer that never sends a keepalive is *not* dropped for being quiet.
    ///
    /// This is the compatibility rule that makes the idle deadline safe to ship: a peer
    /// built before [`FRAME_KEEPALIVE`] ignores the kind (as it does for every late
    /// addition) and therefore never arms the deadline, so it keeps the behaviour it
    /// had. Half-open detection is a property of a pair where both ends are modern.
    #[tokio::test]
    async fn test_a_peer_that_never_sends_a_keepalive_is_not_dropped_for_silence() {
        let (mut manager, _events) =
            manager_with_liveness(60, Duration::from_millis(50), Duration::from_millis(300)).await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let mut peer = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        send_raw_frame(&mut peer, FRAME_HELLO, b"legacy").await;
        assert!(wait_for(|| manager.connected_peers() == 1).await);

        // Four idle timeouts of silence, and not one keepalive.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            manager.connected_peers(),
            1,
            "a peer from before the keepalive frame must not be dropped for silence"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A greeting that carries a name this instance cannot display is refused, not adopted.
    ///
    /// The nickname in `FRAME_HELLO` is chosen by the peer and printed by a front-end, so
    /// it is the one place a peer could put an escape sequence on the user's terminal —
    /// or make a 64 KiB string the key of the outbox, of the pin table, of a pane row and
    /// of a log line. Nothing checked it before: the frame was lossy-decoded and stored.
    /// Refusing the *label* is not refusing the *peer*: the connection stays up and its
    /// frames keep being read, but the peer has no name, exactly like one that has not
    /// greeted yet (it is reached by address, and nothing can be addressed to it).
    #[tokio::test]
    async fn test_a_greeting_with_an_unusable_nickname_is_refused() {
        let (mut manager, _events) = test_manager("greeting-label").await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        // First peer: a name a front-end must not print.
        let mut hostile = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        send_raw_frame(&mut hostile, FRAME_HELLO, b"owned\x1b[2J").await;
        assert!(wait_for(|| manager.connected_peers() == 1).await);
        assert!(
            manager.peer_nicknames().is_empty(),
            "an unusable greeting must leave the peer nameless: {:?}",
            manager.peer_nicknames()
        );

        // A greeting longer than the label rule allows is refused for the same reason.
        let mut oversized = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        send_raw_frame(&mut oversized, FRAME_HELLO, &[b'a'; 512]).await;
        assert!(wait_for(|| manager.connected_peers() == 2).await);
        assert!(
            manager.peer_nicknames().is_empty(),
            "a 512 byte greeting must not become a name: {:?}",
            manager.peer_nicknames()
        );

        // A well-formed greeting on the same manager still works, so the rule refuses the
        // label rather than the connection or the mechanism.
        let mut friendly = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        send_raw_frame(&mut friendly, FRAME_HELLO, b"  Alice  ").await;
        assert!(wait_for(|| manager.peer_nicknames() == vec!["Alice".to_string()]).await);

        // Two refusals, and the good one did not add to them: the count is what
        // `/metrics` reports, so a peer that reconnects with the same bad greeting is
        // visible without reading the log.
        assert_eq!(manager.refused_greetings(), 2);

        manager.shutdown().await.expect("shutdown");
    }

    /// A greeted peer is *sent* keepalives: the sending half of the mechanism.
    ///
    /// The two tests above only exercise the receiving half (what silence means). This
    /// one reads the wire: after the handshake the manager has nothing to say, and the
    /// keepalive is the only thing that arrives.
    #[tokio::test]
    async fn test_a_greeted_peer_is_sent_keepalives() {
        let (mut manager, _events) =
            manager_with_liveness(60, Duration::from_millis(50), Duration::from_secs(30)).await;
        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let peer = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        let (mut reader, mut writer) = tokio::io::split(peer);
        writer
            .write_all(&encode_frame(FRAME_HELLO, b"watchful"))
            .await
            .expect("greet");

        // The manager answers with its own greeting and then goes quiet; a keepalive is
        // the only frame that keeps arriving.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen_keepalive = false;
        while let Ok(Ok(Some((kind, _)))) =
            tokio::time::timeout_at(deadline, read_frame(&mut reader)).await
        {
            if kind == FRAME_KEEPALIVE {
                seen_keepalive = true;
                break;
            }
        }

        assert!(
            seen_keepalive,
            "a greeted peer must receive keepalives on the configured cadence"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A manager built from a configuration asking for a faster probe honours it — and
    /// does not shorten its own deadline to match.
    ///
    /// Two halves of one rule, and they are the reason the configuration key is one-way.
    /// The *cadence* is what `[network] keepalive_interval` tunes: a NAT that forgets an
    /// idle mapping sooner than the default 20 s needs traffic more often, so the frame
    /// must appear on the configured cadence. The read loop below gives up after five
    /// seconds, which is far less than the default cadence — so a manager that ignored
    /// the configured value would see nothing at all. The *deadline* is what it must not
    /// tune: it bounds the peer's silence and the peer may be a default one probing every
    /// 20 s, so a shortened local cadence must leave it at three default cadences.
    #[tokio::test]
    async fn test_a_configured_keepalive_cadence_is_honoured_without_shortening_the_deadline() {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(&crate::config::CryptoConfig::default(), "cadence-test")
                .expect("derive key"),
        );
        let (sender, _events) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.keepalive_interval = 1;

        let mut manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");
        assert_eq!(
            manager.keepalive_interval,
            Duration::from_secs(1),
            "the configured cadence must reach the manager"
        );
        assert_eq!(
            manager.idle_timeout, IDLE_TIMEOUT,
            "a faster probe must not shorten the deadline: the peer may be a default one"
        );

        manager.start().await.expect("start");
        let bound = manager.local_addr().expect("bound address");

        let peer = TcpStream::connect(("127.0.0.1", bound.port()))
            .await
            .expect("connect");
        let (mut reader, mut writer) = tokio::io::split(peer);
        writer
            .write_all(&encode_frame(FRAME_HELLO, b"fast-probe"))
            .await
            .expect("greet");

        // Five seconds: enough for a one second cadence on a loaded machine, and nowhere
        // near enough for the twenty second default.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen_keepalive = false;
        while let Ok(Ok(Some((kind, _)))) =
            tokio::time::timeout_at(deadline, read_frame(&mut reader)).await
        {
            if kind == FRAME_KEEPALIVE {
                seen_keepalive = true;
                break;
            }
        }

        assert!(
            seen_keepalive,
            "a one second cadence must be visible on the wire well inside the default one"
        );
        manager.shutdown().await.expect("shutdown");
    }

    /// A manager built without validation falls back to the shipped cadence.
    ///
    /// `AppConfig::problems` rejects `0`, but `NetworkManager::new` is public and callers
    /// build their configuration in memory (every integration test does), so the value
    /// cannot be assumed to have been validated. A zero interval would mean "never
    /// probe", which silently reopens the half-open hole the keepalive closed, so it
    /// lands on the default cadence instead — and with it the default deadline.
    #[tokio::test]
    async fn test_a_zero_keepalive_interval_falls_back_to_the_shipped_cadence() {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(
                &crate::config::CryptoConfig::default(),
                "cadence-fallback",
            )
            .expect("derive key"),
        );
        let (sender, _events) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.keepalive_interval = 0;

        let manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");

        assert_eq!(manager.keepalive_interval, KEEPALIVE_INTERVAL);
        assert_eq!(
            manager.keepalive_interval,
            Duration::from_secs(crate::config::DEFAULT_KEEPALIVE_INTERVAL),
            "the fallback must be the same number the configuration default uses"
        );
        assert_eq!(manager.idle_timeout, IDLE_TIMEOUT);
    }

    /// Like [`test_manager_on`] but with a caller supplied pin store, so a test can
    /// watch what the transport pins for a peer.
    async fn test_manager_with_pins(
        passphrase: &str,
        pins: PinStore,
    ) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(&crate::config::CryptoConfig::default(), passphrase)
                .expect("derive key"),
        );
        let (sender, receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let manager = NetworkManager::new(&loopback_config(), crypto, sender, pins)
            .await
            .expect("network manager");
        (manager, receiver)
    }

    /// An announced identity is pinned under the nickname that announced it, and a
    /// different identity under that nickname is reported rather than passing
    /// unnoticed.
    ///
    /// This is the transport half of the identity work: the store has its own tests,
    /// and this one proves the transport actually feeds it — a pin that nothing
    /// reported would look identical to a working one.
    #[tokio::test]
    async fn test_a_peer_identity_is_pinned_and_a_change_is_reported() {
        let passphrase = "pin passphrase";
        // Alice keeps the store; Bob only needs to announce an identity.
        let pins = PinStore::volatile();
        let (mut alice, _alice_events) = test_manager_with_pins(passphrase, pins.clone()).await;
        let (mut bob, _bob_events) = test_manager_with_pins(passphrase, PinStore::volatile()).await;

        let alice_identity = NetworkIdentity::generate();
        let bob_identity = NetworkIdentity::generate();
        alice.set_nickname("Alice").await;
        alice.set_identity(&alice_identity).await;
        bob.set_nickname("Bob").await;
        bob.set_identity(&bob_identity).await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        let alice_address = alice.local_addr().unwrap();
        bob.connect(&alice_address.to_string()).await.unwrap();

        assert!(
            wait_for(|| pins.pinned("Bob").is_some()).await,
            "Bob's announced identity must be pinned"
        );
        assert_eq!(pins.pinned("Bob"), Some(bob_identity.public_hex()));
        assert_eq!(pins.changed(), 0, "a first sighting is not a change");

        let views = pins.identities();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].nickname, "bob", "pins are keyed by nickname");
        assert!(!views[0].changed);
        assert_eq!(views[0].fingerprint, bob_identity.fingerprint());

        // The same peer, the same nickname, a different identity.
        let replaced = NetworkIdentity::generate();
        bob.set_identity(&replaced).await;
        assert!(
            wait_for(|| pins.changed() == 1).await,
            "a changed identity must be reported"
        );
        let views = pins.identities();
        assert_eq!(views[0].public_key, replaced.public_hex());
        assert!(views[0].changed, "the change is flagged for the peer");

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// The key ladder a directed message follows: the agreed key first, the static
    /// derivation last, and nothing when the pair has no identity to work from.
    ///
    /// This is the compatibility story as a unit test: a peer from before the key
    /// agreement (no ephemeral), a peer whose ephemeral was unusable, and a peer that
    /// announced no identity at all each land on the documented key.
    #[test]
    fn test_the_pair_key_ladder() {
        let session = CryptoManager::generate_key();
        let own = NetworkIdentity::generate();
        let peer = NetworkIdentity::generate();
        let own_ephemeral = EphemeralKey::generate();
        let peer_ephemeral = EphemeralKey::generate();
        let peer_ephemeral_hex = peer_ephemeral.public_hex();

        let agreed = crate::identity::pair_key(
            &session,
            &own,
            &own_ephemeral,
            peer.public_key(),
            peer_ephemeral.public_key(),
        )
        .expect("a pair key");
        let static_key =
            crate::crypto::contact_key(&session, &own.public_hex(), &peer.public_hex())
                .expect("a static pair key");

        // Both ends announced an ephemeral: the agreed key is preferred, and the static
        // one stays a candidate for a frame written before the agreement completed.
        let both = pair_keys(
            &session,
            &own,
            Some(&own_ephemeral),
            &peer.public_hex(),
            Some(&peer_ephemeral_hex),
        );
        assert_eq!(both.len(), 2);
        assert_eq!(both[0], agreed.to_vec());
        assert_eq!(both[1], static_key);
        assert_ne!(both[0], both[1], "the two derivations are domain-separated");

        // A peer from before the agreement announced no ephemeral: the static
        // derivation is all there is, which is exactly what it used before.
        let legacy = pair_keys(
            &session,
            &own,
            Some(&own_ephemeral),
            &peer.public_hex(),
            None,
        );
        assert_eq!(legacy, vec![static_key.clone()]);

        // An announcement that is not a key is treated as no announcement, so a peer
        // cannot pick a *weaker* key by sending nonsense.
        let unusable = pair_keys(
            &session,
            &own,
            Some(&own_ephemeral),
            &peer.public_hex(),
            Some("not-a-key"),
        );
        assert_eq!(unusable, legacy);

        // No identity from the peer (or none of our own ephemeral): the session key.
        assert!(pair_keys(
            &session,
            &own,
            Some(&own_ephemeral),
            "",
            Some(&peer_ephemeral_hex)
        )
        .is_empty());
        assert_eq!(
            pair_keys(
                &session,
                &own,
                None,
                &peer.public_hex(),
                Some(&peer_ephemeral_hex)
            ),
            vec![static_key],
            "the agreement needs an ephemeral on both sides"
        );
    }

    /// A second ephemeral on the same connection is ignored: the key the two ends agreed
    /// on is not replaced under a peer that is still using it.
    ///
    /// The first-usable-wins rule is what makes a tampered or repeated frame cost the
    /// pair key instead of silently re-keying the connection.
    #[tokio::test]
    async fn test_a_repeated_ephemeral_does_not_re_key_the_connection() {
        let (mut alice, _alice_events) = test_manager("one-ephemeral").await;
        let (mut bob, _bob_events) = test_manager("one-ephemeral").await;

        let alice_identity = NetworkIdentity::generate();
        let bob_identity = NetworkIdentity::generate();
        alice.set_nickname("Alice").await;
        alice.set_identity(&alice_identity).await;
        bob.set_nickname("Bob").await;
        bob.set_identity(&bob_identity).await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        let alice_address = alice.local_addr().unwrap();
        bob.connect(&alice_address.to_string()).await.unwrap();

        // Wait for the *agreed* key, not merely for a pair key: the static derivation is
        // available the moment the identities are, and waiting for `is_some` would race
        // the ephemeral frame.
        let static_key = crate::crypto::contact_key(
            alice.crypto.key(),
            &alice_identity.public_hex(),
            &bob_identity.public_hex(),
        )
        .expect("a static pair key");
        assert!(
            wait_for(|| alice
                .contact_key("Bob")
                .is_some_and(|key| key != static_key))
            .await,
            "the agreed key must replace the static one once the ephemeral arrives"
        );
        let agreed = alice.contact_key("Bob").expect("an agreed key");

        // Bob's connection sends a different ephemeral, as a tampered or repeated frame
        // would: the connection must keep the key it agreed on.
        let bob_remote = {
            let peers = alice.peers.lock().expect("peer table");
            *peers.keys().next().expect("Bob's connection is registered")
        };
        let other = EphemeralKey::generate();
        alice
            .shared()
            .accept_ephemeral(bob_remote, other.public_hex().as_bytes());

        assert_eq!(
            alice.contact_key("Bob"),
            Some(agreed),
            "the first ephemeral of a connection is the one that counts"
        );

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// A receiver reads a frame sealed with any of the three keys of the ladder, on a
    /// real connection.
    ///
    /// The *sender's* half of the rule (the agreed key is preferred when the connection
    /// has one) is pinned by `test_the_pair_key_ladder`; this pins the receiver's half
    /// and, in the middle case, the compatibility path: a frame a peer from before the
    /// key agreement sealed with the static derivation is still read.
    #[tokio::test]
    async fn test_the_three_step_ladder_on_a_connection() {
        let (mut alice, _alice_events) = test_manager("ladder").await;
        let (mut bob, mut bob_events) = test_manager("ladder").await;

        let alice_identity = NetworkIdentity::generate();
        let bob_identity = NetworkIdentity::generate();
        alice.set_nickname("Alice").await;
        alice.set_identity(&alice_identity).await;
        bob.set_nickname("Bob").await;
        bob.set_identity(&bob_identity).await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        bob.connect(&alice.local_addr().unwrap().to_string())
            .await
            .unwrap();

        let static_key = crate::crypto::contact_key(
            alice.crypto.key(),
            &alice_identity.public_hex(),
            &bob_identity.public_hex(),
        )
        .expect("a static pair key");
        // The agreed key only exists once the ephemeral frame has been read, which is why
        // this waits for it to *supersede* the static one rather than for `is_some`.
        assert!(
            wait_for(|| alice
                .contact_key("Bob")
                .is_some_and(|key| key != static_key))
            .await,
            "the connection must agree a key"
        );
        let agreed = alice.contact_key("Bob").expect("an agreed key");
        let session_key = alice.crypto.key().to_vec();

        for (step, key) in [
            ("agreed", &agreed),
            ("static", &static_key),
            ("session", &session_key),
        ] {
            let sealed = alice
                .crypto
                .encrypt_with(key, b"ladder")
                .expect("sealing must work");
            let receipt = match alice
                .send_ciphertext_to_peer("Bob", &sealed, MessageKind::Text, ContentType::Text)
                .unwrap()
            {
                SendOutcome::Sent(receipt) => receipt,
                other => panic!("expected a direct send, got {other:?}"),
            };
            assert_eq!(receipt.peers, 1);

            let (from, text) = next_message_from(&mut bob_events).await;
            assert_eq!(from, "Alice");
            assert_eq!(text, "ladder", "a {step}-sealed frame must be read");
        }

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// Encoding and length accounting round-trip
    #[test]
    fn test_encode_frame_layout() {
        let frame = encode_frame(FRAME_MESSAGE, b"abc");
        assert_eq!(frame[0], FRAME_MESSAGE);
        assert_eq!(&frame[1..5], &[0, 0, 0, 3]);
        assert_eq!(&frame[5..], b"abc");
    }

    /// `(kind, content_type)` maps to exactly one frame kind, and the body layout
    /// is identical in every case, so the message id is always the first 8 bytes.
    #[test]
    fn test_message_frame_kinds() {
        for (kind, content_type, expected) in [
            (MessageKind::Text, ContentType::Text, FRAME_MESSAGE),
            (MessageKind::Action, ContentType::Text, FRAME_ACTION),
            (MessageKind::Text, ContentType::Binary, FRAME_BINARY),
        ] {
            let frame = encode_message_frame(7, b"payload", kind, content_type);
            assert_eq!(frame[0], expected, "{kind}/{content_type}");
            assert_eq!(decode_message_id(&frame[5..]), Some(7));
            assert_eq!(&frame[5 + MESSAGE_ID_LEN..], b"payload");
            assert_eq!(
                message_attributes_of(expected),
                Some((kind, content_type)),
                "the mapping must round-trip"
            );
        }

        // A binary action would collide with an action frame; the core refuses the
        // combination before it gets here, and the encoder resolves it to the
        // action kind rather than panicking.
        let frame = encode_message_frame(1, b"x", MessageKind::Action, ContentType::Binary);
        assert_eq!(frame[0], FRAME_ACTION);

        // Unknown frame kinds carry no message.
        assert_eq!(message_attributes_of(FRAME_HELLO), None);
        assert_eq!(message_attributes_of(FRAME_ACK), None);
        assert_eq!(message_attributes_of(255), None);
    }

    /// A manager can be created and reports its configured limits
    #[tokio::test]
    async fn test_network_manager_creation() {
        let config = NetworkConfig {
            port: 33445,
            bootstrap_nodes: vec!["node1:33445".to_string()],
            connection_timeout: 30,
            keepalive_interval: crate::config::DEFAULT_KEEPALIVE_INTERVAL,
            max_connections: 100,
            enable_upnp: false,
            enable_ipv6: true,
        };

        let crypto_manager = Arc::new(CryptoManager::test_new(
            true,
            "ChaCha20-Poly1305".to_string(),
        ));
        let (event_sender, _event_receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);

        let manager =
            NetworkManager::new(&config, crypto_manager, event_sender, PinStore::volatile())
                .await
                .unwrap();
        assert_eq!(manager.port(), 33445);
        assert_eq!(manager.max_connections(), 100);
        assert!(!manager.is_started());
        assert_eq!(manager.connected_peers(), 0);
    }

    /// Startup binds a real socket and shutdown tears it down
    #[tokio::test]
    async fn test_network_startup_shutdown() {
        let (mut manager, _receiver) = test_manager("startup").await;

        assert!(!manager.is_started());
        assert!(manager.local_addr().is_none());

        manager.start().await.unwrap();
        assert!(manager.is_started());

        let bound = manager.local_addr().expect("bound address");
        assert_ne!(bound.port(), 0, "port 0 must be replaced by the OS port");
        assert_eq!(manager.port(), bound.port());

        manager.shutdown().await.unwrap();
        assert!(!manager.is_started());
        assert!(manager.local_addr().is_none());
    }

    /// A manager with `enable_ipv6` set, for the dual-stack listener test.
    async fn ipv6_manager(passphrase: &str) -> (NetworkManager, mpsc::Receiver<AppEvent>) {
        let crypto = Arc::new(
            CryptoManager::from_passphrase(&crate::config::CryptoConfig::default(), passphrase)
                .expect("derive key"),
        );
        let (sender, receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);
        let mut config = loopback_config();
        config.enable_ipv6 = true;

        let manager = NetworkManager::new(&config, crypto, sender, PinStore::volatile())
            .await
            .expect("network manager");
        (manager, receiver)
    }

    /// `enable_ipv6` binds **one** dual-stack wildcard: the IPv4 peers the TCP
    /// transport always served must still reach it, which is exactly what a plain
    /// `[::]` bind gets wrong on Windows (where the default is IPv6-only).
    ///
    /// The assertion is skipped on a host with no usable IPv6 stack, because that
    /// host takes the documented IPv4 fallback and the test cannot change that.
    #[tokio::test]
    async fn test_ipv6_listener_is_dual_stack() {
        let (mut manager, _receiver) = ipv6_manager("dual-stack").await;
        manager.start().await.unwrap();

        let bound = manager.local_addr().expect("bound address");
        assert_ne!(bound.port(), 0);

        if bound.is_ipv6() {
            // Both families on the same socket and port.
            assert!(
                TcpStream::connect(("127.0.0.1", bound.port()))
                    .await
                    .is_ok(),
                "the dual-stack listener must still accept IPv4"
            );
            assert!(
                TcpStream::connect(("::1", bound.port())).await.is_ok(),
                "the dual-stack listener must accept IPv6"
            );
        } else {
            eprintln!("skipping the dual-stack assertion: this host has no IPv6");
        }

        manager.shutdown().await.unwrap();
    }

    /// Two managers exchange an encrypted message over loopback TCP
    #[tokio::test]
    async fn test_peer_to_peer_message_exchange() {
        const PASSPHRASE: &str = "welcome to the metaverse";

        let (mut alice, mut alice_events) = test_manager(PASSPHRASE).await;
        let (mut bob, mut bob_events) = test_manager(PASSPHRASE).await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();

        alice.set_nickname("Alice").await;
        bob.set_nickname("Bob").await;

        // Bob dials Alice.
        let alice_addr = alice.local_addr().unwrap();
        bob.connect(&alice_addr.to_string()).await.unwrap();

        assert!(
            wait_for(|| alice.connected_peers() == 1 && bob.connected_peers() == 1).await,
            "both sides should see one peer"
        );
        assert!(wait_for(|| alice.peer_nicknames() == vec!["Bob".to_string()]).await);
        assert_eq!(bob.peer_nicknames(), vec!["Alice".to_string()]);

        // Bob sends an encrypted message to Alice.
        assert_eq!(
            bob.broadcast(b"hello from bob", MessageKind::Text, ContentType::Text)
                .unwrap()
                .peers,
            1
        );

        // Handshake events may arrive first, so wait for the message itself.
        let (from, text) = next_message_from(&mut alice_events).await;
        assert_eq!(from, "Bob", "the receiver should see the sender nickname");
        assert_eq!(text, "hello from bob");

        // And Alice can answer Bob.
        assert_eq!(
            alice
                .broadcast(b"hi bob", MessageKind::Text, ContentType::Text)
                .unwrap()
                .peers,
            1
        );
        assert_eq!(next_message(&mut bob_events).await, "hi bob");

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// A peer using a different passphrase cannot decrypt messages
    #[tokio::test]
    async fn test_mismatched_passphrase_is_rejected() {
        let (mut alice, mut alice_events) = test_manager("alice-secret").await;
        let (mut bob, _bob_events) = test_manager("bob-secret").await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();

        let alice_addr = alice.local_addr().unwrap();
        bob.connect(&alice_addr.to_string()).await.unwrap();
        assert!(wait_for(|| alice.connected_peers() == 1).await);

        assert_eq!(
            bob.broadcast(b"secret", MessageKind::Text, ContentType::Text)
                .unwrap()
                .peers,
            1
        );

        // The frame arrives but fails authentication, so no message event is
        // produced.
        let mut saw_message = false;
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(300), alice_events.recv()).await
        {
            if matches!(event, AppEvent::MessageReceived { .. }) {
                saw_message = true;
                break;
            }
        }
        assert!(
            !saw_message,
            "a message encrypted with another key must not be delivered"
        );

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// Messages can be addressed to one peer, and are acknowledged on receipt
    #[tokio::test]
    async fn test_directed_delivery_and_acknowledgement() {
        let (mut alice, mut alice_events) = test_manager("routing").await;
        let (mut bob, mut bob_events) = test_manager("routing").await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        alice.set_nickname("Alice").await;
        bob.set_nickname("Bob").await;

        let alice_addr = alice.local_addr().unwrap();
        bob.connect(&alice_addr.to_string()).await.unwrap();
        assert!(wait_for(|| bob.peer_nicknames() == vec!["Alice".to_string()]).await);

        // Unknown nicknames are buffered rather than dropped.
        assert!(
            matches!(
                bob.send_to_peer("Nobody", b"lost", MessageKind::Text, ContentType::Text)
                    .unwrap(),
                SendOutcome::Queued { .. }
            ),
            "a message for an offline peer must be queued"
        );
        assert_eq!(bob.queued_messages(), 1);

        // Nicknames are matched case-insensitively.
        let receipt = match bob
            .send_to_peer(
                "alice",
                b"direct ping",
                MessageKind::Text,
                ContentType::Text,
            )
            .unwrap()
        {
            SendOutcome::Sent(receipt) => receipt,
            other => panic!("expected a direct send, got {other:?}"),
        };
        assert_eq!(receipt.peers, 1);
        assert!(receipt.message_id > 0, "message ids start at 1");

        let (from, text) = next_message_from(&mut alice_events).await;
        assert_eq!(from, "Bob");
        assert_eq!(text, "direct ping");

        // Alice acknowledges automatically, so Bob observes the delivery.
        let (ack_peer, ack_id) = next_delivery(&mut bob_events).await;
        assert_eq!(ack_peer, "Alice");
        assert_eq!(ack_id, receipt.message_id);

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// Two peers that announced an identity *and* a per-connection ephemeral encrypt a
    /// directed message under a key they agreed, and both ends agree on the same one.
    ///
    /// This is the network half of A12's key agreement: `identity::tests` own what the
    /// agreed key *is* (symmetric, per-connection, needing a secret), and this proves
    /// the transport actually runs the exchange and seals a directed message with the
    /// result.
    #[tokio::test]
    async fn test_a_directed_message_uses_the_pair_key() {
        let (mut alice, mut alice_events) = test_manager("pair-key").await;
        let (mut bob, mut bob_events) = test_manager("pair-key").await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        alice.set_nickname("Alice").await;
        bob.set_nickname("Bob").await;
        let alice_identity = NetworkIdentity::generate();
        let bob_identity = NetworkIdentity::generate();
        alice.set_identity(&alice_identity).await;
        bob.set_identity(&bob_identity).await;

        // Before any connection there is no pair to key: the session key is the only
        // option, which is what a TCP peer used to always get.
        assert_eq!(alice.contact_key("Bob"), None);
        assert_eq!(bob.contact_key("Alice"), None);

        let alice_addr = alice.local_addr().unwrap();
        bob.connect(&alice_addr.to_string()).await.unwrap();
        assert!(wait_for(|| bob.peer_nicknames() == vec!["Alice".to_string()]).await);
        assert!(wait_for(|| alice.peer_nicknames() == vec!["Bob".to_string()]).await);

        // Both ends now hold the same key for the pair, and it is not the session key.
        let (Some(from_alice), Some(from_bob)) =
            (alice.contact_key("Bob"), bob.contact_key("Alice"))
        else {
            panic!("both ends must agree a key once both identities are known");
        };
        assert_eq!(from_alice, from_bob, "the agreement must be symmetric");
        assert_ne!(from_alice.as_slice(), alice.crypto.key());
        assert_eq!(from_alice.len(), crate::crypto::CONTACT_KEY_LENGTH);

        // ...and it is *not* what an observer of the handshake can derive: the static
        // derivation only needs the session key (which every member has) and both
        // announced identities. This is the assertion that fails if the agreement is
        // silently replaced by the older derivation.
        let derivable = crate::crypto::contact_key(
            alice.crypto.key(),
            &alice_identity.public_hex(),
            &bob_identity.public_hex(),
        )
        .expect("both identities are announced");
        assert_eq!(
            derivable.len(),
            crate::crypto::CONTACT_KEY_LENGTH,
            "the fallback is still a real key"
        );
        assert_ne!(
            from_alice, derivable,
            "seeing both public identities must no longer be enough"
        );

        // A peer whose identity was never announced has no pair key, so the session
        // key stays in use for it (the compatibility path).
        assert_eq!(bob.contact_key("Nobody"), None);

        // The message crosses: Alice encrypts under the agreed key, Bob derives the
        // same one and reads it.
        let receipt = match alice
            .send_to_peer("Bob", b"pair keyed", MessageKind::Text, ContentType::Text)
            .unwrap()
        {
            SendOutcome::Sent(receipt) => receipt,
            other => panic!("expected a direct send, got {other:?}"),
        };
        let (from, text) = next_message_from(&mut bob_events).await;
        assert_eq!(from, "Alice");
        assert_eq!(text, "pair keyed");

        // ...and Bob's acknowledgement comes back under the same pair key.
        let (ack_peer, ack_id) = next_delivery(&mut alice_events).await;
        assert_eq!(ack_peer, "Bob");
        assert_eq!(ack_id, receipt.message_id);

        // A third session member agrees a *different* key for its own pair, so it
        // cannot open what Alice wrote to Bob. What an active participant could still
        // do is present its own identity on a *first* contact, which is what the pin
        // warns about (`trust`) and the fingerprint comparison is for.
        let (mut carol, _carol_events) = test_manager("pair-key").await;
        let carol_identity = NetworkIdentity::generate();
        carol.start().await.unwrap();
        carol.set_nickname("Carol").await;
        carol.set_identity(&carol_identity).await;
        carol.connect(&alice_addr.to_string()).await.unwrap();

        // The nickname arrives with the `hello` frame, the identity one frame later and the
        // ephemeral after that; the key needs all three, so waiting for the nickname alone
        // is a race — it was, in this test, until the greeting grew a third frame and lost
        // it. Wait for both ends to have *agreed* a key (i.e. moved past the static
        // derivation the identities alone already produce) instead.
        let carol_static = crate::crypto::contact_key(
            alice.crypto.key(),
            &alice_identity.public_hex(),
            &carol_identity.public_hex(),
        )
        .expect("the two identities suffice to derive a static key");
        assert!(
            wait_for(|| {
                alice
                    .contact_key("Carol")
                    .is_some_and(|key| key != carol_static)
                    && carol
                        .contact_key("Alice")
                        .is_some_and(|key| key != carol_static)
            })
            .await,
            "the third pair must agree a key of its own"
        );
        let (Some(alice_to_carol), Some(carol_to_alice)) =
            (alice.contact_key("Carol"), carol.contact_key("Alice"))
        else {
            panic!("the third pair must agree a key of its own");
        };
        assert_eq!(alice_to_carol, carol_to_alice);
        assert_ne!(
            alice_to_carol, carol_static,
            "the pair must be past its static derivation"
        );
        assert_ne!(
            alice_to_carol, from_alice,
            "one key must not serve two different pairs"
        );

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
        carol.shutdown().await.unwrap();
    }

    /// A peer that announced no identity still talks: the session key is the
    /// fallback, and `contact_key` says so instead of guessing.
    #[tokio::test]
    async fn test_a_peer_without_an_identity_uses_the_session_key() {
        let (mut alice, _alice_events) = test_manager("no-identity").await;
        let (mut bob, mut bob_events) = test_manager("no-identity").await;

        alice.start().await.unwrap();
        bob.start().await.unwrap();
        alice.set_nickname("Alice").await;
        bob.set_nickname("Bob").await;
        // Only Alice announces an identity: a pair needs two.
        alice.set_identity(&NetworkIdentity::generate()).await;

        let alice_addr = alice.local_addr().unwrap();
        bob.connect(&alice_addr.to_string()).await.unwrap();
        assert!(wait_for(|| bob.peer_nicknames() == vec!["Alice".to_string()]).await);
        assert!(wait_for(|| alice.peer_nicknames() == vec!["Bob".to_string()]).await);

        assert_eq!(
            alice.contact_key("Bob"),
            None,
            "one announced identity is not a pair"
        );
        assert_eq!(
            bob.contact_key("Alice"),
            None,
            "Bob has no identity of its own"
        );

        // The message still crosses, under the session key.
        assert!(matches!(
            alice
                .send_to_peer(
                    "Bob",
                    b"session keyed",
                    MessageKind::Text,
                    ContentType::Text
                )
                .unwrap(),
            SendOutcome::Sent(_)
        ));
        let (from, text) = next_message_from(&mut bob_events).await;
        assert_eq!(from, "Alice");
        assert_eq!(text, "session keyed");

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    /// A desired peer that only starts later is connected automatically
    #[tokio::test]
    async fn test_supervisor_connects_when_peer_starts_later() {
        // Reserve a free port, then release it: nothing listens there yet.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = probe.local_addr().unwrap().to_string();
        drop(probe);

        let (mut bob, _bob_events) = test_manager_on("supervisor", 0).await;
        bob.start().await.unwrap();
        bob.desire_peer(address.clone());

        assert_eq!(bob.desired_peers(), vec![address.clone()]);
        assert_eq!(bob.pending_peers(), 1, "nothing is listening yet");

        // The address is currently dead, so an immediate dial fails.
        assert!(bob.connect(&address).await.is_err());
        assert!(bob.connected_peers() == 0);

        // Alice comes up on the advertised port afterwards.
        let port = address.rsplit(':').next().unwrap().parse().unwrap();
        let (mut alice, _alice_events) = test_manager_on("supervisor", port).await;
        alice.start().await.unwrap();

        // The supervisor retries in the background and gets there on its own.
        assert!(
            wait_for(|| bob.connected_peers() == 1).await,
            "the supervisor should connect to {address}"
        );
        assert_eq!(bob.pending_peers(), 0);

        bob.shutdown().await.unwrap();
        alice.shutdown().await.unwrap();
    }

    /// A message sent while the peer is offline is delivered when it connects
    #[tokio::test]
    async fn test_offline_messages_are_delivered_after_the_peer_connects() {
        // Reserve a port, then release it so nothing is listening yet.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);

        let (mut bob, _bob_events) = test_manager_on("outbox", 0).await;
        bob.start().await.unwrap();
        bob.set_nickname("Bob").await;
        bob.desire_peer(address.to_string());

        // Alice is not running, so the message is buffered.
        let outcome = bob
            .send_to_peer(
                "Alice",
                b"are you there?",
                MessageKind::Text,
                ContentType::Text,
            )
            .unwrap();
        assert!(
            matches!(outcome, SendOutcome::Queued { position: 1, .. }),
            "expected the first message to be queued, got {outcome:?}"
        );
        assert_eq!(bob.queued_messages(), 1);

        let second = bob
            .send_to_peer(
                "Alice",
                b"still there?",
                MessageKind::Text,
                ContentType::Text,
            )
            .unwrap();
        assert!(matches!(second, SendOutcome::Queued { position: 2, .. }));
        assert_eq!(bob.queued_messages(), 2);

        // Alice comes up and announces herself; the outbox is flushed.
        let port = address.port();
        let (mut alice, mut alice_events) = test_manager_on("outbox", port).await;
        alice.start().await.unwrap();
        alice.set_nickname("Alice").await;

        assert_eq!(next_message(&mut alice_events).await, "are you there?");
        assert_eq!(next_message(&mut alice_events).await, "still there?");
        assert!(
            wait_for(|| bob.queued_messages() == 0).await,
            "outbox drained"
        );

        bob.shutdown().await.unwrap();
        alice.shutdown().await.unwrap();
    }

    /// The outbox is bounded so a long offline period cannot grow without limit
    #[tokio::test]
    async fn test_outbox_is_bounded() {
        let (mut manager, _events) = test_manager("bounded").await;
        manager.start().await.unwrap();

        for index in 0..OUTBOX_CAPACITY {
            assert!(
                matches!(
                    manager
                        .send_to_peer(
                            "Ghost",
                            format!("m{index}").as_bytes(),
                            MessageKind::Text,
                            ContentType::Text
                        )
                        .unwrap(),
                    SendOutcome::Queued { .. }
                ),
                "message {index} should fit"
            );
        }

        assert_eq!(manager.queued_messages(), OUTBOX_CAPACITY);
        assert_eq!(
            manager
                .send_to_peer(
                    "Ghost",
                    b"one too many",
                    MessageKind::Text,
                    ContentType::Text
                )
                .unwrap(),
            SendOutcome::Dropped
        );
        assert_eq!(manager.queued_messages(), OUTBOX_CAPACITY);

        manager.shutdown().await.unwrap();
    }

    /// Shutdown clears the desired peers so nothing is retried afterwards
    #[tokio::test]
    async fn test_shutdown_clears_desired_peers() {
        let (mut manager, _events) = test_manager("desired").await;
        manager.start().await.unwrap();
        manager.desire_peer("127.0.0.1:9");
        assert_eq!(manager.desired_peers().len(), 1);

        manager.shutdown().await.unwrap();
        assert!(manager.desired_peers().is_empty());
        assert_eq!(manager.pending_peers(), 0);
    }

    /// Wait for the next message event, returning `(sender, text)`
    async fn next_message_from(receiver: &mut mpsc::Receiver<AppEvent>) -> (String, String) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await {
                Ok(Some(AppEvent::MessageReceived { peer, payload, .. })) => {
                    return (peer, String::from_utf8_lossy(&payload).to_string());
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("event channel closed"),
                Err(error) => panic!("timed out waiting for a message: {error}"),
            }
        }
    }

    /// Wait for the next delivery confirmation, returning `(peer, message_id)`
    async fn next_delivery(receiver: &mut mpsc::Receiver<AppEvent>) -> (String, u64) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await {
                Ok(Some(AppEvent::MessageDelivered {
                    peer, message_id, ..
                })) => {
                    return (peer, message_id);
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("event channel closed"),
                Err(error) => panic!("timed out waiting for a delivery confirmation: {error}"),
            }
        }
    }

    /// Wait for the next message event, skipping unrelated network events
    async fn next_message(receiver: &mut mpsc::Receiver<AppEvent>) -> String {
        next_message_from(receiver).await.1
    }
}
