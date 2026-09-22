/*!
 * transport.rs
 *
 * Transport abstraction owned by the core actor.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-12
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`CoreTransport`] lets the core service (`CoreService`, one crate up) drive
 *   either the TCP transport ([`crate::network::NetworkManager`]) or Tox (the
 *   feature-gated `ToxTransport`) without knowing which one is in use
 * - One narrow surface: lifecycle, nickname, peer registration, payload
 *   sending, statistics and pending friend requests
 * - Every transport reports events into the same bounded [`AppEvent`] inbox,
 *   so the actor and its front-ends are transport independent
 *
 * # Design
 *
 * The core needs six things from a transport: start/stop, announce a nickname,
 * bring a peer up, send an opaque payload, describe itself for `/info`, and (for
 * Tox only) surface incoming friend requests. Those operations are collected in
 * [`CoreTransport`] rather than a trait object because an enum keeps the async
 * methods object-safe-free, allocates nothing and stays `Send` for the actor.
 *
 * # Payload confidentiality
 *
 * The TCP transport has no confidentiality of its own, so the core wraps every
 * payload with [`crate::crypto::CryptoManager`] before handing it over. Tox
 * already encrypts peer to peer between friends, so
 * [`CoreTransport::provides_encryption`] is `true` there and the core sends the
 * payload unchanged. In both cases the actor only ever sees a decrypted payload,
 * so message handling is identical.
 */

// `CoreTransport` is one surface for two transports: the Tox arms await a
// blocking worker (`spawn_blocking`) and their accessors cannot be `const`,
// while a TCP-only build (no `tox-protocol`) makes the identical body neither.
// The feature therefore changes the lint's verdict, and the signature must not
// change with it: the actor awaits `start`/`shutdown` and reads the accessors the
// same way for either transport.
#![allow(clippy::unused_async, clippy::missing_const_for_fn)]

#[cfg(feature = "tox-protocol")]
use std::collections::{HashMap, VecDeque};
#[cfg(feature = "tox-protocol")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "tox-protocol")]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(feature = "tox-protocol")]
use std::sync::{Mutex as StdMutex, RwLock as StdRwLock};
#[cfg(feature = "tox-protocol")]
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tracing::warn;
#[cfg(feature = "tox-protocol")]
use tracing::{debug, info};

use crate::cli::CliArgs;
use crate::config::AppConfig;
use crate::crypto::CryptoManager;
use crate::error::{MetaTextError, MetaTextResult};
use crate::identity::NetworkIdentity;
use crate::network::{NetworkManager, SendOutcome};
use crate::trust::{PeerIdentity, PinStore};
#[cfg(feature = "tox-protocol")]
use crate::types::NetworkEvent;
use crate::types::{AppEvent, ContentType, Group, MessageKind};

#[cfg(feature = "tox-protocol")]
use crate::network::DeliveryReceipt;
#[cfg(feature = "tox-protocol")]
use crate::tox::{
    self, BootstrapNode, ToxClient, ToxConfig, ToxConnection, ToxError, ToxEvent, ToxIdentity,
    ToxResult, ToxSender,
};
#[cfg(feature = "tox-protocol")]
use crate::tox_store::{self, StoredMessage, StoredRequest, ToxState, ToxStore, STORE_VERSION};

/// How long the Tox bridge may wait for an event before re-checking its stop
/// flag. Bounds how quickly a shutdown is observed.
#[cfg(feature = "tox-protocol")]
const TOX_EVENT_POLL: Duration = Duration::from_millis(200);

/// How many unanswered friend requests are held.
///
/// A friend request can come from **anyone who knows the Tox address**, so the
/// list is a remote-triggerable allocation: an attacker generating fresh
/// identities produces one entry per identity. Bounding it keeps R7 ("resource use
/// must be bounded") true for a list whose size is not ours to decide. A request
/// that arrives when the list is full is refused (drop-newest) rather than
/// evicting an older one, so a flood cannot push away a request the user is about
/// to accept; the refusals are reported as `friend_requests_dropped` in
/// `/metrics`.
#[cfg(feature = "tox-protocol")]
pub const PENDING_REQUESTS_CAPACITY: usize = 128;

/// How many unanswered group invitations are held.
///
/// Same reasoning as [`PENDING_REQUESTS_CAPACITY`], with one difference: an
/// invitation can only come from a *friend*, so it is a smaller attack surface —
/// but a careless or hostile friend can still invite us without limit, and each
/// entry carries a join token.
#[cfg(feature = "tox-protocol")]
pub const PENDING_INVITES_CAPACITY: usize = 128;

/// How many payloads may wait for one offline Tox friend.
///
/// Mirrors [`crate::network`]'s outbox: a bounded buffer beats either dropping a
/// message or growing without limit.
#[cfg(feature = "tox-protocol")]
pub const TOX_OUTBOX_CAPACITY: usize = 32;

/// How long a payload waits for an offline friend before it is dropped.
#[cfg(feature = "tox-protocol")]
pub const TOX_OUTBOX_TTL: Duration = Duration::from_secs(300);

/// A friend request that has not been accepted yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRequest {
    /// The requester's public key (64 hexadecimal characters).
    pub public_key: String,

    /// The message attached to the request.
    pub message: String,
}

/// A group invitation that has not been answered yet.
///
/// The token is a **capability**: it is what makes the join possible, so it is
/// handed out to the front-end that has to decide, kept single-use by the
/// transport, and never derived from anything the user knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInvite {
    /// Inviting peer's nickname (empty until announced).
    pub peer: String,

    /// Inviting peer's address.
    pub peer_id: String,

    /// Single-use token to pass back to [`CoreTransport::join_group`].
    pub token: String,
}

/// Build a transport failure that can cross the interface.
fn network_error(message: impl Into<String>, operation: &str) -> MetaTextError {
    MetaTextError::Network {
        message: message.into(),
        operation: operation.to_string(),
        source: None,
    }
}

/// The failure every group operation returns on a transport without groups.
///
/// It is an explicit, typed refusal rather than a silent no-op, so a front-end can
/// tell the user *why* `/group` does nothing on TCP. The core reuses this wording
/// for its own pre-check, so the reason is stated in exactly one place.
#[must_use]
pub fn group_unsupported_error(operation: &str) -> MetaTextError {
    MetaTextError::Validation {
        message: format!(
            "the tcp transport has no groups, so it cannot {operation}; \
             start with --transport tox"
        ),
        field: "transport".to_string(),
        expected: Some("a transport that supports groups (--transport tox)".to_string()),
    }
}

/// The transport the core actor drives.
///
/// Chosen once, at construction, from [`CliArgs::transport`]; the actor then
/// uses the same vocabulary for either variant.
///
/// Both variants are boxed: the enum is moved around as a field of the actor, and a
/// variant that is a whole transport inlined would make every move of the enum cost
/// the size of the largest one (a `NetworkManager` carries a peer table, an outbox,
/// the pin store and a dozen shared handles; a `ToxTransport` carries its own
/// worker). The indirection is paid once, and every accessor reaches the transport
/// through the box.
#[derive(Debug)]
pub enum CoreTransport {
    /// Encrypted TCP peering between `host:port` endpoints.
    Tcp(Box<NetworkManager>),

    /// The Tox transport: a `libtoxcore` instance owned by this process.
    #[cfg(feature = "tox-protocol")]
    Tox(Box<ToxTransport>),
}

impl CoreTransport {
    /// Build the transport selected on the command line.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP listener cannot be configured or the Tox
    /// instance cannot be created (missing `libtoxcore`, unusable savedata, a
    /// bad bootstrap entry, ...).
    pub async fn new(
        config: &AppConfig,
        args: &CliArgs,
        crypto: Arc<CryptoManager>,
        event_sender: mpsc::Sender<AppEvent>,
    ) -> MetaTextResult<Self> {
        match args.transport {
            crate::cli::Transport::Tcp => {
                // The identities peers announce are pinned next to the session
                // snapshot, so a change seen after a restart is still a change (see
                // `crate::trust`). Built here rather than for every transport: the Tox
                // one has no identity frames to pin.
                let data_dir = args.data_directory().unwrap_or_else(|| PathBuf::from("."));
                let pins = PinStore::load(&data_dir);
                let network =
                    NetworkManager::new(&config.network, crypto, event_sender, pins).await?;
                Ok(Self::Tcp(Box::new(network)))
            }
            #[cfg(feature = "tox-protocol")]
            crate::cli::Transport::Tox => {
                let tox = ToxTransport::new(config, args, event_sender)?;
                Ok(Self::Tox(Box::new(tox)))
            }
            #[cfg(not(feature = "tox-protocol"))]
            crate::cli::Transport::Tox => Err(MetaTextError::Configuration {
                message: "the tox transport is not compiled into this build; \
                          rebuild with `--features tox-protocol`"
                    .to_string(),
                source: None,
            }),
        }
    }

    /// Short, stable name of the active transport (`"tcp"` or `"tox"`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "tcp",
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => "tox",
        }
    }

    /// Whether the transport encrypts payloads itself.
    ///
    /// `true` for Tox (peer to peer between friends) means the core must **not**
    /// wrap the payload again; `false` for TCP means the core applies
    /// [`CryptoManager`] before handing the payload over.
    #[must_use]
    pub const fn provides_encryption(&self) -> bool {
        match self {
            Self::Tcp(_) => false,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => true,
        }
    }

    /// Whether the transport is up.
    #[must_use]
    pub fn is_started(&self) -> bool {
        match self {
            Self::Tcp(network) => network.is_started(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.is_started(),
        }
    }

    /// Port the transport listens on (`0` when it has no port of its own).
    #[must_use]
    pub fn port(&self) -> u16 {
        match self {
            Self::Tcp(network) => network.port(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.port(),
        }
    }

    /// Configured bootstrap nodes, as `host:port` (or `host:port:key`).
    #[must_use]
    pub fn bootstrap_nodes(&self) -> Vec<String> {
        match self {
            Self::Tcp(network) => network.bootstrap_nodes().to_vec(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.bootstrap_nodes().to_vec(),
        }
    }
}

impl CoreTransport {
    /// Number of peers currently usable for messaging.
    #[must_use]
    pub fn connected_peers(&self) -> usize {
        match self {
            Self::Tcp(network) => network.connected_peers(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.connected_peers(),
        }
    }

    /// Number of known peers that are not reachable yet.
    #[must_use]
    pub fn pending_peers(&self) -> usize {
        match self {
            Self::Tcp(network) => network.pending_peers(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.pending_peers(),
        }
    }

    /// Configured upper bound on concurrent peers.
    #[must_use]
    pub fn max_connections(&self) -> u32 {
        match self {
            Self::Tcp(network) => network.max_connections(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.max_connections(),
        }
    }

    /// Payloads waiting for their destination to become reachable.
    #[must_use]
    pub fn queued_messages(&self) -> usize {
        match self {
            Self::Tcp(network) => network.queued_messages(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.queued_messages(),
        }
    }

    /// Addresses the transport keeps trying to reach.
    #[must_use]
    pub fn desired_peers(&self) -> Vec<String> {
        match self {
            Self::Tcp(network) => network.desired_peers(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.desired_peers(),
        }
    }

    /// Nicknames announced by the reachable peers.
    #[must_use]
    pub fn peer_nicknames(&self) -> Vec<String> {
        match self {
            Self::Tcp(network) => network.peer_nicknames(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.peer_nicknames(),
        }
    }

    /// The identities peers announced, with a change flagged per nickname.
    ///
    /// Empty for a transport that carries no identity frames: Tox identifies a peer
    /// by its own address and encrypts between friends, so there is nothing to pin.
    #[must_use]
    pub fn peer_identities(&self) -> Vec<PeerIdentity> {
        match self {
            Self::Tcp(network) => network.peer_identities(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => Vec::new(),
        }
    }

    /// Identities that changed under a nickname that was already pinned.
    #[must_use]
    pub fn pinned_identity_changes(&self) -> u64 {
        match self {
            Self::Tcp(network) => network.pinned_identity_changes(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => 0,
        }
    }

    /// Announcements that could not be pinned (unusable, or the table is full).
    #[must_use]
    pub fn pinned_identity_refusals(&self) -> u64 {
        match self {
            Self::Tcp(network) => network.pinned_identity_refusals(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => 0,
        }
    }

    /// Greetings refused because the nickname they announced cannot be displayed.
    ///
    /// A TCP greeting is a discrete event, so its refusals are counted. The Tox transport
    /// has no greeting to refuse: a friend's name, a conference title and a participant's
    /// name are read from toxcore through
    /// [`peer_label`](meta_text_proto::ipc::validation::peer_label) every time a snapshot
    /// is taken, so an unusable one is simply *not a name* — there is no event to count,
    /// and the label is refused as often as the snapshot is read.
    #[must_use]
    pub fn refused_greetings(&self) -> u64 {
        match self {
            Self::Tcp(network) => network.refused_greetings(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => 0,
        }
    }

    /// Address other users need in order to reach this instance.
    #[must_use]
    pub fn local_address(&self) -> Option<String> {
        match self {
            Self::Tcp(network) => network.local_addr().map(|address| address.to_string()),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => Some(tox.address().to_string()),
        }
    }

    /// Cryptographic identity this transport exposes to peers.
    ///
    /// `None` for TCP, which has none of its own; `Some` for Tox, whose 76
    /// character address is what a peer has to add.
    #[must_use]
    pub fn public_identity(&self) -> Option<String> {
        match self {
            Self::Tcp(_) => None,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => Some(tox.address().to_string()),
        }
    }
}

impl CoreTransport {
    /// Bring the transport up.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener cannot be bound or the Tox instance is
    /// no longer usable.
    pub async fn start(&mut self) -> MetaTextResult<()> {
        match self {
            Self::Tcp(network) => network.start().await,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.start(),
        }
    }

    /// Stop the transport, releasing every socket and worker thread.
    ///
    /// # Errors
    ///
    /// Returns an error when a clean stop is not possible; the caller logs it and
    /// continues, because shutdown must not be blocked by the transport.
    pub async fn shutdown(&mut self) -> MetaTextResult<()> {
        match self {
            Self::Tcp(network) => network.shutdown().await,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.shutdown().await,
        }
    }

    /// Announce the local nickname to peers.
    pub async fn set_nickname(&self, nickname: &str) {
        match self {
            Self::Tcp(network) => network.set_nickname(nickname).await,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.set_nickname(nickname).await,
        }
    }

    /// Announce the identity to peers.
    ///
    /// Only TCP needs it: those frames are encrypted with one session key, so the two
    /// ends of a pair need a way to agree on key material of their own. The whole key
    /// pair is handed over — the public half is announced, the secret half is what the
    /// key agreement ([`crate::identity::pair_key`]) uses locally. Tox encrypts between
    /// friends already and identifies a peer by its own address, so it ignores it.
    pub async fn set_identity(&self, identity: &NetworkIdentity) {
        match self {
            Self::Tcp(network) => network.set_identity(identity).await,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => {
                let _ = identity;
            }
        }
    }

    /// The key a directed message to `peer` should be sealed under, if the pair has
    /// one of its own.
    ///
    /// `None` means "use the session key": the peer announced no identity, is not
    /// connected, this session has no identity, or the transport encrypts per peer
    /// already (Tox). The core asks before it encrypts, because it — not the
    /// transport — owns the envelope for every transport that needs one.
    ///
    /// Not `async`: it reads local state (the announced identities) and derives a
    /// key, so there is nothing to wait for.
    #[must_use]
    pub fn contact_key(&self, peer: &str) -> Option<Vec<u8>> {
        match self {
            Self::Tcp(network) => network.contact_key(peer),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => {
                let _ = peer;
                None
            }
        }
    }

    /// Announce the local status message to peers.
    pub async fn set_status(&self, status: &str) {
        match self {
            // TCP has no status field to announce.
            Self::Tcp(_) => {
                let _ = status;
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.set_status(status).await,
        }
    }

    /// Register a peer and try to reach it.
    ///
    /// For TCP this dials `identifier` as `host:port`. For Tox it sends a friend
    /// request to the address and returns the new friend's public key, which the
    /// core can then store on the contact.
    ///
    /// # Errors
    ///
    /// Returns an error when the address is malformed or the peer cannot be
    /// registered.
    pub async fn request_peer(&self, identifier: &str) -> MetaTextResult<Option<String>> {
        match self {
            Self::Tcp(network) => {
                let remote = network.connect(identifier).await?;
                Ok(Some(remote.to_string()))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.request_peer(identifier).await.map(Some),
        }
    }
}

impl CoreTransport {
    /// Dial every address, reporting (but not failing on) individual errors.
    ///
    /// # Returns
    ///
    /// Returns how many peers were registered right away.
    pub async fn connect_all(&self, addresses: &[String]) -> usize {
        let mut connected = 0;
        for address in addresses {
            match self.request_peer(address).await {
                Ok(_) => connected += 1,
                Err(error) => {
                    warn!("⚠️ Could not reach {address}: {error}");
                }
            }
        }
        connected
    }

    /// Send an opaque payload to one peer.
    ///
    /// The payload is already encrypted when [`Self::provides_encryption`] is
    /// `false`; otherwise it is the plaintext body and the transport encrypts.
    /// `kind` and `content_type` let the transport tag the payload the way its
    /// own protocol does (TCP frame kinds, `TOX_MESSAGE_TYPE` for Tox).
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be routed or the send fails.
    pub async fn send_payload(
        &self,
        peer: &str,
        payload: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<SendOutcome> {
        match self {
            Self::Tcp(network) => {
                network.send_ciphertext_to_peer(peer, payload, kind, content_type)
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.send_payload(peer, payload, kind, content_type).await,
        }
    }

    /// Friend requests waiting for an answer.
    ///
    /// Empty for TCP, which has no friend-request concept.
    #[must_use]
    pub fn pending_requests(&self) -> Vec<PeerRequest> {
        match self {
            Self::Tcp(_) => Vec::new(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.pending_requests(),
        }
    }

    /// Accept a pending friend request.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport has no such request (including every
    /// TCP session).
    pub async fn accept_request(&self, public_key: &str) -> MetaTextResult<String> {
        match self {
            Self::Tcp(_) => {
                let _ = public_key;
                Err(network_error(
                    "the tcp transport has no friend requests to accept",
                    "accept_request",
                ))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.accept_request(public_key).await,
        }
    }

    /// Reject a pending friend request, without becoming friends.
    ///
    /// Not `async`: no transport is touched. Rejecting is a local decision (see
    /// [`ToxTransport::reject_request`]), so it cannot fail for a network reason.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport has no such request (including every
    /// TCP session).
    pub fn reject_request(&self, public_key: &str) -> MetaTextResult<String> {
        match self {
            Self::Tcp(_) => {
                let _ = public_key;
                Err(network_error(
                    "the tcp transport has no friend requests to reject",
                    "reject_request",
                ))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.reject_request(public_key),
        }
    }

    /// Whether incoming friend requests have to be answered by hand.
    ///
    /// `false` for TCP.
    #[must_use]
    pub const fn supports_friend_requests(&self) -> bool {
        match self {
            Self::Tcp(_) => false,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => true,
        }
    }

    /// Whether this transport can host group chats.
    ///
    /// `false` for TCP, which has no notion of a group: every group request
    /// against it fails with a typed error instead of pretending to work.
    #[must_use]
    pub const fn supports_groups(&self) -> bool {
        match self {
            Self::Tcp(_) => false,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(_) => true,
        }
    }

    /// Group invitations waiting for an answer.
    ///
    /// Empty for TCP, which has no group concept.
    #[must_use]
    pub fn pending_group_invites(&self) -> Vec<GroupInvite> {
        match self {
            Self::Tcp(_) => Vec::new(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.pending_group_invites(),
        }
    }

    /// The groups this session is in.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, or when the Tox snapshot
    /// cannot be read.
    pub async fn groups(&self) -> MetaTextResult<Vec<Group>> {
        match self {
            Self::Tcp(_) => Err(group_unsupported_error("list groups")),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.groups().await,
        }
    }

    /// Create a group and join it.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the title is
    /// rejected, or when toxcore cannot create the conference.
    pub async fn create_group(&self, title: &str) -> MetaTextResult<Group> {
        match self {
            Self::Tcp(_) => {
                let _ = title;
                Err(group_unsupported_error("create a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.create_group(title).await,
        }
    }

    /// Join a group from an invitation token.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the token is
    /// unknown or already used, or when toxcore refuses the join.
    pub async fn join_group(&self, token: &str) -> MetaTextResult<Group> {
        match self {
            Self::Tcp(_) => {
                let _ = token;
                Err(group_unsupported_error("join a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.join_group(token).await,
        }
    }

    /// Discard a pending group invitation without joining anything.
    ///
    /// Not `async`: no transport is touched. An invitation is a capability handed
    /// to us, so discarding it is a local decision (see
    /// [`ToxTransport::decline_group_invite`]) and cannot fail for a network
    /// reason.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport has no groups (TCP) or when no pending
    /// invitation matches `token`.
    pub fn decline_group_invite(&self, token: &str) -> MetaTextResult<String> {
        match self {
            Self::Tcp(_) => {
                let _ = token;
                Err(group_unsupported_error("decline a group invitation"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.decline_group_invite(token),
        }
    }

    /// Invite a peer to a group.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the group or the
    /// peer is unknown, or when the invitation cannot be sent.
    pub async fn invite_to_group(&self, group_id: &str, peer: &str) -> MetaTextResult<()> {
        match self {
            Self::Tcp(_) => {
                let _ = (group_id, peer);
                Err(group_unsupported_error("invite to a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.invite_to_group(group_id, peer).await,
        }
    }

    /// Send an already-encrypted payload to a group.
    ///
    /// The payload is the plaintext body when [`Self::provides_encryption`] is
    /// `true`; otherwise the caller has already sealed it.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the group is
    /// unknown, or when the message cannot be sent.
    pub async fn send_group_payload(
        &self,
        group_id: &str,
        payload: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<()> {
        match self {
            Self::Tcp(_) => {
                let _ = (group_id, payload, kind, content_type);
                Err(group_unsupported_error("send to a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => {
                tox.send_group_payload(group_id, payload, kind, content_type)
                    .await
            }
        }
    }

    /// Rename a group (its transport-level title).
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the group is
    /// unknown, or when the rename is refused.
    pub async fn rename_group(&self, group_id: &str, title: &str) -> MetaTextResult<()> {
        match self {
            Self::Tcp(_) => {
                let _ = (group_id, title);
                Err(group_unsupported_error("rename a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.rename_group(group_id, title).await,
        }
    }

    /// Leave a group.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP transport is asked, when the group is
    /// unknown, or when it cannot be left.
    pub async fn leave_group(&self, group_id: &str) -> MetaTextResult<()> {
        match self {
            Self::Tcp(_) => {
                let _ = group_id;
                Err(group_unsupported_error("leave a group"))
            }
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.leave_group(group_id).await,
        }
    }

    /// Payloads dropped because they waited longer than the outbox TTL.
    #[must_use]
    pub fn expired_messages(&self) -> u64 {
        match self {
            Self::Tcp(network) => network.expired_messages(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.expired_messages(),
        }
    }

    /// Payloads shed by a bounded buffer instead of being queued or delivered.
    ///
    /// Two buffers can shed. On Tox it is the callback hand-off (toxcore callbacks
    /// cannot block). On TCP it is a peer connection's outbound queue: a peer that
    /// stops reading while the core keeps sending fills it, and the excess is
    /// dropped and counted rather than queued without bound. A non-zero value
    /// therefore always means the same thing on both transports — a remote party
    /// could not absorb what was produced.
    #[must_use]
    pub fn dropped_payloads(&self) -> u64 {
        match self {
            Self::Tcp(network) => network.dropped_frames(),
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.dropped_payloads(),
        }
    }

    /// Friend requests shed because too many were already waiting.
    ///
    /// `0` for TCP, which has no friend requests.
    #[must_use]
    pub fn dropped_requests(&self) -> u64 {
        match self {
            Self::Tcp(_) => 0,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.dropped_requests(),
        }
    }

    /// Group invitations shed because too many were already waiting.
    ///
    /// `0` for TCP, which has no groups.
    #[must_use]
    pub fn dropped_invites(&self) -> u64 {
        match self {
            Self::Tcp(_) => 0,
            #[cfg(feature = "tox-protocol")]
            Self::Tox(tox) => tox.dropped_invites(),
        }
    }
}

/// Project a toxcore conference into the core's group vocabulary.
#[cfg(feature = "tox-protocol")]
fn group_from_tox(conference: crate::tox::ToxConference) -> Group {
    Group {
        id: conference.id,
        name: conference.title,
        members: conference.peers,
        joined: conference.connected,
    }
}

/// A group invitation waiting for an answer.
#[cfg(feature = "tox-protocol")]
#[derive(Debug, Clone)]
struct PendingInvite {
    /// The inviting friend's number, needed by `tox_conference_join`.
    friend_number: u32,

    /// Inviting peer's address (their public key when known).
    peer_id: String,

    /// The opaque toxcore cookie, as lowercase hexadecimal.
    token: String,

    /// Whether the invitation is for a text or an audio/video conference.
    conference_type: crate::tox::ToxConferenceType,
}

/// A friend known to the Tox instance.
#[cfg(feature = "tox-protocol")]
#[derive(Debug, Clone)]
struct FriendEntry {
    /// The friend's 64 character public key.
    public_key: String,
    /// The friend's announced nickname (empty until announced).
    name: String,
    /// Whether the friend is reachable right now.
    connected: bool,
}

/// A payload waiting for an offline friend to come online.
#[cfg(feature = "tox-protocol")]
#[derive(Debug, Clone)]
struct QueuedMessage {
    /// Identifier echoed back to the caller as the delivery id.
    message_id: u64,

    /// Payload body, already in its final transport form.
    body: Vec<u8>,

    /// Whether this is a chat message or a third-person action.
    kind: MessageKind,

    /// When it was queued, used to expire it.
    ///
    /// A wall clock rather than an `Instant`, because the queue is persisted and
    /// a `Duration` measured from process start cannot be aged across a restart.
    queued_at: SystemTime,
}

/// How long ago a payload was queued, saturating at zero.
///
/// The wall clock can move backwards (an NTP correction, a suspended laptop, a
/// hand-edited file with a timestamp in the future). A payload must not be
/// expired by that — it is a message somebody was told is waiting — so a
/// non-positive age is reported as zero, which keeps the payload for a full TTL
/// from the moment it is (re)read.
#[cfg(feature = "tox-protocol")]
fn queued_age(queued_at: SystemTime) -> Duration {
    SystemTime::now()
        .duration_since(queued_at)
        .unwrap_or(Duration::ZERO)
}

/// Map the domain message kind onto Tox's own message type.
#[cfg(feature = "tox-protocol")]
const fn tox_message_kind(kind: MessageKind) -> crate::tox::ToxMessageKind {
    match kind {
        MessageKind::Text => crate::tox::ToxMessageKind::Normal,
        MessageKind::Action => crate::tox::ToxMessageKind::Action,
    }
}

/// Map a Tox message type back onto the domain kind.
#[cfg(feature = "tox-protocol")]
const fn domain_message_kind(kind: crate::tox::ToxMessageKind) -> MessageKind {
    match kind {
        crate::tox::ToxMessageKind::Normal => MessageKind::Text,
        crate::tox::ToxMessageKind::Action => MessageKind::Action,
    }
}

/// The per-friend Tox outbox.
#[cfg(feature = "tox-protocol")]
type ToxOutbox = Arc<StdMutex<HashMap<u32, VecDeque<QueuedMessage>>>>;

/// Drop every entry older than [`TOX_OUTBOX_TTL`].
///
/// # Returns
///
/// Returns how many entries were dropped.
#[cfg(feature = "tox-protocol")]
fn prune_outbox(outbox: &ToxOutbox) -> usize {
    let mut outbox = lock(outbox);
    let mut dropped = 0;
    outbox.retain(|_, queue| {
        let before = queue.len();
        queue.retain(|entry| queued_age(entry.queued_at) < TOX_OUTBOX_TTL);
        dropped += before - queue.len();
        !queue.is_empty()
    });
    dropped
}

/// Total number of payloads waiting for an offline friend.
#[cfg(feature = "tox-protocol")]
fn outbox_len(outbox: &ToxOutbox) -> usize {
    lock(outbox).values().map(VecDeque::len).sum()
}

/// Send every payload queued for one friend that has just come online.
///
/// # Returns
///
/// Returns how many payloads were handed to toxcore. The flush stops at the
/// first failure so ordering is preserved and the remainder stays queued.
#[cfg(feature = "tox-protocol")]
fn flush_outbox(client: &ToxClient, outbox: &ToxOutbox, friend_number: u32) -> usize {
    let mut sent = 0;
    loop {
        let Some(next) = pop_outbox(outbox, friend_number) else {
            break;
        };

        if client
            .send_message_typed(friend_number, &next.body, tox_message_kind(next.kind))
            .is_err()
        {
            // Put it back at the front: the friend is not usable after all.
            lock(outbox)
                .entry(friend_number)
                .or_default()
                .push_front(next);
            break;
        }
        debug!(
            "📤 Delivered queued payload #{} to Tox friend #{friend_number}",
            next.message_id
        );
        sent += 1;
    }

    if sent > 0 {
        info!("📬 Flushed {sent} queued payload(s) to Tox friend #{friend_number}");
    }
    sent
}

/// Take the next queued payload for `friend_number`, forgetting the queue once it
/// is empty.
///
/// One helper rather than a `match` around the lock: it keeps the guard's scope
/// exactly as long as the mutation needs, which is what makes it obvious that no
/// lock is held across the `send_message_typed` call in [`flush_outbox`].
#[cfg(feature = "tox-protocol")]
fn pop_outbox(outbox: &ToxOutbox, friend_number: u32) -> Option<QueuedMessage> {
    let mut outbox = lock(outbox);
    let entry = outbox.get_mut(&friend_number)?.pop_front();
    if entry.is_none() {
        outbox.remove(&friend_number);
    }
    entry
}

/// The pieces of a stored state, translated into this run's vocabulary.
///
/// A `ToxState` is keyed by public key; the transport works in friend numbers,
/// which `toxcore` hands out per run. Translating one into the other is where a
/// restart can actually go wrong (an unknown key, an expired TTL, a queue that
/// grew beyond its cap), so the translation is a value with the reasons counted
/// rather than a set of inline `continue`s.
#[cfg(feature = "tox-protocol")]
#[derive(Debug, Default)]
struct RestoredState {
    /// Unanswered friend requests, oldest first.
    requests: Vec<PeerRequest>,

    /// Payloads per friend number, in delivery order.
    outbox: HashMap<u32, VecDeque<QueuedMessage>>,

    /// Identifier the next buffered payload is given, never below 1.
    next_message_id: u64,

    /// Requests refused because the list was already at capacity.
    requests_dropped: usize,

    /// Payloads dropped because their TTL had passed while the client was down.
    expired: usize,

    /// Payloads dropped because they can no longer be addressed: the key is not a
    /// friend (any more), the body is not valid hexadecimal, or the friend's
    /// queue was already at [`TOX_OUTBOX_CAPACITY`].
    undeliverable: usize,
}

/// Translate a stored state back into the live transport's vocabulary.
///
/// The friend table is read under its own lock, so this must be called with no
/// lock held — [`ToxStore::update`]'s snapshot closure and [`ToxTransport::new`]
/// both satisfy that.
#[cfg(feature = "tox-protocol")]
fn restore_state(state: &ToxState, friends: &StdMutex<HashMap<u32, FriendEntry>>) -> RestoredState {
    let mut restored = RestoredState::default();

    for stored in &state.requests {
        // A request is identified by its key, exactly like the live list; a
        // duplicate is not a new request, so it must not count as a refusal even
        // when the list is full (the same rule the bridge applies on arrival).
        let duplicate = restored
            .requests
            .iter()
            .any(|request| request.public_key.eq_ignore_ascii_case(&stored.public_key));
        if duplicate {
            continue;
        }
        if restored.requests.len() >= PENDING_REQUESTS_CAPACITY {
            restored.requests_dropped += 1;
            continue;
        }
        restored.requests.push(PeerRequest {
            public_key: stored.public_key.clone(),
            message: stored.message.clone(),
        });
    }

    let table = lock(friends);
    let by_key: HashMap<String, u32> = table
        .iter()
        .map(|(number, friend)| (friend.public_key.to_lowercase(), *number))
        .collect();
    drop(table);

    for message in &state.outbox {
        let Some(number) = by_key.get(&message.public_key.to_lowercase()).copied() else {
            restored.undeliverable += 1;
            continue;
        };
        let Ok(body) = hex::decode(&message.body) else {
            restored.undeliverable += 1;
            continue;
        };
        let queued_at = tox_store::from_unix_ms(message.queued_at_unix_ms);
        if queued_age(queued_at) >= TOX_OUTBOX_TTL {
            restored.expired += 1;
            continue;
        }
        let queue = restored.outbox.entry(number).or_default();
        // Drop-newest, like the runtime path: a queue that was already full when
        // the client stopped must not push out what it did keep.
        if queue.len() >= TOX_OUTBOX_CAPACITY {
            restored.undeliverable += 1;
            continue;
        }
        queue.push_back(QueuedMessage {
            message_id: message.message_id,
            body,
            kind: message.kind,
            queued_at,
        });
    }

    // A file written by a build that handed out higher ids must not be reused,
    // or two different payloads would share a delivery id.
    let highest = state
        .outbox
        .iter()
        .map(|message| message.message_id)
        .max()
        .unwrap_or(0);
    restored.next_message_id = state.next_message_id.max(highest.saturating_add(1)).max(1);
    restored
}

/// Snapshot the live state the way a restart has to read it back.
///
/// The outbox is keyed by public key rather than friend number (see
/// [`crate::tox_store`]), which needs the friend table; a queue whose friend is
/// somehow missing from that table is skipped, because there would be no way to
/// address it after a restart either. Friends are visited in ascending number
/// order so the file is deterministic.
#[cfg(feature = "tox-protocol")]
fn snapshot_state(
    friends: &StdMutex<HashMap<u32, FriendEntry>>,
    requests: &StdMutex<Vec<PeerRequest>>,
    outbox: &ToxOutbox,
    next_message_id: &AtomicU64,
) -> ToxState {
    // Each guard is scoped to the read that needs it: this runs on the actor's
    // path (a payload was queued) and on the bridge's (the queue was flushed or
    // pruned), so holding one lock past its own read would block the other side
    // for no reason.
    let requests = {
        let pending = lock(requests);
        pending
            .iter()
            .map(|request| StoredRequest {
                public_key: request.public_key.clone(),
                message: request.message.clone(),
            })
            .collect::<Vec<_>>()
    };

    let outbox = {
        let table = lock(friends);
        let queues = lock(outbox);
        let mut numbers: Vec<u32> = queues.keys().copied().collect();
        numbers.sort_unstable();

        let mut stored = Vec::new();
        for number in numbers {
            let (Some(friend), Some(queue)) = (table.get(&number), queues.get(&number)) else {
                continue;
            };
            stored.extend(queue.iter().map(|message| StoredMessage {
                public_key: friend.public_key.clone(),
                message_id: message.message_id,
                kind: message.kind,
                body: hex::encode(&message.body),
                queued_at_unix_ms: tox_store::unix_ms(message.queued_at),
            }));
        }
        // Released before the value is handed back: the guards cover the read
        // above and nothing else, so a bridge task that wants the friend table
        // (or a payload queued while this snapshot is taken) does not wait for
        // the write that follows.
        drop(queues);
        drop(table);
        stored
    };

    ToxState {
        version: STORE_VERSION,
        next_message_id: next_message_id.load(Ordering::SeqCst),
        requests,
        outbox,
    }
}

/// Read the store and translate it into this run's vocabulary.
///
/// The savedata's friend list is read first, and only when there is something to
/// translate: a restored queue is keyed by public key, and the friend numbers it
/// has to be re-expressed in are assigned per run. Every reason an entry can be
/// dropped is reported here, so the startup path stays short and a queue that was
/// partly discarded is visible in the log.
///
/// # Arguments
///
/// * `store` - The file to read
/// * `client` - The instance, asked for the savedata's friend list
/// * `friends` - The table to fill from that list
///
/// # Returns
///
/// Returns the requests, the outbox (per friend number) and the counters the
/// transport adopts at startup.
#[cfg(feature = "tox-protocol")]
fn restore_queue(
    store: &ToxStore,
    client: &ToxClient,
    friends: &StdMutex<HashMap<u32, FriendEntry>>,
) -> RestoredState {
    let stored = store.load();
    if !stored.is_empty() {
        // Seeded before the bridge starts, so a stored payload can be addressed
        // immediately rather than only once a callback has arrived.
        seed_friends(client, friends);
    }

    let restored = restore_state(&stored, friends);
    let messages: usize = restored.outbox.values().map(VecDeque::len).sum();
    if messages > 0 || !restored.requests.is_empty() {
        info!(
            "♻️ Restored {} pending friend request(s) and {messages} queued payload(s) from {}",
            restored.requests.len(),
            store.path().display()
        );
    }
    if restored.expired > 0 {
        info!(
            "⌛ Dropped {} queued payload(s) that expired while metaText was not running",
            restored.expired
        );
    }
    if restored.undeliverable > 0 {
        info!(
            "⚠️ Dropped {} queued payload(s) that can no longer be delivered",
            restored.undeliverable
        );
    }
    if restored.requests_dropped > 0 {
        debug!(
            "🚫 Refused {} restored friend request(s): {PENDING_REQUESTS_CAPACITY} are kept at most",
            restored.requests_dropped
        );
    }

    restored
}

/// Map a Tox failure onto the crate's transport error type.
#[cfg(feature = "tox-protocol")]
fn tox_error(error: ToxError) -> MetaTextError {
    // `ToxError::Unavailable` is a build/configuration problem, everything else
    // is a transport failure; both are reported without leaking internals.
    match error {
        ToxError::Unavailable => MetaTextError::Configuration {
            message: error.to_string(),
            source: None,
        },
        other => network_error(other.to_string(), "tox"),
    }
}

/// Run a blocking Tox command on the blocking pool.
///
/// `toxcore` is single-threaded and every [`ToxSender`] call blocks until the
/// worker answers, so calling one directly would stall the actor. A panic inside
/// the command is turned into an error instead of unwinding into the actor.
#[cfg(feature = "tox-protocol")]
async fn call_tox<T, F>(command: F) -> MetaTextResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> ToxResult<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(command).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(tox_error(error)),
        Err(join) => Err(network_error(
            format!("the Tox command task failed: {join}"),
            "tox",
        )),
    }
}

/// The Tox transport: a `libtoxcore` instance plus the bridge that turns its
/// callbacks into [`AppEvent`]s.
///
/// The instance is created in [`ToxTransport::new`] and driven by a dedicated
/// blocking task, because `toxcore` is not thread safe and must be iterated from
/// one thread. The actor only ever talks to this type.
#[cfg(feature = "tox-protocol")]
#[derive(Debug)]
pub struct ToxTransport {
    /// Cloneable command handle to the worker thread.
    sender: ToxSender,

    /// Local identity captured at startup.
    identity: ToxIdentity,

    /// Nickname mirrored locally so `/info` does not have to block.
    nickname: StdRwLock<String>,

    /// Where the identity is persisted.
    savedata: PathBuf,

    /// Where the pending requests and the outbox are persisted.
    ///
    /// Written whenever one of them changes, so no shutdown hook is needed and a
    /// crash between two changes loses at most the last one.
    store: ToxStore,

    /// Bootstrap nodes as `host:port`, for reporting.
    bootstrap: Vec<String>,

    /// Identifiers this transport was asked to reach.
    desired: StdMutex<Vec<String>>,

    /// Friend table, updated by the bridge task.
    friends: Arc<StdMutex<HashMap<u32, FriendEntry>>>,

    /// Friend requests waiting for an answer.
    requests: Arc<StdMutex<Vec<PeerRequest>>>,

    /// Group invitations waiting for an answer, with the capability needed to
    /// join. An entry is removed when it is used, so a token cannot be replayed.
    invites: Arc<StdMutex<Vec<PendingInvite>>>,

    /// Group titles by conference id, so a message can be named without asking
    /// toxcore. Written by the bridge and by the group operations.
    group_names: Arc<StdMutex<HashMap<String, String>>>,

    /// Payloads waiting for an offline friend.
    outbox: ToxOutbox,

    /// Next identifier handed out to a queued payload.
    next_message_id: Arc<AtomicU64>,

    /// How many queued payloads expired before they could be delivered.
    expired: Arc<AtomicU64>,

    /// Friend requests refused because the pending list was full.
    dropped_requests: Arc<AtomicU64>,

    /// Group invitations refused because the pending list was full.
    dropped_invites: Arc<AtomicU64>,

    /// Set to stop the bridge task.
    stop: Arc<AtomicBool>,

    /// The bridge task, joined on shutdown.
    bridge: Option<tokio::task::JoinHandle<()>>,

    /// Configured UDP port (`0` when toxcore picks one).
    port: u16,

    /// Configured upper bound on concurrent peers.
    max_connections: u32,

    /// Whether [`ToxTransport::start`] was called.
    started: bool,
}

#[cfg(feature = "tox-protocol")]
impl ToxTransport {
    /// Create the instance and start its bridge task.
    ///
    /// The identity is loaded from `<data_dir>/tox-savedata.bin` when present,
    /// so the address a friend saved keeps working across restarts.
    ///
    /// # Errors
    ///
    /// Returns an error when `libtoxcore` is unavailable, the savedata cannot be
    /// loaded, the configuration is out of toxcore's range, or `--bootstrap` is
    /// not a `host:port:PUBLIC_KEY` triple.
    pub fn new(
        config: &AppConfig,
        args: &CliArgs,
        event_sender: mpsc::Sender<AppEvent>,
    ) -> MetaTextResult<Self> {
        let data_dir = args.data_directory().unwrap_or_else(|| PathBuf::from("."));
        let nickname = args
            .nickname
            .clone()
            .unwrap_or_else(|| config.app.default_nickname.clone());

        let mut tox_config = ToxConfig::new(data_dir, nickname);
        tox_config
            .status_message
            .clone_from(&config.app.default_status);
        tox_config.bootstrap_nodes = tox::default_bootstrap_nodes();
        // The documented `[network]` switch is enforced here: toxcore uses it to
        // decide whether the DHT and direct peer connections may use IPv6.
        tox_config.ipv6_enabled = config.network.enable_ipv6;

        // An explicit `--bootstrap host:port:key` is dialled first so a private
        // node is preferred over the public list.
        if let Some(entry) = &args.bootstrap_node {
            let node = BootstrapNode::parse(entry).map_err(tox_error)?;
            tox_config.bootstrap_nodes.insert(0, node);
        }

        // `--port` pins the UDP port; leaving it unset lets the OS choose. The
        // TCP listener port from `config.toml` is deliberately *not* reused here:
        // it belongs to the TCP transport, and pinning toxcore to it would make
        // the Tox transport fail to start whenever that port is taken.
        let udp_port = args.port.unwrap_or(0);
        if udp_port != 0 {
            tox_config.start_port = udp_port;
            tox_config.end_port = udp_port;
        }

        let bootstrap: Vec<String> = tox_config
            .bootstrap_nodes
            .iter()
            .map(|node| format!("{}:{}", node.host, node.port))
            .collect();

        let client = ToxClient::start(tox_config).map_err(tox_error)?;
        let identity = client.identity().clone();
        let sender = client.sender();
        let savedata = client.savedata_path().to_path_buf();
        let nickname = identity.nickname.clone();

        // The queue sits next to the savedata, so "the data directory" means the
        // same thing for the identity and for what was in flight. `new` reports
        // nothing to restore as cheaply as it reports a restored queue: a missing
        // file is simply the first run.
        let store = ToxStore::new(savedata.parent().unwrap_or_else(|| Path::new(".")));
        let friends = Arc::new(StdMutex::new(HashMap::new()));
        let restored = restore_queue(&store, &client, &friends);

        let requests = Arc::new(StdMutex::new(restored.requests));
        let invites = Arc::new(StdMutex::new(Vec::new()));
        let group_names = Arc::new(StdMutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let outbox: ToxOutbox = Arc::new(StdMutex::new(restored.outbox));
        // Ids start at 1 so that 0 can be used as "no message", like the TCP
        // transport does; a restored queue continues the ids it handed out.
        let next_message_id = Arc::new(AtomicU64::new(restored.next_message_id));
        let expired = Arc::new(AtomicU64::new(restored.expired as u64));
        let dropped_requests = Arc::new(AtomicU64::new(0));
        let dropped_invites = Arc::new(AtomicU64::new(0));

        let bridge = spawn_bridge(
            client,
            event_sender,
            Arc::clone(&friends),
            Arc::clone(&requests),
            Arc::clone(&invites),
            Arc::clone(&group_names),
            Arc::clone(&outbox),
            Arc::clone(&next_message_id),
            Arc::clone(&expired),
            Arc::clone(&dropped_requests),
            Arc::clone(&dropped_invites),
            store.clone(),
            Arc::clone(&stop),
        );

        info!("🧪 Tox transport ready: {}", identity.address);

        Ok(Self {
            sender,
            identity,
            nickname: StdRwLock::new(nickname),
            savedata,
            store,
            bootstrap,
            desired: StdMutex::new(Vec::new()),
            friends,
            requests,
            invites,
            group_names,
            outbox,
            next_message_id,
            expired,
            dropped_requests,
            dropped_invites,
            stop,
            bridge: Some(bridge),
            port: udp_port,
            max_connections: config.network.max_connections,
            started: true,
        })
    }

    /// The 76 character Tox address other users have to add.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.identity.address
    }

    /// Where the identity is persisted.
    #[must_use]
    pub fn savedata_path(&self) -> &std::path::Path {
        &self.savedata
    }

    /// Where the pending friend requests and the outbox are persisted.
    ///
    /// Next to the savedata, and rewritten whenever either changes; see
    /// [`crate::tox_store`].
    #[must_use]
    pub fn store_path(&self) -> &std::path::Path {
        self.store.path()
    }
    /// Whether the transport is considered up.
    #[must_use]
    pub const fn is_started(&self) -> bool {
        self.started
    }

    /// Configured UDP port (`0` when toxcore picks one).
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Bootstrap nodes as `host:port`.
    #[must_use]
    pub fn bootstrap_nodes(&self) -> &[String] {
        &self.bootstrap
    }

    /// Configured upper bound on concurrent peers.
    #[must_use]
    pub const fn max_connections(&self) -> u32 {
        self.max_connections
    }
}

#[cfg(feature = "tox-protocol")]
impl ToxTransport {
    /// Number of friends currently reachable.
    #[must_use]
    pub fn connected_peers(&self) -> usize {
        lock(&self.friends)
            .values()
            .filter(|friend| friend.connected)
            .count()
    }

    /// Number of friends that are known but not reachable yet.
    #[must_use]
    pub fn pending_peers(&self) -> usize {
        lock(&self.friends)
            .values()
            .filter(|friend| !friend.connected)
            .count()
    }

    /// Payloads waiting for their destination.
    ///
    /// Expired entries are dropped here; the actor polls this once per tick, so
    /// a queue that is never flushed cannot hold stale payloads forever — and
    /// because an expiry is also a change to what the store holds, it is
    /// published too.
    #[must_use]
    pub fn queued_messages(&self) -> usize {
        let expired = prune_outbox(&self.outbox);
        if expired > 0 {
            self.expired.fetch_add(expired as u64, Ordering::SeqCst);
            self.persist();
        }
        outbox_len(&self.outbox)
    }

    /// How many queued payloads expired before they could be delivered.
    #[must_use]
    pub fn expired_messages(&self) -> u64 {
        self.expired.load(Ordering::SeqCst)
    }

    /// Friend requests refused because the pending list was full.
    ///
    /// Non-zero means somebody (or a flood) sent more requests than
    /// [`PENDING_REQUESTS_CAPACITY`] allows to wait at once. The list is bounded
    /// because a request only needs our Tox address to be produced.
    #[must_use]
    pub fn dropped_requests(&self) -> u64 {
        self.dropped_requests.load(Ordering::SeqCst)
    }

    /// Group invitations refused because the pending list was full.
    #[must_use]
    pub fn dropped_invites(&self) -> u64 {
        self.dropped_invites.load(Ordering::SeqCst)
    }

    /// Payloads the bridge had to shed because its bounded hand-off was full.
    ///
    /// toxcore callbacks run on the worker thread and cannot block, so an
    /// overloaded consumer loses the *newest* event instead of growing memory
    /// without bound. A non-zero value means the core could not keep up.
    #[must_use]
    pub fn dropped_payloads(&self) -> u64 {
        self.sender.dropped_events()
    }

    /// Identifiers this transport was asked to reach.
    #[must_use]
    pub fn desired_peers(&self) -> Vec<String> {
        lock(&self.desired).clone()
    }

    /// Nicknames announced by the reachable friends.
    #[must_use]
    pub fn peer_nicknames(&self) -> Vec<String> {
        lock(&self.friends)
            .values()
            .filter(|friend| friend.connected && !friend.name.is_empty())
            .map(|friend| friend.name.clone())
            .collect()
    }

    /// Friend requests waiting for an answer.
    #[must_use]
    pub fn pending_requests(&self) -> Vec<PeerRequest> {
        lock(&self.requests).clone()
    }
}

/// Extract the public key from an identifier.
///
/// A Tox address is `public key (32 B) || nospam (4 B) || checksum (2 B)`, so
/// the leading 64 hexadecimal characters are the key. Anything shorter is
/// returned unchanged (it is already a key, or an unusable value the caller will
/// report).
///
/// The cut is taken in characters, not bytes: the identifier is whatever the user
/// typed after `/add`, so it may contain multi-byte text, and slicing 64 bytes of
/// it panicked whenever the 64th byte fell inside a character — taking the core
/// down from a command line.
#[cfg(feature = "tox-protocol")]
fn public_key_prefix(identifier: &str) -> String {
    let key_chars = crate::tox::PUBLIC_KEY_SIZE * 2;
    identifier.chars().take(key_chars).collect()
}

/// Lock a shared table, recovering a poisoned mutex instead of panicking.
///
/// A poisoned lock means another thread panicked while holding it. The tables
/// here are only ever replaced or appended to, so the data is still structurally
/// usable; recovering keeps one unlucky thread from taking the service down.
#[cfg(feature = "tox-protocol")]
fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("⚠️ Recovered a poisoned Tox state lock");
            poisoned.into_inner()
        }
    }
}

#[cfg(feature = "tox-protocol")]
impl ToxTransport {
    /// Mark the transport as up.
    ///
    /// The instance already runs (it was created in [`ToxTransport::new`]), so
    /// this only records that the actor reached its start phase.
    ///
    /// # Errors
    ///
    /// Never fails today; the `Result` keeps the transport surface uniform.
    pub const fn start(&mut self) -> MetaTextResult<()> {
        self.started = true;
        Ok(())
    }

    /// Stop the bridge task and persist the identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the bridge task did not stop cleanly. The identity
    /// is persisted by the bridge before it exits, so a reported failure still
    /// leaves the savedata on disk.
    pub async fn shutdown(&mut self) -> MetaTextResult<()> {
        self.stop.store(true, Ordering::SeqCst);
        let Some(bridge) = self.bridge.take() else {
            return Ok(());
        };
        self.started = false;

        match bridge.await {
            Ok(()) => {
                info!("✅ Tox transport stopped");
                Ok(())
            }
            Err(join) => Err(network_error(
                format!("the Tox bridge did not stop cleanly: {join}"),
                "tox shutdown",
            )),
        }
    }

    /// Change the nickname announced to friends.
    pub async fn set_nickname(&self, nickname: &str) {
        let owned = nickname.to_string();
        let sender = self.sender.clone();
        let request = owned.clone();
        match call_tox(move || sender.set_nickname(&request)).await {
            Ok(()) => {
                if let Ok(mut current) = self.nickname.write() {
                    current.clone_from(&owned);
                }
            }
            Err(error) => warn!("⚠️ Could not set the Tox nickname: {error}"),
        }
    }

    /// Change the personal status message.
    pub async fn set_status(&self, status: &str) {
        let request = status.to_string();
        let sender = self.sender.clone();
        if let Err(error) = call_tox(move || sender.set_status_message(&request)).await {
            warn!("⚠️ Could not set the Tox status message: {error}");
        }
    }
}

#[cfg(feature = "tox-protocol")]
impl ToxTransport {
    /// Send a friend request and return the target's public key.
    ///
    /// # Errors
    ///
    /// Returns an error when the address is not a 76 character Tox address or
    /// toxcore refuses the request (bad checksum, self, already a friend, ...).
    pub async fn request_peer(&self, identifier: &str) -> MetaTextResult<String> {
        let trimmed = identifier.trim().to_uppercase();
        let public_key = public_key_prefix(&trimmed);

        // The message is what the other side sees before deciding; use the local
        // nickname so the request is identifiable.
        let nickname = self
            .nickname
            .read()
            .map(|name| name.clone())
            .unwrap_or_default();
        let message = if nickname.trim().is_empty() {
            "metaText friend request".to_string()
        } else {
            format!("{nickname} via metaText")
        };

        let address = trimmed.clone();
        let sender = self.sender.clone();
        let friend_number = call_tox(move || sender.add_friend(&address, &message)).await?;
        debug!("🤝 Tox friend request sent to {public_key} (friend #{friend_number})");

        self.remember_desired(&trimmed);
        // Ask for a snapshot so `pending_peers` reflects the new friend before
        // the bridge has seen a connection event.
        self.refresh_friends_async().await;

        Ok(public_key)
    }

    /// Send an opaque payload to a friend.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Network`] when no friend matches `peer` or
    /// toxcore refuses the send.
    pub async fn send_payload(
        &self,
        peer: &str,
        payload: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<SendOutcome> {
        let Some(friend_number) = self.resolve_friend(peer) else {
            return Err(tox_error(ToxError::UnknownFriend(peer.to_string())));
        };

        // Tox messages are opaque byte arrays with no content-type field, so the
        // receiver sniffs UTF-8 validity (`forward_event`). A binary body
        // therefore travels unchanged and is classified on arrival instead of
        // being declared here.
        let _ = content_type;

        // `tox_friend_send_message` blocks until the worker answers, so it runs
        // on the blocking pool and cannot stall the actor.
        let sender = self.sender.clone();
        let body = payload.to_vec();
        let tox_kind = tox_message_kind(kind);
        match tokio::task::spawn_blocking(move || {
            sender.send_message_typed(friend_number, &body, tox_kind)
        })
        .await
        {
            Ok(Ok(message_id)) => Ok(SendOutcome::Sent(DeliveryReceipt {
                message_id: u64::from(message_id),
                peers: 1,
            })),
            // The friend is known but offline: buffer the payload and report it
            // as queued, exactly like the TCP transport does.
            Ok(Err(ToxError::SendMessage(3, _))) => Ok(self.enqueue(friend_number, payload, kind)),
            Ok(Err(error)) => Err(tox_error(error)),
            Err(join) => Err(network_error(
                format!("the Tox send task failed: {join}"),
                "send_message",
            )),
        }
    }

    /// Append a payload to a friend's outbox.
    ///
    /// Expired entries are pruned first so a queue that is never flushed cannot
    /// hold stale payloads; a full queue is reported as
    /// [`SendOutcome::Dropped`] rather than silently growing. What is queued is
    /// published to the store, so the payload survives a restart — the reason
    /// the outbox exists at all.
    fn enqueue(&self, friend_number: u32, payload: &[u8], kind: MessageKind) -> SendOutcome {
        let expired = prune_outbox(&self.outbox);
        self.expired.fetch_add(expired as u64, Ordering::SeqCst);

        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        let mut outbox = lock(&self.outbox);
        let queue = outbox.entry(friend_number).or_default();
        if queue.len() >= TOX_OUTBOX_CAPACITY {
            return SendOutcome::Dropped;
        }
        queue.push_back(QueuedMessage {
            message_id,
            body: payload.to_vec(),
            kind,
            queued_at: SystemTime::now(),
        });
        // The position is read while the guard is alive, so the lock is released
        // before the outcome is built rather than at the end of the function.
        let position = queue.len();
        drop(outbox);
        self.persist();
        SendOutcome::Queued {
            message_id,
            position,
        }
    }

    /// Forget a pending friend request without accepting it.
    ///
    /// toxcore has no "reject": an incoming request is only a proposal, and not
    /// answering it *is* the refusal — there is no state on either side to undo and
    /// no notification to send. What this does is drop the entry from the pending
    /// list, which matters because that list is bounded
    /// ([`PENDING_REQUESTS_CAPACITY`]): without a way to discard one, a flood could
    /// only be cleared by accepting the flood, and it would keep refusing the
    /// requests a user actually wants to see.
    ///
    /// # Errors
    ///
    /// Returns an error when no pending request matches `public_key`.
    pub fn reject_request(&self, public_key: &str) -> MetaTextResult<String> {
        let wanted = public_key.trim().to_uppercase();
        let removed = {
            let mut requests = lock(&self.requests);
            let Some(position) = requests
                .iter()
                .position(|request| request.public_key.eq_ignore_ascii_case(&wanted))
            else {
                return Err(network_error(
                    format!("no pending friend request matches '{public_key}'"),
                    "reject_request",
                ));
            };
            requests.remove(position).public_key
        };
        // The stored list has to agree with the live one, or a rejected request
        // would be offered again after a restart.
        self.persist();
        Ok(removed)
    }

    /// Accept a pending friend request.
    ///
    /// # Errors
    ///
    /// Returns an error when no pending request matches `public_key` or toxcore
    /// refuses the accept.
    pub async fn accept_request(&self, public_key: &str) -> MetaTextResult<String> {
        let wanted = public_key.trim().to_uppercase();
        // The key is cloned out of the guard so the guard can be released at the
        // end of this statement, rather than being held across the toxcore call
        // below.
        let known = lock(&self.requests)
            .iter()
            .find(|request| request.public_key.eq_ignore_ascii_case(&wanted))
            .map(|request| request.public_key.clone());
        let Some(key) = known else {
            return Err(network_error(
                format!("no pending friend request matches '{public_key}'"),
                "accept_request",
            ));
        };

        let owned = key.clone();
        let sender = self.sender.clone();
        let friend_number = call_tox(move || sender.accept_friend(&owned)).await?;

        // The request is spent only now. Removing it before the call would lose it
        // when toxcore refuses (for example because the friend list is full), and
        // the requester would have to send it again for a retry that never needed
        // to happen — the same mistake A35 fixed for group invitations.
        lock(&self.requests).retain(|request| !request.public_key.eq_ignore_ascii_case(&key));
        // The request is answered, so it must not come back as pending after a
        // restart either.
        self.persist();
        info!("✅ Accepted Tox friend request from {key} (friend #{friend_number})");

        self.remember_desired(&key);
        self.refresh_friends_async().await;
        Ok(key)
    }

    /// Group invitations waiting for an answer.
    #[must_use]
    pub fn pending_group_invites(&self) -> Vec<GroupInvite> {
        lock(&self.invites)
            .iter()
            .map(|invite| GroupInvite {
                peer: lock(&self.friends)
                    .get(&invite.friend_number)
                    .map_or_else(String::new, |friend| friend.name.clone()),
                peer_id: invite.peer_id.clone(),
                token: invite.token.clone(),
            })
            .collect()
    }

    /// The groups this instance is in, refreshed from toxcore.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be read.
    pub async fn groups(&self) -> MetaTextResult<Vec<Group>> {
        let sender = self.sender.clone();
        let snapshot = call_tox(move || sender.snapshot()).await?;
        let groups: Vec<Group> = snapshot
            .conferences
            .into_iter()
            .map(group_from_tox)
            .collect();
        // Keep the title cache warm so a later group message needs no lookup.
        for group in &groups {
            remember_group(&self.group_names, &group.id, &group.name);
        }
        Ok(groups)
    }

    /// Create a text conference and remember it.
    ///
    /// # Errors
    ///
    /// Returns an error when the title is rejected or toxcore refuses.
    pub async fn create_group(&self, title: &str) -> MetaTextResult<Group> {
        let sender = self.sender.clone();
        let owned = title.to_string();
        let conference = call_tox(move || sender.conference_new(&owned)).await?;
        let group = group_from_tox(conference);
        remember_group(&self.group_names, &group.id, &group.name);
        info!("👥 Created Tox group '{}' ({})", group.name, group.id);
        Ok(group)
    }

    /// Join a group using a single-use invitation token.
    ///
    /// The token is spent by a join that *succeeded*, not by an attempt: a
    /// transient failure (`FAIL_SEND`, a friend that is momentarily unreachable)
    /// leaves the invitation in place so it can be retried. Consuming it up front
    /// would turn "not right now" into "ask your peer for a new invitation". A
    /// token that has been joined with is gone, so it still cannot be replayed.
    ///
    /// # Errors
    ///
    /// Returns an error when no invitation matches `token`, when the invitation
    /// is for an audio/video conference, or when toxcore refuses the join.
    pub async fn join_group(&self, token: &str) -> MetaTextResult<Group> {
        let wanted = token.trim().to_lowercase();
        // Copied out of the guard so it can be released here: the join below is an
        // await, and holding a lock across it would block the bridge task too.
        let found = lock(&self.invites)
            .iter()
            .find(|invite| invite.token == wanted)
            .map(|invite| (invite.friend_number, invite.conference_type));
        let Some((friend_number, conference_type)) = found else {
            return Err(network_error(
                format!("no pending group invitation matches '{token}'"),
                "join_group",
            ));
        };

        if conference_type != crate::tox::ToxConferenceType::Text {
            // Nothing can ever consume an audio/video invitation, so leaving it in
            // the list would only offer a join that always fails.
            lock(&self.invites).retain(|invite| invite.token != wanted);
            return Err(network_error(
                "the invitation is for an audio/video conference, which this client cannot join",
                "join_group",
            ));
        }

        let cookie = hex::decode(&wanted).map_err(|_| {
            network_error(
                "the invitation token is not valid hexadecimal",
                "join_group",
            )
        })?;
        let sender = self.sender.clone();
        let conference = call_tox(move || sender.conference_join(friend_number, &cookie)).await?;

        // The join succeeded, so the token has been spent.
        lock(&self.invites).retain(|invite| invite.token != wanted);

        let group = group_from_tox(conference);
        remember_group(&self.group_names, &group.id, &group.name);
        info!("👥 Joined Tox group '{}' ({})", group.name, group.id);
        Ok(group)
    }

    /// Discard a pending group invitation without joining anything.
    ///
    /// The twin of [`ToxTransport::reject_request`] on the invitation side, and
    /// for the same reason: the pending list is bounded
    /// ([`PENDING_INVITES_CAPACITY`]), so an invitation a user does not want could
    /// previously only be cleared by joining the conference and leaving it, and a
    /// flood could crowd out the invitation that *is* wanted.
    ///
    /// Nothing is sent to the inviter. toxcore has no "decline": the invitation is
    /// a cookie handed to us, so dropping it is invisible on the other side — which
    /// is also why this cannot fail for a network reason. It is *not* an implicit
    /// block either: the same peer can invite us again, exactly like a refused
    /// friend request can be repeated.
    ///
    /// # Errors
    ///
    /// Returns an error when no pending invitation matches `token`.
    pub fn decline_group_invite(&self, token: &str) -> MetaTextResult<String> {
        let wanted = token.trim().to_lowercase();
        if wanted.is_empty() {
            return Err(network_error(
                "a group invitation token is required",
                "decline_group_invite",
            ));
        }

        let mut invites = lock(&self.invites);
        let Some(position) = invites.iter().position(|invite| invite.token == wanted) else {
            return Err(network_error(
                format!("no pending group invitation matches '{token}'"),
                "decline_group_invite",
            ));
        };
        let removed = invites.remove(position).token;
        drop(invites);
        info!(
            "🚫 Discarded the group invitation {}",
            crate::utils::abbreviate(&removed)
        );
        Ok(removed)
    }

    /// Invite a friend to a group.
    ///
    /// # Errors
    ///
    /// Returns an error when the friend or the group is unknown, or toxcore
    /// refuses the invitation.
    pub async fn invite_to_group(&self, group_id: &str, peer: &str) -> MetaTextResult<()> {
        let Some(friend_number) = self.resolve_friend(peer) else {
            return Err(tox_error(ToxError::UnknownFriend(peer.to_string())));
        };
        let sender = self.sender.clone();
        let owned = group_id.to_string();
        call_tox(move || sender.conference_invite(&owned, friend_number)).await
    }

    /// Send an opaque payload to a group.
    ///
    /// Tox conferences carry no content type, so (as for a friend message) the
    /// bytes travel unchanged and the receiver classifies them.
    ///
    /// # Errors
    ///
    /// Returns an error when the group is unknown or the message cannot be sent.
    pub async fn send_group_payload(
        &self,
        group_id: &str,
        payload: &[u8],
        kind: MessageKind,
        content_type: ContentType,
    ) -> MetaTextResult<()> {
        let _ = content_type;
        let sender = self.sender.clone();
        let owned = group_id.to_string();
        let body = payload.to_vec();
        let tox_kind = tox_message_kind(kind);
        call_tox(move || sender.conference_send(&owned, &body, tox_kind)).await
    }

    /// Rename a group (its conference title).
    ///
    /// The title belongs to the conference, so toxcore forwards it to every peer,
    /// which learns it through its `conference_title` callback
    /// ([`crate::tox::ToxEvent::ConferenceTitleChanged`]). Our **own** callback
    /// does not fire for our own change, so the new title is cached here as well,
    /// which is also what keeps later messages labelled with the new name.
    ///
    /// # Errors
    ///
    /// Returns an error when the title is too long, the group is unknown or
    /// toxcore refuses the change.
    pub async fn rename_group(&self, group_id: &str, title: &str) -> MetaTextResult<()> {
        let sender = self.sender.clone();
        let owned = group_id.to_string();
        let wanted = title.to_string();
        call_tox(move || sender.conference_set_title(&owned, &wanted)).await?;
        remember_group(&self.group_names, group_id, title);
        info!("👥 Renamed Tox group {group_id} to '{title}'");
        Ok(())
    }

    /// Leave a group.
    ///
    /// # Errors
    ///
    /// Returns an error when the group is unknown or toxcore refuses.
    pub async fn leave_group(&self, group_id: &str) -> MetaTextResult<()> {
        let sender = self.sender.clone();
        let owned = group_id.to_string();
        call_tox(move || sender.conference_leave(&owned)).await?;
        // The conference is gone, so its cached title is too: the cache must not
        // grow one entry per conference ever joined.
        forget_group(&self.group_names, group_id);
        Ok(())
    }
}

/// Stopping on drop is what keeps a *dropped* transport from wedging teardown.
///
/// [`ToxTransport::shutdown`] is the orderly path, but it is skipped whenever the
/// owner never gets to call it: an actor torn down by a runtime drop, or a
/// front-end/test that panics before its `shutdown().await`. The bridge runs on
/// `tokio::task::spawn_blocking`, and a tokio runtime waits for its blocking tasks
/// when it is dropped — so a bridge that was never told to stop keeps the runtime
/// waiting forever and the process hangs instead of exiting.
///
/// The loop already re-reads the flag every `TOX_EVENT_POLL` (200 ms) and the
/// bridge owns its own clone, so setting it here is enough for the loop to exit
/// and run the savedata write the bridge performs on the way out. Nothing here can
/// block: `Drop` must not, and it does not need to — the write happens on the
/// bridge thread, which the runtime is already waiting for.
#[cfg(feature = "tox-protocol")]
impl Drop for ToxTransport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(feature = "tox-protocol")]
impl ToxTransport {
    /// Publish the current pending requests and outbox to the store.
    ///
    /// Called after every mutation of either, which is what makes the file a
    /// durability guarantee rather than a best-effort snapshot taken at some
    /// other time: whatever a front-end was last told is what a restart reads
    /// back. A failed write is reported and ignored — the queue is still in
    /// memory, so the cost is durability, not delivery.
    fn persist(&self) {
        if let Err(error) = self.store.update(|| {
            snapshot_state(
                &self.friends,
                &self.requests,
                &self.outbox,
                &self.next_message_id,
            )
        }) {
            warn!(
                "⚠️ Could not persist the Tox queue to {}: {error}",
                self.store.path().display()
            );
        }
    }

    /// Refresh the friend table without blocking the actor task.
    ///
    /// `ToxSender::snapshot` blocks until the toxcore worker answers, so it must
    /// run on the blocking pool; stalling the actor here would freeze every
    /// front-end.
    async fn refresh_friends_async(&self) {
        let sender = self.sender.clone();
        match tokio::task::spawn_blocking(move || sender.snapshot()).await {
            Ok(Ok(snapshot)) => store_friends(&self.friends, snapshot.friends),
            Ok(Err(error)) => warn!("⚠️ Could not refresh the Tox friend list: {error}"),
            Err(join) => warn!("⚠️ The Tox snapshot task failed: {join}"),
        }
    }

    /// Look up a friend number from a public key, Tox address or nickname.
    fn resolve_friend(&self, peer: &str) -> Option<u32> {
        let trimmed = peer.trim();
        let wanted = public_key_prefix(&trimmed.to_uppercase());
        lock(&self.friends)
            .iter()
            .find(|(_, friend)| {
                friend.public_key.eq_ignore_ascii_case(&wanted)
                    || friend.name.eq_ignore_ascii_case(trimmed)
            })
            .map(|(number, _)| *number)
    }

    /// Remember an identifier in the desired-peer list (deduplicated).
    fn remember_desired(&self, identifier: &str) {
        let mut desired = lock(&self.desired);
        if !desired.iter().any(|entry| entry == identifier) {
            desired.push(identifier.to_string());
        }
    }
}

/// Start the blocking task that turns Tox callbacks into [`AppEvent`]s.
///
/// The task owns [`ToxClient`] (single consumer), seeds the friend table, then
/// The shared state the Tox bridge maintains on behalf of the core.
///
/// The pieces are independently locked (a slow friend-table update must not block
/// an invite), but they are always needed together, so they travel as one value:
/// [`forward_event`] reads better with one parameter than with five.
#[cfg(feature = "tox-protocol")]
struct BridgeState {
    /// Friend table, updated by the bridge task.
    friends: Arc<StdMutex<HashMap<u32, FriendEntry>>>,

    /// Friend requests waiting for an answer.
    requests: Arc<StdMutex<Vec<PeerRequest>>>,

    /// Group invitations waiting for an answer.
    invites: Arc<StdMutex<Vec<PendingInvite>>>,

    /// Group titles by conference id.
    group_names: Arc<StdMutex<HashMap<String, String>>>,

    /// Payloads waiting for an offline friend.
    outbox: ToxOutbox,

    /// Identifier the next buffered payload is given; only read for the store.
    next_message_id: Arc<AtomicU64>,

    /// Payloads dropped because they expired; the bridge prunes on every
    /// connection event, so it is the one that observes most expiries.
    expired: Arc<AtomicU64>,

    /// Friend requests refused because the pending list was full.
    dropped_requests: Arc<AtomicU64>,

    /// Group invitations refused because the pending list was full.
    dropped_invites: Arc<AtomicU64>,

    /// Where the pending requests and the outbox are persisted.
    store: ToxStore,
}

#[cfg(feature = "tox-protocol")]
impl BridgeState {
    /// Publish the pending requests and the outbox, exactly as the transport
    /// side does after a mutation of its own.
    ///
    /// The bridge adds a friend request and drains the outbox when a friend comes
    /// online, so it owns two of the four mutations; without this call a restart
    /// would resurrect a request that had been answered, or lose a payload that
    /// had been queued.
    fn persist(&self) {
        if let Err(error) = self.store.update(|| {
            snapshot_state(
                &self.friends,
                &self.requests,
                &self.outbox,
                &self.next_message_id,
            )
        }) {
            warn!(
                "⚠️ Could not persist the Tox queue to {}: {error}",
                self.store.path().display()
            );
        }
    }
}

/// Spawn the bridge task for a started instance.
///
/// forwards events into the core's bounded inbox until asked to stop. It
/// persists the savedata and joins the toxcore worker on the way out, so the
/// identity is never lost between runs.
///
/// The parameter list is the set of handles the bridge task shares with the
/// transport it belongs to (friend table, pending requests, pending invitations,
/// group titles, outbox, counters, the store). They are separate `Arc`s rather
/// than fields of one struct because the transport mutates them from the async
/// side while the bridge mutates them from the event loop; grouping them would
/// only move the list one indirection away.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "tox-protocol")]
fn spawn_bridge(
    client: ToxClient,
    events: mpsc::Sender<AppEvent>,
    friends: Arc<StdMutex<HashMap<u32, FriendEntry>>>,
    requests: Arc<StdMutex<Vec<PeerRequest>>>,
    invites: Arc<StdMutex<Vec<PendingInvite>>>,
    group_names: Arc<StdMutex<HashMap<String, String>>>,
    outbox: ToxOutbox,
    next_message_id: Arc<AtomicU64>,
    expired: Arc<AtomicU64>,
    dropped_requests: Arc<AtomicU64>,
    dropped_invites: Arc<AtomicU64>,
    store: ToxStore,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let state = BridgeState {
            friends,
            requests,
            invites,
            group_names,
            outbox,
            next_message_id,
            expired,
            dropped_requests,
            dropped_invites,
            store,
        };
        seed_friends(&client, &state.friends);

        while !stop.load(Ordering::SeqCst) {
            let Some(event) = client.next_event(TOX_EVENT_POLL) else {
                continue;
            };
            // A closed inbox means the core has stopped; leave the loop so the
            // savedata is written and the worker is joined.
            if !forward_event(event, &client, &events, &state) {
                break;
            }
        }

        let _ = client.shutdown();
    })
}

/// Replace the friend table with the entries of a snapshot.
#[cfg(feature = "tox-protocol")]
fn store_friends(table: &StdMutex<HashMap<u32, FriendEntry>>, friends: Vec<crate::tox::ToxFriend>) {
    let mut table = lock(table);
    *table = friends
        .into_iter()
        .map(|friend| {
            (
                friend.number,
                FriendEntry {
                    public_key: friend.public_key,
                    name: friend.name,
                    connected: friend.status != ToxConnection::Offline,
                },
            )
        })
        .collect();
}

/// Replace the friend table with a fresh snapshot.
#[cfg(feature = "tox-protocol")]
fn seed_friends(client: &ToxClient, friends: &StdMutex<HashMap<u32, FriendEntry>>) {
    let Ok(snapshot) = client.snapshot() else {
        return;
    };
    store_friends(friends, snapshot.friends);
}

/// Translate one Tox callback event and hand it to the core.
///
/// # Returns
///
/// Returns `false` when the core inbox is gone, which tells the bridge to stop.
///
/// One arm per [`ToxEvent`] variant on purpose: the translation is the contract
/// between toxcore's vocabulary and ours, and a test per variant would still not
/// notice a variant that was never translated at all (the `match` is exhaustive
/// over the enum, so a new callback fails to compile here).
///
/// `significant_drop_tightening` is allowed for the pending-list arms: the guard
/// covers a read (`already_pending`) *and* the mutation that follows
/// (`keep_pending`), and dropping it in between would split one decision across
/// two critical sections. The lint sees only the first use and suggests a rewrite
/// (`lock(list).any(..)`) that does not compile.
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
#[cfg(feature = "tox-protocol")]
fn forward_event(
    event: ToxEvent,
    client: &ToxClient,
    events: &mpsc::Sender<AppEvent>,
    state: &BridgeState,
) -> bool {
    let BridgeState {
        friends,
        requests,
        invites,
        group_names,
        outbox,
        dropped_requests,
        dropped_invites,
        // `next_message_id`, `expired` and `store` are reached through `state`
        // where they are needed, so the destructuring stays about the handles
        // this function reads.
        ..
    } = state;
    let app_event = match event {
        ToxEvent::SelfConnection { status } => {
            AppEvent::NetworkEvent(NetworkEvent::BootstrapStatus {
                node_address: "tox-dht".to_string(),
                connected: status != ToxConnection::Offline,
            })
        }
        ToxEvent::FriendRequest {
            public_key,
            message,
        } => {
            let kept = {
                let mut list = lock(requests);
                let already_pending = list
                    .iter()
                    .any(|request| request.public_key.eq_ignore_ascii_case(&public_key));
                // A duplicate is not a new request, so it must not count as a
                // refusal; a genuinely new one is refused once the list is full.
                let kept = !already_pending
                    && keep_pending(
                        &mut list,
                        PeerRequest {
                            public_key: public_key.clone(),
                            message: message.clone(),
                        },
                        PENDING_REQUESTS_CAPACITY,
                        dropped_requests,
                    );
                if !already_pending && !kept {
                    debug!(
                        "🚫 Refused a friend request from {}: {PENDING_REQUESTS_CAPACITY} are already waiting",
                        crate::utils::abbreviate(&public_key)
                    );
                }
                kept
            };
            // A stored request is what makes it answerable after a restart, which
            // is the point of keeping it at all; a refused or duplicated one
            // changed nothing and is not written.
            if kept {
                state.persist();
            }
            AppEvent::PeerRequestReceived {
                peer_id: public_key,
                message,
            }
        }
        ToxEvent::FriendConnection {
            friend_number,
            status,
            name,
        } => {
            // The public key is not part of the callback payload, so refresh the
            // table from a snapshot before reporting the transition.
            seed_friends(client, friends);
            let entry = lock(friends).get(&friend_number).cloned();
            let peer_id = entry.as_ref().map_or_else(
                || friend_number.to_string(),
                |friend| friend.public_key.clone(),
            );

            if status == ToxConnection::Offline {
                AppEvent::NetworkEvent(NetworkEvent::PeerDisconnected {
                    peer_id,
                    reason: "friend is offline".to_string(),
                })
            } else {
                // The friend just became reachable: deliver what was buffered for
                // it while it was away.
                let expired = prune_outbox(outbox);
                if expired > 0 {
                    // Counted rather than only logged: the bridge is where most
                    // expiries are observed (a payload can sit in the queue for a
                    // whole offline period), and `/metrics` has to agree with what
                    // the user was told was waiting.
                    state.expired.fetch_add(expired as u64, Ordering::SeqCst);
                    debug!(
                        "⌛ Dropped {expired} expired Tox payload(s) for friend #{friend_number}"
                    );
                }
                let sent = flush_outbox(client, outbox, friend_number);
                // Both draining the queue and dropping from it are changes the
                // store has to see, or a restart would offer a delivered payload
                // again (or keep an expired one).
                if expired > 0 || sent > 0 {
                    state.persist();
                }

                let nickname = if name.is_empty() {
                    entry.map_or_else(String::new, |friend| friend.name)
                } else {
                    name
                };
                AppEvent::NetworkEvent(NetworkEvent::PeerConnected {
                    peer_id,
                    metadata: HashMap::from([("nickname".to_string(), nickname)]),
                })
            }
        }
        ToxEvent::FriendName {
            friend_number,
            name,
        } => {
            // A name can arrive *after* the connection callback, and a re-learn
            // announces nothing: both rules live in `nickname_event`.
            let Some(event) = nickname_event(friend_number, &name, client, friends) else {
                return true;
            };
            event
        }
        ToxEvent::Message {
            friend_number,
            body,
            kind,
        } => {
            let entry = friend_entry(friend_number, client, friends);
            let (peer, peer_id) = entry.map_or_else(
                || {
                    (
                        format!("friend #{friend_number}"),
                        friend_number.to_string(),
                    )
                },
                |friend| {
                    let label = if friend.name.is_empty() {
                        short_key(&friend.public_key)
                    } else {
                        friend.name
                    };
                    (label, friend.public_key)
                },
            );
            let is_text = std::str::from_utf8(&body).is_ok();
            AppEvent::MessageReceived {
                peer,
                peer_id,
                payload: body,
                kind: domain_message_kind(kind),
                // Tox carries no content type, so the receiver classifies the
                // body: valid UTF-8 is text, anything else is opaque bytes.
                content_type: if is_text {
                    ContentType::Text
                } else {
                    ContentType::Binary
                },
            }
        }
        ToxEvent::ConferenceMessage {
            conference_id,
            peer_number,
            peer_name,
            kind,
            body,
        } => {
            // A conference is identified by its id, so the same message renders the
            // same way after a restart (the conference number does not survive one).
            let group = resolve_group_name(group_names, client, &conference_id);
            let peer = if peer_name.is_empty() {
                format!("peer {peer_number}")
            } else {
                peer_name
            };
            let is_text = std::str::from_utf8(&body).is_ok();
            AppEvent::GroupMessageReceived {
                group_id: conference_id.clone(),
                name: group,
                peer,
                peer_id: format!("{conference_id}#{peer_number}"),
                payload: body,
                kind: domain_message_kind(kind),
                content_type: if is_text {
                    ContentType::Text
                } else {
                    ContentType::Binary
                },
            }
        }
        // All three events mean the same thing to a front-end: the group's name
        // and participant list are what they are now. A rename is delivered as a
        // `GroupChanged` too, so a front-end never has to poll to notice it.
        ToxEvent::ConferenceConnected {
            conference_id,
            title,
            peers,
        }
        | ToxEvent::ConferencePeersChanged {
            conference_id,
            title,
            peers,
        }
        | ToxEvent::ConferenceTitleChanged {
            conference_id,
            title,
            peers,
        } => {
            remember_group(group_names, &conference_id, &title);
            AppEvent::GroupChanged {
                group_id: conference_id,
                name: title,
                members: peers,
                joined: true,
            }
        }
        ToxEvent::ConferenceInvite {
            friend_number,
            kind,
            cookie,
        } => {
            // The cookie is the capability; it is stored so that joining later
            // does not need the callback payload again, and hex-encoded so the
            // front-end can pass it back as a string.
            let token = hex::encode(&cookie);
            let peer_id = lock(friends).get(&friend_number).map_or_else(
                || public_key_prefix(&friend_number.to_string()),
                |friend| friend.public_key.clone(),
            );
            {
                let mut list = lock(invites);
                // The same invitation may be delivered more than once; keep one.
                let already_pending = list.iter().any(|invite| invite.token == token);
                if !already_pending
                    && !keep_pending(
                        &mut list,
                        PendingInvite {
                            friend_number,
                            peer_id: peer_id.clone(),
                            token: token.clone(),
                            conference_type: kind,
                        },
                        PENDING_INVITES_CAPACITY,
                        dropped_invites,
                    )
                {
                    debug!(
                        "🚫 Refused a group invitation: {PENDING_INVITES_CAPACITY} are already waiting"
                    );
                }
            }
            let peer = lock(friends)
                .get(&friend_number)
                .map_or_else(String::new, |friend| friend.name.clone());
            AppEvent::GroupInviteReceived {
                peer,
                peer_id,
                token,
            }
        }
    };

    events.blocking_send(app_event).is_ok()
}

/// The event that announces a nickname toxcore just reported, if any.
///
/// The public key — the peer id a front-end addresses a friend by — only exists in
/// the snapshot, and a name change is exactly the moment the table is stale, so the
/// table is refreshed first. `None` means "nothing to announce": either the friend
/// is not in the table, is not reachable (a rename of an offline friend is not a
/// connection), or the name is what the table already had — see
/// [`reported_nickname`] for why the last case matters.
#[cfg(feature = "tox-protocol")]
fn nickname_event(
    friend_number: u32,
    name: &str,
    client: &ToxClient,
    friends: &StdMutex<HashMap<u32, FriendEntry>>,
) -> Option<AppEvent> {
    let previous = lock(friends)
        .get(&friend_number)
        .map(|friend| friend.name.clone());
    seed_friends(client, friends);

    let friend = lock(friends).get(&friend_number).cloned()?;
    let nickname = reported_nickname(previous.as_deref(), friend.connected, name)?;
    Some(AppEvent::NetworkEvent(NetworkEvent::PeerConnected {
        peer_id: friend.public_key,
        metadata: HashMap::from([("nickname".to_string(), nickname)]),
    }))
}

/// The friend table entry for `friend_number`, refreshed when it is missing.
///
/// A friend whose nickname has not been learned yet is refreshed once here: a
/// message can arrive while the `friend_name` callback is still in flight, and
/// attributing it to a bare key while the nickname is available would be a worse
/// answer than one extra snapshot. The refresh is conditional, so a settled friend
/// costs nothing.
#[cfg(feature = "tox-protocol")]
fn friend_entry(
    friend_number: u32,
    client: &ToxClient,
    friends: &StdMutex<HashMap<u32, FriendEntry>>,
) -> Option<FriendEntry> {
    if lock(friends)
        .get(&friend_number)
        .is_none_or(|friend| friend.name.is_empty())
    {
        seed_friends(client, friends);
    }
    lock(friends).get(&friend_number).cloned()
}

/// The nickname to announce for a friend whose name toxcore just reported.
///
/// Returns `None` when there is nothing new to announce, which is the common
/// case and the reason this is a separate function: toxcore raises
/// `friend_name` for every name change, including the one that arrives *after*
/// the connection callback (the bug this closes) and the re-learned name of a
/// reconnecting friend — and a re-learn must not announce a second
/// "connected" line for a peer that was never disconnected.
///
/// A rename while the friend is offline announces nothing either: the friend
/// table is refreshed, so the new name is used by `/peers`, message
/// attribution and `/msg <nickname>` as soon as the friend is reachable.
#[cfg(feature = "tox-protocol")]
#[must_use]
fn reported_nickname(previous: Option<&str>, connected: bool, name: &str) -> Option<String> {
    let learned = name.trim();
    if !connected || learned.is_empty() {
        return None;
    }
    if previous.is_some_and(|old| old.eq_ignore_ascii_case(learned)) {
        return None;
    }
    Some(learned.to_string())
}

/// Remember a conference's title for later messages.
#[cfg(feature = "tox-protocol")]
fn remember_group(cache: &StdMutex<HashMap<String, String>>, conference_id: &str, title: &str) {
    lock(cache).insert(conference_id.to_string(), title.to_string());
}

/// Forget a conference's cached title.
///
/// Called when a group is left. Without it the cache would keep one entry per
/// conference ever joined — small, but it is still a map that only ever grows, and
/// the conference it described is gone.
#[cfg(feature = "tox-protocol")]
fn forget_group(cache: &StdMutex<HashMap<String, String>>, conference_id: &str) {
    lock(cache).retain(|id, _| !id.eq_ignore_ascii_case(conference_id));
}

/// Append `value` to `list` unless that would exceed `capacity`.
///
/// A refused value is counted in `dropped` instead of evicting an older one: both
/// callers keep a list fed by remote peers (see [`PENDING_REQUESTS_CAPACITY` and
/// `PENDING_INVITES_CAPACITY`]), and drop-newest is what keeps a flood from
/// pushing away an entry the user is about to act on. The count is what makes the
/// refusal visible rather than silent.
///
/// # Returns
///
/// Returns `true` when the value was kept.
#[cfg(feature = "tox-protocol")]
fn keep_pending<T>(list: &mut Vec<T>, value: T, capacity: usize, dropped: &AtomicU64) -> bool {
    if list.len() >= capacity {
        dropped.fetch_add(1, Ordering::SeqCst);
        return false;
    }
    list.push(value);
    true
}

/// Read a conference's title, preferring the bridge's cache.
///
/// The cache is filled by the conference events the bridge already sees, so a
/// message does not have to round-trip to the toxcore worker just to be named.
/// Only an unknown id (a message that arrived before the first group event) costs
/// one lookup, and the result is cached for the next one.
#[cfg(feature = "tox-protocol")]
fn resolve_group_name(
    cache: &StdMutex<HashMap<String, String>>,
    client: &ToxClient,
    conference_id: &str,
) -> String {
    if let Some(name) = lock(cache).get(conference_id) {
        return name.clone();
    }

    let Ok(snapshot) = client.snapshot() else {
        return String::new();
    };
    let mut table = lock(cache);
    for conference in snapshot.conferences {
        table.insert(conference.id.clone(), conference.title.clone());
    }
    table.get(conference_id).cloned().unwrap_or_default()
}

/// Abbreviate a public key for display (`D5F0…0831`).
#[cfg(feature = "tox-protocol")]
fn short_key(public_key: &str) -> String {
    crate::utils::abbreviate(public_key)
}

#[cfg(all(test, feature = "tox-protocol"))]
mod tests {
    use super::*;

    /// The pending lists are bounded, and a refusal is counted rather than silent.
    ///
    /// A friend request only needs our Tox address to be produced, so the list that
    /// holds the unanswered ones is a remote-triggerable allocation: without a cap
    /// an attacker generating identities grows it without limit.
    #[test]
    fn test_pending_lists_are_bounded() {
        let dropped = AtomicU64::new(0);
        let mut list: Vec<u32> = Vec::new();

        for value in 0..3 {
            assert!(
                keep_pending(&mut list, value, 3, &dropped),
                "room for {value}"
            );
        }
        assert_eq!(list, vec![0, 1, 2]);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);

        // Drop-newest: a flood must not evict an entry the user is about to act on.
        assert!(!keep_pending(&mut list, 3, 3, &dropped));
        assert_eq!(list, vec![0, 1, 2], "the oldest entry must survive");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        // The advertised caps are the ones the lists use.
        assert_eq!(PENDING_REQUESTS_CAPACITY, 128);
        assert_eq!(PENDING_INVITES_CAPACITY, 128);
    }

    /// Leaving a group forgets its cached title, and only that one.
    #[test]
    fn test_forget_group_drops_only_the_left_conference() {
        let cache = StdMutex::new(HashMap::new());
        let left = "ab".repeat(crate::tox::CONFERENCE_ID_SIZE);
        let kept = "cd".repeat(crate::tox::CONFERENCE_ID_SIZE);
        remember_group(&cache, &left, "Team");
        remember_group(&cache, &kept, "Other");

        // The lookup is case-insensitive, like every other conference id comparison.
        forget_group(&cache, &left.to_uppercase());

        // Read through two short-lived guards: there is no reason to keep the lock
        // while asserting.
        assert_eq!(
            lock(&cache).keys().cloned().collect::<Vec<_>>(),
            vec![kept],
            "only the left group is forgotten"
        );
    }

    /// Display keys are abbreviated but remain identifiable.
    #[test]
    fn test_short_key_abbreviates_only_long_keys() {
        assert_eq!(short_key("D5F0CFAD57CC86F5"), "D5F0…86F5");
        // Short values are returned unchanged instead of producing "…".
        assert_eq!(short_key("ABCD"), "ABCD");
        assert_eq!(short_key(""), "");
    }

    /// The identifier prefix is used as the public key.
    #[test]
    fn test_public_key_prefix_of_an_address() {
        let key = "A".repeat(crate::tox::PUBLIC_KEY_SIZE * 2);
        // A Tox address appends nospam (4 bytes) and a checksum (2 bytes).
        let address = format!("{key}BEEF00112233");
        assert_eq!(address.len(), crate::cli::TOX_ADDRESS_HEX_LEN);
        assert_eq!(public_key_prefix(&address), key);

        // A bare key is returned unchanged, as is a short unusable value.
        assert_eq!(public_key_prefix(&key), key);
        assert_eq!(public_key_prefix("AABB"), "AABB");
    }

    /// The prefix is cut on a character boundary, never in the middle of one.
    ///
    /// The identifier is whatever the user typed after `/add`, so it is not
    /// guaranteed to be ASCII: cutting 64 *bytes* of it panicked whenever byte 64
    /// fell inside a multi-byte character, which took the core (and the session)
    /// down from a command line.
    #[test]
    fn test_public_key_prefix_never_splits_a_character() {
        // 70 three byte characters: byte 64 is not a character boundary, and the
        // value is longer than a key, so the cut really is taken.
        let identifier = "你".repeat(70);
        let prefix = public_key_prefix(&identifier);
        assert_eq!(prefix.chars().count(), crate::tox::PUBLIC_KEY_SIZE * 2);
        assert!(
            identifier.starts_with(&prefix),
            "the public key has to be a prefix of the identifier"
        );

        // A value with fewer characters than a key is returned unchanged, however
        // many bytes it takes.
        let short = "你".repeat(22);
        assert_eq!(short.len(), 66);
        assert_eq!(public_key_prefix(&short), short);
    }

    /// A friend request compares by public key, ignoring case.
    #[test]
    fn test_peer_request_identity() {
        let request = PeerRequest {
            public_key: "ab12".to_string(),
            message: "hi".to_string(),
        };
        assert!(request.public_key.eq_ignore_ascii_case("AB12"));
    }

    /// A friend table with one entry per `(number, public key)` pair.
    ///
    /// The number is what `toxcore` assigns this run; a restart can hand out a
    /// different one, which is exactly what the store has to survive.
    fn friend_table(entries: &[(u32, &str)]) -> StdMutex<HashMap<u32, FriendEntry>> {
        let mut table = HashMap::new();
        for (number, key) in entries {
            table.insert(
                *number,
                FriendEntry {
                    public_key: (*key).to_string(),
                    name: format!("peer{number}"),
                    connected: false,
                },
            );
        }
        StdMutex::new(table)
    }

    /// A public key of the right shape.
    fn key(prefix: char) -> String {
        prefix.to_string().repeat(crate::tox::PUBLIC_KEY_SIZE * 2)
    }

    /// A stored payload for `public_key`, queued `age` ago.
    fn stored_message(public_key: &str, message_id: u64, age: Duration) -> StoredMessage {
        StoredMessage {
            public_key: public_key.to_string(),
            message_id,
            kind: MessageKind::Text,
            body: hex::encode(b"hello"),
            queued_at_unix_ms: tox_store::unix_ms(SystemTime::now() - age),
        }
    }

    /// A stored state that holds requests and nothing else.
    fn state_with_requests(requests: Vec<StoredRequest>) -> ToxState {
        ToxState {
            version: STORE_VERSION,
            next_message_id: 1,
            requests,
            outbox: Vec::new(),
        }
    }

    /// What the transport holds is what a restart reads back, and a payload finds
    /// its friend through the *key*, in whatever number this run assigned.
    #[test]
    fn test_the_queue_round_trips_through_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ToxStore::new(dir.path());
        let alice = key('A');
        let friends = friend_table(&[(0, &alice)]);

        let requests = StdMutex::new(vec![PeerRequest {
            public_key: key('C'),
            message: "please add me".to_string(),
        }]);
        let outbox: ToxOutbox = Arc::new(StdMutex::new(HashMap::from([(
            0,
            VecDeque::from([QueuedMessage {
                message_id: 4,
                body: b"first".to_vec(),
                kind: MessageKind::Action,
                queued_at: SystemTime::now(),
            }]),
        )])));
        let next_message_id = AtomicU64::new(9);

        store
            .update(|| snapshot_state(&friends, &requests, &outbox, &next_message_id))
            .expect("publish");

        // A restart: same savedata, empty queue — and the friend this time has
        // number 7.
        let restored = restore_state(&store.load(), &friend_table(&[(7, &alice)]));

        assert_eq!(restored.requests.len(), 1);
        assert_eq!(restored.requests[0].public_key, key('C'));
        assert_eq!(restored.requests[0].message, "please add me");
        assert_eq!(restored.next_message_id, 9);
        assert_eq!(restored.expired, 0);
        assert_eq!(restored.undeliverable, 0);

        let queue = restored.outbox.get(&7).expect("friend #7 holds the queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].body, b"first");
        assert_eq!(queue[0].kind, MessageKind::Action);
        assert_eq!(queue[0].message_id, 4);
        assert!(
            !restored.outbox.contains_key(&0),
            "the number from the previous run must not be reused"
        );
    }

    /// A payload that waited longer than the TTL while the client was down is
    /// dropped and counted: the outbox ages on the same clock whether or not the
    /// process is running.
    #[test]
    fn test_a_payload_that_expired_while_down_is_not_restored() {
        let alice = key('A');
        let expired = ToxState {
            version: STORE_VERSION,
            next_message_id: 3,
            requests: Vec::new(),
            outbox: vec![stored_message(
                &alice,
                1,
                TOX_OUTBOX_TTL + Duration::from_secs(1),
            )],
        };

        let restored = restore_state(&expired, &friend_table(&[(0, &alice)]));
        assert!(restored.outbox.is_empty());
        assert_eq!(restored.expired, 1);
        // The id is not handed out again even though the payload is gone.
        assert_eq!(restored.next_message_id, 3);

        // Inside the window the payload is restored.
        let fresh = ToxState {
            outbox: vec![stored_message(&alice, 1, Duration::from_secs(1))],
            ..expired
        };
        let restored = restore_state(&fresh, &friend_table(&[(0, &alice)]));
        assert_eq!(restored.expired, 0);
        assert_eq!(restored.outbox.len(), 1);
        assert_eq!(restored.outbox[&0][0].body, b"hello");
    }

    /// A payload that cannot be addressed again is dropped: its peer is not a
    /// friend (any more), or the entry is not readable.
    #[test]
    fn test_a_payload_that_cannot_be_addressed_is_dropped() {
        let alice = key('A');
        let stranger = key('B');
        let unaddressable = ToxState {
            version: STORE_VERSION,
            next_message_id: 5,
            requests: Vec::new(),
            outbox: vec![
                // Not in the friend table: there is no number to send it to.
                stored_message(&stranger, 1, Duration::ZERO),
                // A body that is not hexadecimal: the file was edited or damaged.
                StoredMessage {
                    body: "not hexadecimal".to_string(),
                    ..stored_message(&alice, 2, Duration::ZERO)
                },
            ],
        };

        let restored = restore_state(&unaddressable, &friend_table(&[(0, &alice)]));
        assert!(restored.outbox.is_empty());
        assert_eq!(restored.undeliverable, 2);
        assert_eq!(restored.expired, 0);
    }

    /// The restored queue is capped per friend, keeping the oldest payloads in
    /// delivery order — the same drop-newest rule the runtime path applies.
    #[test]
    fn test_a_restored_queue_is_capped_keeping_the_delivery_order() {
        let alice = key('A');
        let stored = ToxState {
            version: STORE_VERSION,
            next_message_id: TOX_OUTBOX_CAPACITY as u64 + 5,
            requests: Vec::new(),
            outbox: (0..=TOX_OUTBOX_CAPACITY)
                .map(|index| stored_message(&alice, index as u64, Duration::ZERO))
                .collect(),
        };

        let restored = restore_state(&stored, &friend_table(&[(3, &alice)]));
        let queue = &restored.outbox[&3];
        assert_eq!(queue.len(), TOX_OUTBOX_CAPACITY);
        assert_eq!(queue[0].message_id, 0, "the oldest payload is kept first");
        assert_eq!(
            queue[TOX_OUTBOX_CAPACITY - 1].message_id,
            TOX_OUTBOX_CAPACITY as u64 - 1
        );
        assert_eq!(restored.undeliverable, 1);
    }

    /// The stored request list is bounded and deduplicated exactly like the live
    /// one, so a restart cannot widen it or double an entry.
    #[test]
    fn test_restored_requests_are_bounded_and_deduplicated() {
        let mut requests: Vec<StoredRequest> = (0..PENDING_REQUESTS_CAPACITY)
            .map(|index| StoredRequest {
                public_key: format!("{index:064X}"),
                message: "spam".to_string(),
            })
            .collect();
        // A duplicate of an entry already present, in the other hex case, and one
        // request past the cap.
        requests.push(StoredRequest {
            public_key: format!("{:064X}", 0).to_lowercase(),
            message: "again".to_string(),
        });
        requests.push(StoredRequest {
            public_key: key('F'),
            message: "late".to_string(),
        });

        let restored = restore_state(&state_with_requests(requests), &friend_table(&[]));
        assert_eq!(restored.requests.len(), PENDING_REQUESTS_CAPACITY);
        assert_eq!(restored.requests[0].message, "spam");
        assert_eq!(
            restored.requests_dropped, 1,
            "only the request past the cap is a refusal"
        );
    }

    /// A timestamp in the future is treated as "just queued": it cannot be told
    /// apart from a clock that jumped back, and dropping a message somebody was
    /// told is waiting is the worse answer.
    #[test]
    fn test_a_timestamp_in_the_future_is_treated_as_just_queued() {
        let ahead = SystemTime::now() + Duration::from_secs(600);
        assert_eq!(queued_age(ahead), Duration::ZERO);

        let alice = key('A');
        let stored = ToxState {
            version: STORE_VERSION,
            next_message_id: 2,
            requests: Vec::new(),
            outbox: vec![StoredMessage {
                queued_at_unix_ms: tox_store::unix_ms(ahead),
                ..stored_message(&alice, 1, Duration::ZERO)
            }],
        };

        let restored = restore_state(&stored, &friend_table(&[(0, &alice)]));
        assert_eq!(restored.expired, 0);
        assert_eq!(restored.outbox.len(), 1);
    }

    /// A payload queued for an offline friend survives a restart of the
    /// transport, still addressed to that friend.
    ///
    /// The friend exists only as an address (the peer instance is never able to
    /// reach it), so every payload has to be buffered — which is exactly the
    /// state the store exists for. Needs `libtoxcore`, but no network access: both
    /// instances are local and the DHT is never dialled. A restart is a second
    /// `ToxTransport` over the same data directory, and the payload is asserted in
    /// the live queue, not only in the file, because translating a stored key back
    /// into this run's friend number is the part that can go wrong.
    #[tokio::test]
    async fn test_a_queued_payload_survives_a_restart() {
        if !crate::tox::is_linked() {
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let peer_dir = tempfile::tempdir().expect("tempdir");
        let config = AppConfig::default();
        let args = CliArgs {
            transport: crate::cli::Transport::Tox,
            data_dir: Some(dir.path().to_path_buf()),
            nickname: Some("QueueTester".to_string()),
            port: Some(0),
            ..CliArgs::default()
        };

        let peer =
            ToxClient::start(ToxConfig::new(peer_dir.path(), "Peer")).expect("peer instance");
        let peer_key = peer.public_key();

        // The first run: add the peer, then send while it is unreachable.
        let (events, _events_rx) = mpsc::channel(64);
        let mut first = ToxTransport::new(&config, &args, events).expect("first transport");
        let friend_key = first
            .request_peer(peer.address())
            .await
            .expect("friend request must be accepted locally");
        assert_eq!(friend_key, peer_key);

        let outcome = first
            .send_payload(
                &friend_key,
                b"survive this",
                MessageKind::Text,
                ContentType::Text,
            )
            .await
            .expect("the payload must be buffered, not refused");
        let SendOutcome::Queued {
            message_id,
            position,
        } = outcome
        else {
            panic!("an offline friend must queue, got {outcome:?}");
        };
        assert_eq!(message_id, 1);
        assert_eq!(position, 1);
        assert_eq!(first.queued_messages(), 1);
        assert!(
            first.store_path().exists(),
            "the queue is on disk before the process ends: {}",
            first.store_path().display()
        );

        // The queue is written before shutdown, so an *unclean* exit keeps it too.
        first.shutdown().await.expect("shutdown");

        let stored = ToxStore::new(dir.path()).load();
        assert_eq!(stored.outbox.len(), 1);
        assert_eq!(stored.outbox[0].public_key, friend_key);
        assert_eq!(
            hex::decode(&stored.outbox[0].body).expect("hex body"),
            b"survive this"
        );

        // The restart: the savedata still lists the friend, and the queued payload
        // is back in the live queue.
        let (events, _events_rx) = mpsc::channel(64);
        let mut second = ToxTransport::new(&config, &args, events).expect("second transport");
        assert_eq!(
            second.queued_messages(),
            1,
            "the queued payload must be restored, not lost with the process"
        );
        assert!(second.pending_requests().is_empty(), "no request was made");
        second.shutdown().await.expect("shutdown");

        peer.shutdown().expect("peer shutdown");
    }

    /// A transport dropped *without* `shutdown()` must still stop its bridge.
    ///
    /// The bridge is a `spawn_blocking` task, and a tokio runtime waits for its
    /// blocking tasks when it is dropped. The stop flag used to be set only by
    /// `ToxTransport::shutdown`, so a transport dropped on any other path — an
    /// actor torn down by a runtime drop, or a front-end/test that panics before
    /// its `shutdown().await` — left the bridge looping in `next_event()` and the
    /// teardown waiting on it: a hang, not an error.
    ///
    /// The check awaits the bridge task directly (the test module is a child of
    /// this one, so the private handle is reachable) with a bounded `timeout`, so a
    /// regression fails here rather than hanging the suite. The helper thread runs
    /// its own runtime and reports its verdict *before* dropping that runtime, so a
    /// regression cannot turn this test itself into a hang.
    #[test]
    fn test_dropping_the_transport_stops_its_bridge() {
        if !crate::tox::is_linked() {
            return;
        }

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let stopped = runtime.block_on(async {
                let dir = tempfile::tempdir().expect("tempdir");
                let config = AppConfig::default();
                let args = CliArgs {
                    transport: crate::cli::Transport::Tox,
                    data_dir: Some(dir.path().to_path_buf()),
                    nickname: Some("DropTester".to_string()),
                    port: Some(0),
                    ..CliArgs::default()
                };
                let (event_tx, _event_rx) = mpsc::channel(16);
                let mut transport = ToxTransport::new(&config, &args, event_tx).expect("transport");
                let bridge = transport.bridge.take().expect("the bridge is running");

                // The orderly path is deliberately *not* taken: this is the case
                // that used to hang.
                drop(transport);

                tokio::time::timeout(Duration::from_secs(15), bridge)
                    .await
                    .is_ok()
            });
            // Report before tearing the runtime down: if the bridge never stopped,
            // the runtime drop below would hang this thread, so the verdict has to
            // be sent first.
            let _ = done_tx.send(stopped);
            drop(runtime);
        });

        let stopped = done_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the helper thread must report its verdict");
        assert!(
            stopped,
            "the bridge must stop when the transport is dropped without shutdown()"
        );
    }

    /// A late nickname is reported exactly once, and only for a reachable friend.
    ///
    /// This is the regression test for the friend whose name crosses the wire
    /// *after* `friend_connection_status`: the connection event carried an empty
    /// name, and without the `friend_name` callback the peer stayed nameless for
    /// the rest of the session — `/peers` said it was still completing its
    /// handshake, messages were attributed to a bare key, and `/msg <nickname>`
    /// could not resolve it.
    #[cfg(feature = "tox-protocol")]
    #[test]
    fn test_a_late_nickname_is_reported_once() {
        // The name arrives while the friend is not reachable yet: cache only.
        assert_eq!(reported_nickname(Some(""), false, "Bob"), None);

        // The friend connects, the name is already known: the connection event
        // announces it, so the name event must stay silent...
        assert_eq!(reported_nickname(Some("Bob"), true, "Bob"), None);
        assert_eq!(reported_nickname(Some("bob"), true, "BOB"), None);
        // ...but the late name — the bug — is announced.
        assert_eq!(
            reported_nickname(Some(""), true, "Bob").as_deref(),
            Some("Bob")
        );
        // As is a genuine rename, and a friend whose entry has no name yet.
        assert_eq!(
            reported_nickname(Some("Bob"), true, "Bobby").as_deref(),
            Some("Bobby")
        );
        assert_eq!(reported_nickname(None, true, "Bob").as_deref(), Some("Bob"));

        // Nothing to announce: a cleared name, or whitespace around it.
        assert_eq!(reported_nickname(Some("Bob"), true, ""), None);
        assert_eq!(reported_nickname(None, true, "   "), None);
        // The reported name is trimmed, so `/msg <nickname>` matches what is shown.
        assert_eq!(
            reported_nickname(None, true, "  Bob  ").as_deref(),
            Some("Bob")
        );
    }
}
