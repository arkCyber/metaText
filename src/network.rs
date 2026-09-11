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

use crate::crypto::CryptoManager;
use crate::error::{MetaTextError, MetaTextResult};
use crate::types::{AppEvent, NetworkEvent};

/// Frame kind carrying the peer's nickname (plain text).
const FRAME_HELLO: u8 = 1;

/// Frame kind carrying an encrypted chat message.
const FRAME_MESSAGE: u8 = 2;

/// Frame kind acknowledging a received message.
const FRAME_ACK: u8 = 3;

/// Bytes reserved at the front of message/ack frames for the message id.
const MESSAGE_ID_LEN: usize = 8;

/// Largest frame accepted from a peer.
///
/// The length prefix is attacker controlled, so it is validated against this
/// bound before any allocation happens.
const MAX_FRAME_SIZE: u32 = 64 * 1024;

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
    /// Outbound queue drained by the peer's writer task
    sender: mpsc::UnboundedSender<Vec<u8>>,

    /// Nickname announced by the peer during the handshake, if seen yet
    nickname: Option<String>,
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

    /// Whether the network is started
    started: bool,

    /// Encryption shared with the application
    crypto: Arc<CryptoManager>,

    /// Application event channel
    event_sender: mpsc::Sender<AppEvent>,

    /// Nickname announced to peers
    nickname: Arc<RwLock<String>>,

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
    pub async fn new(
        config: &crate::config::NetworkConfig,
        crypto_manager: Arc<CryptoManager>,
        event_sender: mpsc::Sender<AppEvent>,
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
            started: false,
            crypto: crypto_manager,
            event_sender,
            nickname: Arc::new(RwLock::new(String::new())),
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

        let bind_addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener =
            TcpListener::bind(bind_addr)
                .await
                .map_err(|error| MetaTextError::Network {
                    message: format!("Failed to bind {bind_addr}: {error}"),
                    operation: "bind".to_string(),
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

    /// Set (or update) the nickname announced to peers
    ///
    /// The new name is pushed to every connected peer immediately.
    pub async fn set_nickname(&self, nickname: &str) {
        *self.nickname.write().await = nickname.to_string();

        let hello = self.shared().hello_frame().await;
        let senders: Vec<mpsc::UnboundedSender<Vec<u8>>> = self
            .peers
            .lock()
            .map(|peers| peers.values().map(|peer| peer.sender.clone()).collect())
            .unwrap_or_default();

        for sender in senders {
            let _ = sender.send(hello.clone());
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
    pub fn broadcast(&self, plaintext: &[u8]) -> MetaTextResult<DeliveryReceipt> {
        let ciphertext = self.crypto.encrypt(plaintext)?;
        self.broadcast_ciphertext(&ciphertext)
    }

    /// Queue an already encrypted payload for every connected peer
    ///
    /// Callers that encrypt themselves (for example to reuse the ciphertext
    /// for local statistics) use this to avoid encrypting twice.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn broadcast_ciphertext(&self, ciphertext: &[u8]) -> MetaTextResult<DeliveryReceipt> {
        let senders = self.peer_senders(|_| true)?;
        Ok(self.dispatch_to(senders, ciphertext))
    }

    /// Encrypt `plaintext` and send it to a single peer by nickname
    ///
    /// The nickname is compared case-insensitively against the names announced
    /// during the handshake. When no such peer is connected the message is
    /// buffered in that peer's outbox.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Cryptographic`] if encryption fails, or
    /// [`MetaTextError::Network`] if the peer registry is unusable.
    pub fn send_to_peer(&self, nickname: &str, plaintext: &[u8]) -> MetaTextResult<SendOutcome> {
        let ciphertext = self.crypto.encrypt(plaintext)?;
        self.send_ciphertext_to_peer(nickname, &ciphertext)
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
            return Ok(SendOutcome::Sent(self.dispatch_to(senders, ciphertext)));
        }

        // Nobody is announcing that name right now: buffer the message.
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        Ok(self.enqueue(&wanted, message_id, ciphertext))
    }

    /// Append a message to a nickname's outbox
    fn enqueue(&self, nickname: &str, message_id: u64, ciphertext: &[u8]) -> SendOutcome {
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

    /// Allocate an id, frame `ciphertext` and queue it for `senders`
    fn dispatch_to(
        &self,
        senders: Vec<mpsc::UnboundedSender<Vec<u8>>>,
        ciphertext: &[u8],
    ) -> DeliveryReceipt {
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        let frame = encode_message_frame(message_id, ciphertext);

        let mut peers = 0;
        for sender in senders {
            if sender.send(frame.clone()).is_ok() {
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
    ) -> MetaTextResult<Vec<mpsc::UnboundedSender<Vec<u8>>>> {
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
            peers: Arc::clone(&self.peers),
            tasks: Arc::clone(&self.tasks),
            desired_peers: Arc::clone(&self.desired_peers),
            outbox: Arc::clone(&self.outbox),
            outbox_expired: Arc::clone(&self.outbox_expired),
            max_connections: self.max_connections,
            connect_timeout: self.connect_timeout,
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
        let (sender, receiver) = mpsc::unbounded_channel::<Vec<u8>>();

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

        // Announce our nickname immediately so the other side can show it.
        let _ = sender.send(self.hello_frame().await);

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
}

/// I/O half of the peer connection, split out to keep the impl blocks small.
impl Shared {
    /// Read frames until the peer disconnects
    async fn read_loop(&self, mut reader: ReadHalf<TcpStream>, remote: SocketAddr) {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some((kind, payload))) => self.dispatch(kind, &payload, remote).await,
                // Clean end of stream: the peer closed the connection.
                Ok(None) => break,
                Err(error) => {
                    debug!("Peer {remote} read error: {error}");
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
                reason: "connection closed".to_string(),
            }))
            .await;
    }

    /// Write queued frames until the channel closes
    async fn write_loop(
        &self,
        mut writer: WriteHalf<TcpStream>,
        mut receiver: mpsc::UnboundedReceiver<Vec<u8>>,
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
                let nickname = String::from_utf8_lossy(payload).to_string();
                if let Ok(mut peers) = self.peers.lock() {
                    if let Some(peer) = peers.get_mut(&remote) {
                        peer.nickname = Some(nickname.clone());
                    }
                }
                info!("👤 Peer {remote} identifies as '{nickname}'");

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
            FRAME_MESSAGE => {
                let Some(message_id) = decode_message_id(payload) else {
                    warn!("⚠️ Message frame without an id from {remote}");
                    return;
                };
                let ciphertext = &payload[MESSAGE_ID_LEN..];

                match self.crypto.decrypt(ciphertext) {
                    Ok(plaintext) => {
                        let _ = self
                            .event_sender
                            .send(AppEvent::MessageReceived {
                                peer: self.nickname_of(remote),
                                peer_id: remote.to_string(),
                                payload: plaintext,
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
            .is_some_and(|sender| sender.send(frame).is_ok())
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
            let frame = encode_message_frame(entry.message_id, &entry.ciphertext);
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
fn encode_message_frame(message_id: u64, ciphertext: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(MESSAGE_ID_LEN + ciphertext.len());
    payload.extend_from_slice(&message_id.to_be_bytes());
    payload.extend_from_slice(ciphertext);
    encode_frame(FRAME_MESSAGE, &payload)
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

        let manager = NetworkManager::new(&config, crypto, sender)
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

    /// Encoding and length accounting round-trip
    #[test]
    fn test_encode_frame_layout() {
        let frame = encode_frame(FRAME_MESSAGE, b"abc");
        assert_eq!(frame[0], FRAME_MESSAGE);
        assert_eq!(&frame[1..5], &[0, 0, 0, 3]);
        assert_eq!(&frame[5..], b"abc");
    }

    /// A manager can be created and reports its configured limits
    #[tokio::test]
    async fn test_network_manager_creation() {
        let config = NetworkConfig {
            port: 33445,
            bootstrap_nodes: vec!["node1:33445".to_string()],
            connection_timeout: 30,
            max_connections: 100,
            enable_upnp: true,
            enable_ipv6: true,
        };

        let crypto_manager = Arc::new(CryptoManager::test_new(
            true,
            "ChaCha20-Poly1305".to_string(),
        ));
        let (event_sender, _event_receiver) = mpsc::channel(EVENT_INBOX_CAPACITY);

        let manager = NetworkManager::new(&config, crypto_manager, event_sender)
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
        assert_eq!(bob.broadcast(b"hello from bob").unwrap().peers, 1);

        // Handshake events may arrive first, so wait for the message itself.
        let (from, text) = next_message_from(&mut alice_events).await;
        assert_eq!(from, "Bob", "the receiver should see the sender nickname");
        assert_eq!(text, "hello from bob");

        // And Alice can answer Bob.
        assert_eq!(alice.broadcast(b"hi bob").unwrap().peers, 1);
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

        assert_eq!(bob.broadcast(b"secret").unwrap().peers, 1);

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
                bob.send_to_peer("Nobody", b"lost").unwrap(),
                SendOutcome::Queued { .. }
            ),
            "a message for an offline peer must be queued"
        );
        assert_eq!(bob.queued_messages(), 1);

        // Nicknames are matched case-insensitively.
        let receipt = match bob.send_to_peer("alice", b"direct ping").unwrap() {
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
        let outcome = bob.send_to_peer("Alice", b"are you there?").unwrap();
        assert!(
            matches!(outcome, SendOutcome::Queued { position: 1, .. }),
            "expected the first message to be queued, got {outcome:?}"
        );
        assert_eq!(bob.queued_messages(), 1);

        let second = bob.send_to_peer("Alice", b"still there?").unwrap();
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
                        .send_to_peer("Ghost", format!("m{index}").as_bytes())
                        .unwrap(),
                    SendOutcome::Queued { .. }
                ),
                "message {index} should fit"
            );
        }

        assert_eq!(manager.queued_messages(), OUTBOX_CAPACITY);
        assert_eq!(
            manager.send_to_peer("Ghost", b"one too many").unwrap(),
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
