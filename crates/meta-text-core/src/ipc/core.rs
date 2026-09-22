/*!
 * core.rs
 *
 * Headless core application service: the single owner of domain state.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Owns configuration, crypto, storage, transport and the contact list
 * - Serves [`Request`]s from any number of front-ends
 * - Broadcasts [`CoreEvent`]s without blocking on slow consumers
 * - Bounded command queue: overload is reported, never buffered forever
 * - Graceful shutdown that flushes the session before releasing the sockets
 *
 * # Design
 *
 * [`CoreService`] is an *actor*: it owns every mutable field and runs on a
 * single task, so no lock is needed for domain state and no front-end can
 * observe a half-applied update. Front-ends interact through [`CoreHandle`],
 * which is cheap to clone and safe to share.
 */

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, instrument, warn};

use crate::cli::CliArgs;
use crate::config::AppConfig;
use crate::crypto::CryptoManager;
use crate::database::{DatabaseManager, MessageDirection, StoredMessage};
use crate::identity::{IdentityStore, NetworkIdentity};
use crate::network::SendOutcome;
use crate::transport::CoreTransport;
use crate::types::{AppEvent, AppState, Contact, Group, UserStatus};

use super::protocol::{
    ContactView, ContentType, CoreEvent, ErrorCode, ErrorInfo, GroupView, MessageKind, MessageView,
    MetricsView, PeerIdentityView, Reply, Request, SendOutcomeKind, SendReport, SessionInfo,
    StatisticsView, ALLOWED_MESSAGE_CONTROLS,
};
use super::{protocol, validation};

/// File name used to persist the session (nickname, contacts, statistics).
const SESSION_STATE_FILE: &str = "metatext-session.json";

/// Nickname used when the configured default fails validation.
const FALLBACK_NICKNAME: &str = "metaText";

/// Default number of in-flight requests a core accepts before applying
/// backpressure. Small on purpose: a front-end that cannot keep up should
/// learn about it instead of growing an unbounded queue.
pub const DEFAULT_COMMAND_CAPACITY: usize = 64;

/// Default number of events buffered for each subscriber.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// How often the service reconciles statistics and performs the timed save.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Tunables for a [`CoreService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreServiceOptions {
    /// In-flight request queue depth.
    pub command_capacity: usize,

    /// Per-subscriber event buffer depth.
    pub event_capacity: usize,
}

impl Default for CoreServiceOptions {
    fn default() -> Self {
        Self {
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }
}

/// A request paired with the channel its reply must be delivered on.
#[derive(Debug)]
struct Command {
    /// Operation requested by the front-end.
    request: Request,

    /// When the front-end enqueued the request, used to measure actor lag.
    enqueued_at: std::time::Instant,

    /// One-shot reply channel.
    reply: oneshot::Sender<super::protocol::ResponseResult>,
}

/// Client handle to a running [`CoreService`].
///
/// Cloning is cheap; every clone talks to the same actor instance.
#[derive(Debug, Clone)]
pub struct CoreHandle {
    /// Bounded request queue.
    commands: mpsc::Sender<Command>,

    /// Event fan-out.
    events: broadcast::Sender<CoreEvent>,
}

impl CoreHandle {
    /// Send a request and await its reply.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Backpressure`] when the command queue is full,
    /// [`ErrorCode::Internal`] when the service has stopped, and otherwise the
    /// error reported by the service itself.
    pub async fn request(&self, request: Request) -> Result<Reply, ErrorInfo> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command {
                request,
                enqueued_at: std::time::Instant::now(),
                reply,
            })
            .await
            .map_err(|_| {
                ErrorInfo::critical(ErrorCode::Internal, "the core service is not running")
            })?;

        receiver
            .await
            .map_err(|_| {
                ErrorInfo::critical(
                    ErrorCode::Internal,
                    "the core service dropped the reply channel",
                )
            })?
            .into_result()
    }

    /// Subscribe to the event stream.
    ///
    /// The receiver may lag behind; a lag is reported by the transport as a
    /// gap rather than stalling the core.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    /// Whether the actor is still accepting commands.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.commands.is_closed()
    }

    /// Ask the service to stop.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`CoreHandle::request`].
    pub async fn shutdown(&self) -> Result<Reply, ErrorInfo> {
        self.request(Request::Shutdown).await
    }
}

/// Serializable snapshot of everything that survives a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionState {
    /// Local user nickname.
    nickname: String,

    /// Local user status message.
    status_message: String,

    /// Contact list.
    contacts: Vec<Contact>,

    /// Runtime statistics carried over from the previous session.
    statistics: crate::types::AppStatistics,
}

/// One iteration of the actor loop.
#[derive(Debug)]
enum ActorStep {
    /// A request arrived, or the command channel closed.
    Command(Option<Command>),

    /// A transport event arrived, or the transport channel closed.
    Event(Option<AppEvent>),

    /// The maintenance tick fired.
    Tick,
}

impl ActorStep {
    /// Classify a command-channel result.
    const fn from_command(command: Option<Command>) -> Self {
        Self::Command(command)
    }

    /// Classify a transport-channel result.
    const fn from_event(event: Option<AppEvent>) -> Self {
        Self::Event(event)
    }
}

/// The headless metaText backend.
///
/// Construction is split from activation so that a caller can prepare a
/// service without opening sockets or files (useful for tests and for a
/// front-end that wants to fail before touching the network).
pub struct CoreService {
    /// Effective configuration after CLI overrides.
    config: AppConfig,

    /// Parsed command line arguments (mode, paths, peers).
    args: CliArgs,

    /// Mutable session state; owned outright, hence lock-free.
    state: AppState,

    /// Encryption shared with the transport.
    crypto: Arc<CryptoManager>,

    /// Peer-to-peer transport (TCP or Tox, chosen by `--transport`).
    transport: CoreTransport,

    /// Persistence backend.
    database: DatabaseManager,

    /// Nickname announced to peers.
    nickname: String,

    /// Personal status message.
    status_message: String,

    /// Announced network identity of this instance.
    ///
    /// A persisted X25519 key pair: the public half is what peers derive the pair's
    /// key from, and it is the same value next run, which is what lets a peer pin it
    /// (see [`crate::identity`]). The secret half never leaves this struct, and its
    /// [`std::fmt::Debug`] view is redacted.
    identity: NetworkIdentity,

    /// Friend list.
    contacts: Vec<Contact>,

    /// Identifier of the active conversation.
    active_chat: Option<uuid::Uuid>,

    /// Groups this session is in, keyed by the transport's stable identifier.
    groups: Vec<Group>,

    /// Identifier of the active group, when one is selected.
    active_group: Option<String>,

    /// Session start time.
    started_at: chrono::DateTime<chrono::Utc>,

    /// Timestamp of the last automatic session save.
    last_auto_save: tokio::time::Instant,

    /// Transport event source; taken by the actor loop.
    inbox: Option<mpsc::Receiver<AppEvent>>,

    /// Where the session snapshot is written.
    session_path: PathBuf,

    /// Resource limits.
    options: CoreServiceOptions,

    /// Requests that were waiting to be executed when the last one was picked up.
    ///
    /// Written by the actor loop, read by [`Self::metrics_view`]; it is the one
    /// counter the service cannot derive from its own fields.
    command_queue_depth: usize,

    /// Queue wait of the most recently served request (actor lag).
    request_wait_last: Duration,

    /// Worst queue wait observed since start.
    request_wait_max: Duration,

    /// Execution time of the most recently served request.
    request_service_last: Duration,

    /// Worst execution time observed since start.
    request_service_max: Duration,

    /// Requests served since start.
    requests_served: u64,
}

impl std::fmt::Debug for CoreService {
    /// Redacted debug view: the session key must never reach a log record.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreService")
            .field("nickname", &self.nickname)
            .field("identity", &self.identity)
            .field("contacts", &self.contacts.len())
            .field("active_chat", &self.active_chat)
            .field("session_path", &self.session_path)
            .finish_non_exhaustive()
    }
}

impl CoreService {
    /// Build the backend without opening sockets or files.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration loaded from disk.
    /// * `args` - Parsed command line arguments, applied as overrides.
    ///
    /// # Errors
    ///
    /// Returns an error when the cryptographic state cannot be derived (for
    /// example an invalid `--passphrase` configuration).
    #[instrument(skip(config, args))]
    pub async fn new(config: AppConfig, args: CliArgs) -> Result<Self> {
        let mut config = config;
        Self::apply_overrides(&mut config, &args);

        let (event_sender, event_receiver) =
            mpsc::channel::<AppEvent>(crate::network::EVENT_INBOX_CAPACITY);

        // A shared passphrase makes every peer derive the same key, which is
        // what allows them to decrypt each other. Without one, each process
        // uses a fresh random key and messages stay local.
        let crypto = Arc::new(match &args.passphrase {
            Some(passphrase) => CryptoManager::from_passphrase(&config.crypto, passphrase)
                .context("Failed to derive the encryption key from the passphrase")?,
            None => CryptoManager::new(&config.crypto)
                .await
                .context("Failed to initialise the crypto manager")?,
        });

        let database = DatabaseManager::new(&config.database)
            .await
            .context("Failed to initialise the database manager")?;
        // The transport is selected here, once, from `--transport`. The core
        // never learns which one it got: both report events into `event_sender`
        // and expose the same send/receive surface.
        let transport = CoreTransport::new(&config, &args, Arc::clone(&crypto), event_sender)
            .await
            .context("Failed to initialise the transport")?;

        let data_dir = args.data_directory().unwrap_or_else(|| PathBuf::from("."));
        let session_path = data_dir.join(SESSION_STATE_FILE);
        // The identity is read from (or written to) the data directory before
        // anything announces it: a peer pins the value it sees, so a fresh random
        // one every run would make the pin meaningless (see `identity`).
        let identity = IdentityStore::new(&data_dir).load_or_create();

        // A configuration file can be edited by hand, so the configured
        // defaults are validated here too: the library is a public entry point
        // and must not depend on the binary having run `AppConfig::validate`.
        let nickname = match validation::nickname(&config.app.default_nickname) {
            Ok(nickname) => nickname,
            Err(error) => {
                warn!(
                    "⚠️ app.default_nickname is not usable ({error}); falling back to '{FALLBACK_NICKNAME}'"
                );
                FALLBACK_NICKNAME.to_string()
            }
        };
        let status_message = match validation::status(&config.app.default_status) {
            Ok(status) => status,
            Err(error) => {
                warn!("⚠️ app.default_status is not usable ({error}); using an empty status");
                String::new()
            }
        };

        let mut service = Self {
            nickname,
            status_message,
            identity,
            state: AppState::new(),
            crypto,
            transport,
            database,
            contacts: Vec::new(),
            active_chat: None,
            groups: Vec::new(),
            active_group: None,
            started_at: chrono::Utc::now(),
            last_auto_save: tokio::time::Instant::now(),
            inbox: Some(event_receiver),
            session_path,
            options: CoreServiceOptions::default(),
            command_queue_depth: 0,
            request_wait_last: Duration::ZERO,
            request_wait_max: Duration::ZERO,
            request_service_last: Duration::ZERO,
            request_service_max: Duration::ZERO,
            requests_served: 0,
            config,
            args,
        };

        service.load_session().await;
        Ok(service)
    }

    /// Apply the precedence rule "command line wins over configuration file".
    fn apply_overrides(config: &mut AppConfig, args: &CliArgs) {
        if let Some(port) = args.port {
            config.network.port = port;
        }
        if args.max_connections != 0 {
            config.network.max_connections = args.max_connections;
        }
        if args.no_encryption {
            config.crypto.enable_encryption = false;
        }
        if let Some(nickname) = &args.nickname {
            config.app.default_nickname.clone_from(nickname);
        }
        if args.debug {
            config.logging.level = "debug".to_string();
        }
        if let Some(node) = &args.bootstrap_node {
            // Only the TCP transport consumes the configured bootstrap list; Tox
            // builds its own from `default_bootstrap_nodes` plus `--bootstrap`,
            // so adding a `host:port:KEY` triple here would just be dead data.
            if args.transport == crate::cli::Transport::Tcp
                && !config.network.bootstrap_nodes.contains(node)
            {
                config.network.bootstrap_nodes.push(node.clone());
            }
        }

        // Resolve a relative SQLite path against the data directory so that
        // `--data-dir` decides where the database lives instead of silently
        // writing into the current working directory.
        if config.database.database_type.eq_ignore_ascii_case("sqlite") {
            let configured = config.database.connection_string.clone();
            let special = configured == ":memory:" || configured.starts_with("sqlite:");
            if !special && std::path::Path::new(&configured).is_relative() {
                if let Some(dir) = args.data_directory() {
                    config.database.connection_string =
                        dir.join(configured).to_string_lossy().into_owned();
                }
            }
        }
    }

    /// Override the resource limits (used by tests and embedders).
    #[must_use]
    pub const fn with_options(mut self, options: CoreServiceOptions) -> Self {
        self.options = options;
        self
    }

    /// Bind sockets, open storage and announce the local nickname.
    ///
    /// # Errors
    ///
    /// Returns an error when the database or the transport listener cannot be
    /// started. Reachability of individual peers is never fatal.
    pub async fn start(&mut self) -> Result<()> {
        self.database
            .start()
            .await
            .context("Failed to start the database")?;

        // The nickname, identity and status are set *before* the listener is bound, so
        // the very first greeting of an inbound connection already carries them. Binding
        // first would open a window in which a peer that connects immediately is greeted
        // with an empty nickname and no identity — it would then have to be corrected by
        // the re-announcement that `set_identity` performs, and a frame that arrived in
        // between could not use the pair key. Setting them first makes the greeting
        // always complete, and makes the identity write happen before any frame exists.
        self.transport.set_nickname(&self.nickname).await;
        // The identity is what the other end needs to agree a per-pair key on; its
        // public half is announced, and announcing it costs one frame per connection.
        self.transport.set_identity(&self.identity).await;
        self.transport.set_status(&self.status_message).await;

        self.transport
            .start()
            .await
            .context("Failed to start the transport")?;

        // Peers named on the command line are registered with the transport.
        // For TCP `--bootstrap` is an extra address to dial; for Tox it is a DHT
        // node, and the addresses to reach are the `--peer` Tox addresses.
        let mut wanted = self.args.peers.clone();
        if self.args.transport == crate::cli::Transport::Tcp {
            if let Some(node) = &self.args.bootstrap_node {
                wanted.push(node.clone());
            }
        }
        if !wanted.is_empty() {
            let connected = self.transport.connect_all(&wanted).await;
            info!(
                "🔗 Established {connected} of {} requested peer connection(s)",
                wanted.len()
            );
        }

        self.started_at = chrono::Utc::now();
        self.state.statistics.start_time = Some(self.started_at);
        info!("✅ Core service started");
        Ok(())
    }

    /// Move the service onto its own task and return a client handle.
    ///
    /// The returned handle can be cloned and shared. The task stops when the
    /// last handle is dropped, when [`CoreHandle::shutdown`] is called, or when
    /// the transport reports a terminal shutdown event.
    #[must_use]
    pub fn spawn(self) -> CoreHandle {
        let (commands_tx, commands_rx) = mpsc::channel::<Command>(self.options.command_capacity);
        let (events_tx, _) = broadcast::channel::<CoreEvent>(self.options.event_capacity);

        let handle = CoreHandle {
            commands: commands_tx,
            events: events_tx.clone(),
        };

        tokio::spawn(async move {
            self.run(commands_rx, events_tx).await;
        });

        handle
    }

    /// Actor loop: serialise every state mutation on one task.
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        events: broadcast::Sender<CoreEvent>,
    ) {
        // The inbox is taken out of `self` so the select can borrow it while
        // the handlers borrow `self` mutably.
        let mut inbox = self.inbox.take();
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        // The first tick completes immediately; skip it so startup is not
        // immediately followed by a maintenance pass.
        ticker.tick().await;

        let ready = self.session_info();
        let _ = events.send(CoreEvent::Ready {
            session: Box::new(ready),
        });

        loop {
            // Without an inbox (only possible if the actor was restarted) the
            // select degenerates to requests plus the tick.
            let event = match inbox.as_mut() {
                Some(receiver) => {
                    tokio::select! {
                        command = commands.recv() => ActorStep::from_command(command),
                        pushed = receiver.recv() => ActorStep::from_event(pushed),
                        _ = ticker.tick() => ActorStep::Tick,
                    }
                }
                None => {
                    tokio::select! {
                        command = commands.recv() => ActorStep::from_command(command),
                        _ = ticker.tick() => ActorStep::Tick,
                    }
                }
            };

            match event {
                ActorStep::Command(Some(command)) => {
                    // Record how deep the queue was behind this request and how
                    // long it waited, so `/metrics` can report actor lag without
                    // touching the receiver from anywhere else.
                    self.command_queue_depth = commands.len();
                    // Read before `command.request` is moved out below.
                    let waited = command.enqueued_at.elapsed();
                    if matches!(command.request, Request::Shutdown) {
                        let started = std::time::Instant::now();
                        let reply = self.handle(command.request, &events).await;
                        self.record_request(waited, started.elapsed());
                        // Stop accepting transport events before the subsystems
                        // close: dropping the receiver releases a transport task
                        // that is blocked handing over an event, which would
                        // otherwise deadlock against `finish` below.
                        drop(inbox.take());
                        // Terminal cleanup runs *before* the reply is sent, so a
                        // caller that awaits `Request::Shutdown` (and therefore
                        // `main`) cannot return while the session snapshot is
                        // still being written or the subsystems are still
                        // closing. Replying first used to let the runtime drop
                        // the actor midway through `save_session`, which lost
                        // the snapshot and logged a spurious "Failed to write".
                        self.finish(events.clone()).await;
                        // A closed reply channel only means the caller gave up.
                        let _ = command.reply.send(reply);
                        return;
                    }
                    let started = std::time::Instant::now();
                    let result = self.handle(command.request, &events).await;
                    self.record_request(waited, started.elapsed());
                    // A closed reply channel only means the caller gave up.
                    let _ = command.reply.send(result);
                }
                // Every handle was dropped: nothing can observe state any more.
                ActorStep::Command(None) => break,
                ActorStep::Event(Some(app_event)) => {
                    if self.on_app_event(app_event, &events).await {
                        break;
                    }
                }
                // The transport channel is closed; requests still work.
                ActorStep::Event(None) => inbox = None,
                ActorStep::Tick => self.on_tick().await,
            }
        }

        // Same reasoning as the shutdown branch: release a transport task that
        // may be waiting on the bounded inbox before we stop the transport.
        drop(inbox.take());
        self.finish(events).await;
    }

    /// Terminal cleanup: persist the session and close every subsystem.
    async fn finish(&mut self, events: broadcast::Sender<CoreEvent>) {
        if let Err(error) = self.save_session().await {
            // `{error:#}` prints the whole anyhow chain, so the underlying
            // cause (permission denied, missing directory, disk full, ...) is
            // not hidden behind the outer context.
            warn!("⚠️ Failed to persist the session during shutdown: {error:#}");
        }
        if let Err(error) = self.transport.shutdown().await {
            warn!("⚠️ Failed to stop the transport cleanly: {error}");
        }
        if let Err(error) = self.database.shutdown().await {
            warn!("⚠️ Failed to stop the database cleanly: {error}");
        }

        let _ = events.send(CoreEvent::Shutdown {
            reason: "the core service stopped".to_string(),
        });
        info!("👋 Core service stopped");
    }

    /// Fold one served request into the actor-lag counters.
    ///
    /// `wait` is the queueing delay (the *lag* a monitor watches) and `service`
    /// is how long the actor spent on it. Both maxima are kept so a transient
    /// stall is not averaged away.
    fn record_request(&mut self, wait: Duration, service: Duration) {
        self.request_wait_last = wait;
        self.request_service_last = service;
        self.request_wait_max = self.request_wait_max.max(wait);
        self.request_service_max = self.request_service_max.max(service);
        self.requests_served += 1;
    }

    /// Periodic maintenance: reconcile counters and auto-save the session.
    async fn on_tick(&mut self) {
        self.state.statistics.active_connections =
            u32::try_from(self.transport.connected_peers()).unwrap_or(u32::MAX);

        let interval = self.config.app.auto_save_interval;
        if interval > 0 && self.last_auto_save.elapsed().as_secs() >= interval {
            if let Err(error) = self.save_session().await {
                warn!("⚠️ Automatic session save failed: {error:#}");
            }
            self.last_auto_save = tokio::time::Instant::now();
        }
    }

    /// Dispatch one request. Failures are values, never panics.
    async fn handle(
        &mut self,
        request: Request,
        events: &broadcast::Sender<CoreEvent>,
    ) -> super::protocol::ResponseResult {
        match request {
            Request::Ping { echo } => ok(Reply::Pong { echo }),
            Request::SessionInfo => {
                // The snapshot reports the same group count `/group` lists, so the
                // transport is asked first: it is the source of truth (toxcore rejoins a
                // conference after a restart) and the cache alone can be cold, which made
                // `/whoami` say "0 groups" in a session that really was in one.
                self.refresh_group_cache().await;
                ok(Reply::Session {
                    session: self.session_info(),
                })
            }
            Request::Statistics => ok(Reply::Statistics {
                statistics: self.statistics_view(),
            }),
            Request::ListContacts => ok(Reply::Contacts {
                contacts: self.contacts.iter().map(contact_view).collect(),
            }),
            Request::AddContact { identifier, note } => self.add_contact(identifier, note).await,
            Request::RemoveContact { target } => self.remove_contact(&target).await,
            Request::SetNickname { nickname } => self.set_nickname(nickname, events).await,
            Request::SetStatus { text } => self.set_status(&text, events),
            Request::SelectConversation { target } => self.select_conversation(&target),
            Request::Connect { address } => self.connect(&address).await,
            Request::SendMessage {
                target,
                text,
                kind,
                content_type,
            } => self.send_message(target, &text, kind, content_type).await,
            Request::History { limit } => self.history(limit).await,
            Request::SaveSession => match self.save_session().await {
                Ok(()) => ok(Reply::Saved {
                    path: self.session_path.display().to_string(),
                }),
                Err(error) => err(ErrorInfo::critical(ErrorCode::Internal, format!("{error}"))),
            },
            Request::Shutdown => ok(Reply::ShuttingDown { acknowledged: true }),
            Request::GroupInvites => ok(Reply::GroupInvites {
                supported: self.transport.supports_groups(),
                invites: self
                    .transport
                    .pending_group_invites()
                    .into_iter()
                    .map(|invite| super::protocol::GroupInviteView {
                        peer: invite.peer,
                        peer_id: invite.peer_id,
                        token: invite.token,
                    })
                    .collect(),
            }),
            Request::PeerRequests => ok(Reply::PeerRequests {
                requests: self
                    .transport
                    .pending_requests()
                    .into_iter()
                    .map(|request| super::protocol::PeerRequestView {
                        short_key: crate::utils::abbreviate(&request.public_key),
                        public_key: request.public_key,
                        message: request.message,
                    })
                    .collect(),
            }),
            Request::AcceptPeerRequest { public_key } => self.accept_request(&public_key).await,
            Request::RejectPeerRequest { public_key } => self.reject_request(&public_key),
            Request::Groups => self.list_groups().await,
            Request::CreateGroup { title } => {
                self.create_group(title.as_deref().unwrap_or_default())
                    .await
            }
            Request::JoinGroup { token } => self.join_group(&token).await,
            Request::DeclineGroupInvite { token } => self.decline_group_invite(&token),
            Request::RenameGroup { group_id, name } => self.rename_group(group_id, &name).await,
            Request::InviteToGroup { group_id, peer } => {
                self.invite_to_group(&group_id, &peer).await
            }
            Request::SendGroupMessage {
                group_id,
                text,
                kind,
                content_type,
            } => {
                self.send_group_message(group_id, &text, kind, content_type)
                    .await
            }
            Request::LeaveGroup { group_id } => self.leave_group(group_id).await,
            Request::Metrics => ok(Reply::Metrics {
                metrics: self.metrics_view(events),
            }),
        }
    }

    /// The refusal for a full friend list, if it is full.
    ///
    /// Every path that can create a contact goes through this (adding one,
    /// accepting a request), so the configured maximum cannot be enforced in one
    /// place and forgotten in the other — which is exactly what had happened: the
    /// limit was checked on `/add` only.
    fn friend_limit_error(&self) -> Option<ErrorInfo> {
        let max_friends = self.config.app.max_friends;
        (self.contacts.len() >= max_friends).then(|| {
            ErrorInfo::new(
                ErrorCode::Backpressure,
                format!("contact limit reached ({max_friends}); cannot add more friends"),
            )
        })
    }

    /// Reject a pending friend request.
    ///
    /// Nothing is sent to the requester and no contact is created: the request
    /// simply leaves the pending list. That is the whole point — the list is
    /// bounded, so a user needs a way to make room without accepting strangers.
    ///
    /// Synchronous and read-only on purpose: unlike accepting, a refusal creates no
    /// contact and touches no transport, so there is nothing to await and nothing to
    /// mutate in the core.
    fn reject_request(&self, public_key: &str) -> super::protocol::ResponseResult {
        let key = public_key.trim();
        if key.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a friend request public key is required",
            ));
        }

        match self.transport.reject_request(key) {
            Ok(rejected) => {
                info!(
                    "🚫 Discarded the friend request from {}",
                    crate::utils::abbreviate(&rejected)
                );
                ok(Reply::PeerRequestRejected {
                    public_key: rejected,
                })
            }
            Err(error) => err(ErrorInfo::from(error)),
        }
    }

    /// Accept a pending friend request and record the new contact.
    async fn accept_request(&mut self, public_key: &str) -> super::protocol::ResponseResult {
        let key = public_key.trim();
        if key.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a friend request public key is required",
            ));
        }
        // Accepting creates a contact just as `/add` does, so the same limit
        // applies. Checked *before* the transport: a refused accept must leave the
        // request pending, because the user can make room and answer it later.
        if let Some(error) = self.friend_limit_error() {
            return err(error);
        }

        match self.transport.accept_request(key).await {
            Ok(accepted) => {
                info!(
                    "🤝 Accepted friend request from {}",
                    crate::utils::abbreviate(&accepted)
                );
                // Remember the new friend so `/list`, `/chat` and history work
                // exactly as they do for a manually added contact.
                let identifier = match validation::identifier(&accepted) {
                    Ok(identifier) => identifier,
                    Err(error) => return err(error),
                };
                if !self
                    .contacts
                    .iter()
                    .any(|contact| contact.name.eq_ignore_ascii_case(&identifier))
                {
                    let contact = Contact::new(identifier.clone(), identifier.as_bytes().to_vec());
                    if self.database.is_persistent() {
                        if let Err(error) = self.database.save_contact(&contact).await {
                            warn!("⚠️ Failed to persist the accepted contact: {error}");
                        }
                    }
                    self.contacts.push(contact);
                    if let Err(error) = self.save_session().await {
                        warn!("⚠️ Failed to persist the session after accepting: {error}");
                    }
                }
                ok(Reply::PeerRequestAccepted {
                    public_key: accepted,
                })
            }
            Err(error) => err(ErrorInfo::new(
                ErrorCode::Network,
                format!("could not accept the friend request: {error}"),
            )),
        }
    }

    /// Refresh the group cache from the transport, which is the source of truth.
    ///
    /// toxcore rejoins a conference after a restart and reports it on its own schedule, so
    /// a cache filled only by this session's own `/group` calls can be behind it — or
    /// empty, in a session that has not asked yet. A transport without groups has none to
    /// report, so whatever the cache holds is not a group of this session. A read of a
    /// transport that *does* have groups which fails keeps the cache: an answer that did
    /// not arrive is not evidence that the groups are gone.
    async fn refresh_group_cache(&mut self) {
        if !self.transport.supports_groups() {
            self.groups.clear();
            return;
        }

        match self.transport.groups().await {
            Ok(groups) => {
                for group in groups {
                    self.remember_group(&group.id, &group.name, group.members, group.joined);
                }
            }
            Err(error) => warn!("⚠️ Could not read the group list: {error}"),
        }
    }

    /// Answer `Request::Groups`.
    ///
    /// This is the one group request that succeeds on every transport: it reports
    /// `supported: false` with an empty list rather than failing, so a front-end
    /// can say *why* there are no groups instead of showing an error.
    async fn list_groups(&mut self) -> super::protocol::ResponseResult {
        let supported = self.transport.supports_groups();
        // Refresh before answering, so the list is the transport's rather than this
        // session's memory of it.
        self.refresh_group_cache().await;

        ok(Reply::Groups {
            supported,
            groups: self.groups.iter().map(group_view).collect(),
        })
    }

    /// Create a group and remember it.
    async fn create_group(&mut self, title: &str) -> super::protocol::ResponseResult {
        // The same boundary validation the transports are documented to assume.
        let title = match validation::group_name(title) {
            Ok(title) => title,
            Err(error) => return err(error),
        };
        let title = title.as_str();

        match self.transport.create_group(title).await {
            Ok(group) => {
                self.remember_group(&group.id, &group.name, group.members, group.joined);
                // The group just created is the one the user wants to talk to.
                self.active_group = Some(group.id.clone());
                ok(Reply::GroupCreated {
                    group: group_view(&group),
                })
            }
            Err(error) => err(group_error("create a group", error)),
        }
    }

    /// Join a group from a single-use invitation token.
    async fn join_group(&mut self, token: &str) -> super::protocol::ResponseResult {
        let token = token.trim();
        if token.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a group invitation token is required",
            ));
        }

        match self.transport.join_group(token).await {
            Ok(group) => {
                self.remember_group(&group.id, &group.name, group.members, group.joined);
                self.active_group = Some(group.id.clone());
                ok(Reply::GroupJoined {
                    group: group_view(&group),
                })
            }
            Err(error) => err(group_error("join the group", error)),
        }
    }

    /// Discard a pending group invitation.
    ///
    /// Synchronous like [`CoreService::reject_request`], and for the same reason:
    /// an invitation is a capability handed to us, so saying *no* sends nothing and
    /// touches no transport — there is nothing to await and no error a network
    /// could produce.
    fn decline_group_invite(&self, token: &str) -> super::protocol::ResponseResult {
        if let Some(error) = self.capability_refusal("discard a group invitation") {
            return err(error);
        }
        let token = token.trim();
        if token.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a group invitation token is required",
            ));
        }

        match self.transport.decline_group_invite(token) {
            Ok(token) => ok(Reply::GroupInviteDeclined { token }),
            Err(error) => err(group_error("discard the group invitation", error)),
        }
    }

    /// Rename a group.
    ///
    /// The new name is remembered in the session state as well as sent to the
    /// transport, so `/group list` and `/info` are right immediately; the transport
    /// reports our own change to the other participants, whose front-ends learn it
    /// through `GroupChanged` (our own callback does not fire for our own change).
    async fn rename_group(
        &mut self,
        group_id: Option<String>,
        name: &str,
    ) -> super::protocol::ResponseResult {
        if let Some(error) = self.capability_refusal("rename a group") {
            return err(error);
        }
        let name = match validation::group_name(name) {
            Ok(name) => name,
            Err(error) => return err(error),
        };
        if name.is_empty() {
            // `group_name` allows an empty value on purpose: a group can be
            // *created* before its title is known. A rename to nothing is different
            // — toxcore refuses a zero-length title with `INVALID_LENGTH`, which is
            // reported as a network failure no user can act on — so it is rejected
            // at the boundary with a usable message instead.
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a group name is required to rename a group",
            ));
        }
        let wanted = group_id.as_deref().unwrap_or_default();
        let Some(group) = self.resolve_group(wanted) else {
            return err(no_such_group());
        };

        match self.transport.rename_group(&group.id, &name).await {
            Ok(()) => {
                self.remember_group(&group.id, &name, group.members, group.joined);
                ok(Reply::GroupRenamed {
                    group_id: group.id,
                    group: name,
                })
            }
            Err(error) => err(group_error("rename the group", error)),
        }
    }

    /// Invite a peer to a group.
    async fn invite_to_group(&self, group_id: &str, peer: &str) -> super::protocol::ResponseResult {
        if let Some(error) = self.capability_refusal("invite to a group") {
            return err(error);
        }
        let Some(group) = self.resolve_group(group_id) else {
            return err(no_such_group());
        };
        let peer = peer.trim();
        if peer.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a peer to invite is required",
            ));
        }

        match self.transport.invite_to_group(&group.id, peer).await {
            Ok(()) => ok(Reply::GroupInvited {
                group_id: group.id,
                peer: peer.to_string(),
            }),
            Err(error) => err(group_error("invite the peer", error)),
        }
    }

    /// Send a message (or an action, or a binary payload) to a group.
    async fn send_group_message(
        &mut self,
        group_id: Option<String>,
        text: &str,
        kind: MessageKind,
        content_type: ContentType,
    ) -> super::protocol::ResponseResult {
        let text = text.trim();
        if text.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a message body is required",
            ));
        }
        if content_type.is_binary() && kind == MessageKind::Action {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a binary payload cannot be a third-person action",
            ));
        }
        if let Some(error) = self.capability_refusal("send to a group") {
            return err(error);
        }

        let wanted = group_id.as_deref().unwrap_or_default();
        let Some(group) = self.resolve_group(wanted) else {
            return err(no_such_group());
        };

        // The same validation and the same canonical body as a direct message, so
        // a group body cannot bypass the character or length rules.
        let max_length = self.config.app.max_message_length;
        let body: Vec<u8> = match content_type {
            ContentType::Text => match validation::message(text, max_length) {
                Ok(text) => text.into_bytes(),
                Err(error) => return err(error),
            },
            ContentType::Binary => match validation::binary(text, max_length) {
                Ok(bytes) => bytes,
                Err(error) => return err(error),
            },
        };

        // The stored/echoed body is computed before the payload moves, so the
        // canonical form is available for persistence.
        let stored = match content_type {
            ContentType::Text => String::from_utf8_lossy(&body).into_owned(),
            ContentType::Binary => hex::encode(&body),
        };

        // Tox conferences encrypt between peers, so (as for a direct Tox message)
        // the envelope is applied only when the transport provides none.
        let payload = if self.transport.provides_encryption() {
            body
        } else {
            match self.crypto.encrypt(&body) {
                Ok(ciphertext) => ciphertext,
                Err(error) => return err(ErrorInfo::from(error)),
            }
        };
        let wire_bytes = payload.len() as u64;

        if let Err(error) = self
            .transport
            .send_group_payload(&group.id, &payload, kind, content_type)
            .await
        {
            return err(group_error("send to the group", error));
        }

        self.state.statistics.messages_sent += 1;
        self.state.statistics.bytes_sent += wire_bytes;

        self.save_group_message(&group, &stored, wire_bytes, kind, content_type, false)
            .await;

        ok(Reply::GroupSent {
            group_id: group.id,
            group: group.name,
            wire_bytes,
        })
    }

    /// Leave a group.
    async fn leave_group(&mut self, group_id: Option<String>) -> super::protocol::ResponseResult {
        if let Some(error) = self.capability_refusal("leave a group") {
            return err(error);
        }
        let wanted = group_id.as_deref().unwrap_or_default();
        let Some(group) = self.resolve_group(wanted) else {
            return err(no_such_group());
        };

        match self.transport.leave_group(&group.id).await {
            Ok(()) => {
                let id = group.id.clone();
                self.groups.retain(|group| group.id != id);
                if self.active_group.as_deref() == Some(id.as_str()) {
                    self.active_group = None;
                }
                ok(Reply::GroupLeft { group_id: id })
            }
            Err(error) => err(group_error("leave the group", error)),
        }
    }

    /// The error for a group request against a transport without groups.
    ///
    /// Checked *before* looking at state: on TCP the honest reason is that the
    /// transport has no groups at all, not that a particular group is missing.
    fn capability_refusal(&self, operation: &str) -> Option<ErrorInfo> {
        (!self.transport.supports_groups())
            .then(|| ErrorInfo::from(crate::transport::group_unsupported_error(operation)))
    }

    /// Resolve a group by id, name or index; an empty selector uses the active
    /// group, or the only group when there is exactly one.
    fn resolve_group(&self, selector: &str) -> Option<Group> {
        let wanted = selector.trim();
        if wanted.is_empty() {
            let id = self
                .active_group
                .clone()
                // A slice pattern rather than `groups[0]`: it matches exactly one
                // element, so there is no index and no length check that have to agree.
                .or_else(|| match self.groups.as_slice() {
                    [only] => Some(only.id.clone()),
                    _ => None,
                })?;
            return self.groups.iter().find(|group| group.id == id).cloned();
        }

        // `1` selects the first group, like `/chat 1` selects the first contact.
        if let Ok(index) = wanted.parse::<usize>() {
            if index >= 1 {
                if let Some(group) = self.groups.get(index - 1) {
                    return Some(group.clone());
                }
            }
        }

        self.groups
            .iter()
            .find(|group| group.id.eq_ignore_ascii_case(wanted))
            .or_else(|| {
                self.groups
                    .iter()
                    .find(|group| group.name.eq_ignore_ascii_case(wanted))
            })
            .cloned()
    }

    /// Insert or update a group in the session state.
    fn remember_group(&mut self, group_id: &str, name: &str, members: usize, joined: bool) {
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.id.eq_ignore_ascii_case(group_id))
        {
            // A transport event does not always carry the name (a message that
            // arrived before the title was known), so an empty name never
            // overwrites a known one.
            if !name.is_empty() {
                group.name = name.to_string();
            }
            group.members = members;
            group.joined = joined;
            return;
        }
        self.groups.push(Group {
            id: group_id.to_string(),
            name: name.to_string(),
            members,
            joined,
        });
    }

    /// Fold an incoming group message into state, history and the event stream.
    #[allow(clippy::too_many_arguments)]
    async fn on_group_message(
        &mut self,
        group_id: &str,
        name: &str,
        peer: &str,
        peer_id: &str,
        payload: Vec<u8>,
        kind: MessageKind,
        content_type: ContentType,
        events: &broadcast::Sender<CoreEvent>,
    ) {
        let wire_bytes = payload.len() as u64;

        // A text body must be UTF-8 and free of the control characters a body may not
        // carry; an opaque one is expected to be opaque, so it is rendered as
        // hexadecimal rather than reported as undecodable. Both rules live in `body_of`,
        // so a direct message and a group message cannot disagree about them.
        let body = match body_of(payload, content_type) {
            Ok(body) => body,
            Err(reason) => {
                warn!(
                    "⚠️ A group frame from {peer} in {group_id} {reason}; reporting it as undecodable"
                );
                self.state.statistics.messages_received += 1;
                self.state.statistics.bytes_received += wire_bytes;
                publish(
                    events,
                    CoreEvent::MessageUndecodable {
                        peer: peer.to_string(),
                        peer_id: peer_id.to_string(),
                        wire_bytes,
                        reason: reason.to_string(),
                        group_id: Some(group_id.to_string()),
                        group: (!name.is_empty()).then(|| name.to_string()),
                    },
                );
                return;
            }
        };

        self.state.statistics.messages_received += 1;
        self.state.statistics.bytes_received += wire_bytes;

        // A group message is sometimes the first thing that mentions the group (a
        // conference loaded from savedata may not have produced an event yet), so
        // fold the group in before persisting anything.
        let group_name = if name.is_empty() {
            self.groups
                .iter()
                .find(|group| group.id.eq_ignore_ascii_case(group_id))
                .map_or_else(String::new, |group| group.name.clone())
        } else {
            name.to_string()
        };
        self.remember_group(group_id, &group_name, 0, true);

        let group = Group {
            id: group_id.to_string(),
            name: group_name.clone(),
            members: 0,
            joined: true,
        };
        self.save_group_message(&group, &body, wire_bytes, kind, content_type, true)
            .await;

        publish(
            events,
            CoreEvent::GroupMessageReceived {
                group_id: group_id.to_string(),
                group: group_name,
                peer: peer.to_string(),
                peer_id: peer_id.to_string(),
                wire_bytes,
                body,
                kind,
                content_type,
            },
        );
    }

    /// Mirror one group message into the durable store.
    async fn save_group_message(
        &self,
        group: &Group,
        body: &str,
        wire_bytes: u64,
        kind: MessageKind,
        content_type: ContentType,
        incoming: bool,
    ) {
        if !self.database.is_persistent() {
            return;
        }
        let direction = if incoming {
            MessageDirection::Incoming
        } else {
            MessageDirection::Outgoing
        };
        // The conversation is the group, so a restored history groups the messages
        // the way the live session did.
        let record =
            StoredMessage::in_group(direction, group, body, wire_bytes, kind, content_type);
        if let Err(error) = self.database.save_message(&record).await {
            warn!("⚠️ Failed to persist a group message: {error}");
        }
    }

    /// Fold a transport event into state and republish it as a [`CoreEvent`].
    ///
    /// # Returns
    ///
    /// Returns `true` when the service must stop.
    ///
    /// One arm per `AppEvent` on purpose: the actor's state machine is this
    /// `match`, and an event it forgets to handle is a bug that should be visible
    /// in the diff rather than hidden inside a helper.
    #[allow(clippy::too_many_lines)]
    async fn on_app_event(
        &mut self,
        event: AppEvent,
        events: &broadcast::Sender<CoreEvent>,
    ) -> bool {
        match event {
            AppEvent::MessageReceived {
                peer,
                peer_id,
                payload,
                kind,
                content_type,
            } => {
                let wire_bytes = payload.len() as u64;

                // A text payload must be UTF-8 and free of control characters; a binary
                // payload is opaque *by contract*, so it is rendered as hexadecimal and
                // is never a decode failure. A payload that fails either rule is reported
                // as undecodable instead of being mangled into text a user would read and
                // believe — and is not stored, so it never comes back as history either.
                let body = match body_of(payload, content_type) {
                    Ok(body) => body,
                    Err(reason) => {
                        warn!("⚠️ A frame from {peer_id} {reason}; reporting it as undecodable");
                        self.state.statistics.messages_received += 1;
                        self.state.statistics.bytes_received += wire_bytes;
                        publish(
                            events,
                            CoreEvent::MessageUndecodable {
                                peer,
                                peer_id,
                                wire_bytes,
                                reason: reason.to_string(),
                                group_id: None,
                                group: None,
                            },
                        );
                        return false;
                    }
                };

                self.state.statistics.messages_received += 1;
                self.state.statistics.bytes_received += wire_bytes;

                if self.database.is_persistent() {
                    let record = StoredMessage::new(
                        MessageDirection::Incoming,
                        peer.clone(),
                        body.clone(),
                        wire_bytes,
                        kind,
                        content_type,
                    );
                    if let Err(error) = self.database.save_message(&record).await {
                        warn!("⚠️ Failed to persist an incoming message: {error}");
                    }
                }

                publish(
                    events,
                    CoreEvent::MessageReceived {
                        peer,
                        peer_id,
                        body,
                        wire_bytes,
                        kind,
                        content_type,
                    },
                );
                false
            }
            AppEvent::MessageDelivered {
                peer,
                peer_id,
                message_id,
            } => {
                publish(
                    events,
                    CoreEvent::MessageDelivered {
                        peer,
                        peer_id,
                        message_id,
                    },
                );
                false
            }
            AppEvent::PeerRequestReceived { peer_id, message } => {
                // Surface it for an explicit decision; nothing is accepted here.
                info!(
                    "📨 Friend request from {}",
                    crate::utils::abbreviate(&peer_id)
                );
                publish(
                    events,
                    CoreEvent::PeerRequestReceived {
                        short_key: crate::utils::abbreviate(&peer_id),
                        public_key: peer_id,
                        message,
                    },
                );
                false
            }
            AppEvent::NetworkEvent(network_event) => {
                Self::on_network_event(network_event, events);
                false
            }
            AppEvent::FriendStatusChanged {
                friend_id,
                is_online,
            } => {
                if let Some(contact) = self.contacts.iter_mut().find(|c| c.id == friend_id) {
                    contact.status = if is_online {
                        UserStatus::Online
                    } else {
                        UserStatus::Offline
                    };
                    if is_online {
                        contact.update_last_seen();
                    }
                }
                false
            }
            // The backend has no user interface, so raw input is not its
            // business.
            AppEvent::UserInput(_) => false,
            AppEvent::GroupMessageReceived {
                group_id,
                name,
                peer,
                peer_id,
                payload,
                kind,
                content_type,
            } => {
                self.on_group_message(
                    &group_id,
                    &name,
                    &peer,
                    &peer_id,
                    payload,
                    kind,
                    content_type,
                    events,
                )
                .await;
                false
            }
            AppEvent::GroupChanged {
                group_id,
                name,
                members,
                joined,
            } => {
                self.remember_group(&group_id, &name, members, joined);
                publish(
                    events,
                    CoreEvent::GroupChanged {
                        group_id,
                        group: name,
                        members,
                        joined,
                    },
                );
                false
            }
            AppEvent::GroupInviteReceived {
                peer,
                peer_id,
                token,
            } => {
                // Never accepted automatically: a front-end decides. The token is
                // held by the transport, which also makes it single-use.
                publish(
                    events,
                    CoreEvent::GroupInviteReceived {
                        peer,
                        peer_id,
                        token,
                    },
                );
                false
            }
            AppEvent::Shutdown => {
                debug!("🛑 Transport requested a global shutdown");
                true
            }
        }
    }

    /// Translate a transport-level event into the public event stream.
    ///
    /// An associated function rather than a method: it touches only the public
    /// event stream, which the caller already holds, so borrowing `self` here
    /// would suggest state the translation does not read.
    fn on_network_event(event: crate::types::NetworkEvent, events: &broadcast::Sender<CoreEvent>) {
        match event {
            crate::types::NetworkEvent::PeerConnected { peer_id, metadata } => {
                let nickname = metadata.get("nickname").cloned().unwrap_or_default();
                publish(events, CoreEvent::PeerConnected { peer_id, nickname });
            }
            crate::types::NetworkEvent::PeerDisconnected { peer_id, reason } => {
                publish(events, CoreEvent::PeerDisconnected { peer_id, reason });
            }
            crate::types::NetworkEvent::BootstrapStatus {
                node_address,
                connected,
            } => {
                debug!("🌐 Bootstrap {node_address} connected={connected}");
            }
        }
    }

    /// Add a friend by DID-like identifier.
    async fn add_contact(
        &mut self,
        identifier: String,
        note: Option<String>,
    ) -> super::protocol::ResponseResult {
        // Validate before touching any state so a rejected request leaves the
        // friend list, the store and the statistics untouched.
        let identifier = match validation::identifier(&identifier) {
            Ok(identifier) => identifier,
            Err(error) => return err(error),
        };
        let note = match validation::note(note) {
            Ok(note) => note,
            Err(error) => return err(error),
        };

        if let Some(error) = self.friend_limit_error() {
            return err(error);
        }

        // The placeholder public key stores the identifier bytes, so decode it
        // back to a string for a case-insensitive duplicate check.
        let duplicate = self.contacts.iter().any(|c| {
            String::from_utf8_lossy(&c.public_key).eq_ignore_ascii_case(&identifier)
                || c.name.eq_ignore_ascii_case(&identifier)
        });
        if duplicate {
            return ok(Reply::ContactExists { identifier });
        }

        let mut contact = Contact::new(identifier.clone(), identifier.as_bytes().to_vec());
        contact.note = note;

        // On Tox, naming a friend is what sends the request: the transport turns
        // the address into a `tox_friend_add` and the peer still has to accept.
        // On TCP the peer is merely remembered here and reached with `/connect`
        // or `--peer`, so this is a no-op.
        if self.transport.supports_friend_requests() {
            match self.transport.request_peer(&identifier).await {
                // Record the peer's public key so later sends do not depend on
                // how the address was formatted.
                Ok(Some(public_key)) => contact.public_key = public_key.into_bytes(),
                Ok(None) => {}
                Err(error) => {
                    return err(ErrorInfo::new(
                        ErrorCode::Network,
                        format!("could not send a friend request to {identifier}: {error}"),
                    ))
                }
            }
        }

        // Mirror the new contact into the durable store when one is available.
        if self.database.is_persistent() {
            if let Err(error) = self.database.save_contact(&contact).await {
                warn!("⚠️ Failed to persist contact {identifier}: {error}");
            }
        }

        self.contacts.push(contact);
        ok(Reply::ContactAdded {
            index: self.contacts.len(),
            identifier,
        })
    }

    /// Remove a friend by 1-based index or name fragment.
    async fn remove_contact(&mut self, target: &str) -> super::protocol::ResponseResult {
        let target = target.trim();
        if target.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a contact index or name is required",
            ));
        }

        let Some(index) = find_contact_index(&self.contacts, target) else {
            return err(ErrorInfo::new(
                ErrorCode::NotFound,
                format!("no friend matches '{target}'"),
            ));
        };

        let removed = self.contacts.remove(index);

        // Drop the active conversation if it pointed at the removed contact.
        if self.active_chat == Some(removed.id) {
            self.active_chat = None;
        }

        // Persist immediately so a crash cannot resurrect the friend.
        if let Err(error) = self.save_session().await {
            warn!("⚠️ Failed to persist the contact list after removal: {error}");
        }

        ok(Reply::ContactRemoved {
            index: index + 1,
            name: removed.name,
        })
    }

    /// Change (or report) the local nickname.
    async fn set_nickname(
        &mut self,
        nickname: String,
        events: &broadcast::Sender<CoreEvent>,
    ) -> super::protocol::ResponseResult {
        let nickname = nickname.trim().to_string();
        if nickname.is_empty() {
            return ok(Reply::Updated {
                subject: "nickname".to_string(),
                detail: self.nickname.clone(),
            });
        }
        let nickname = match validation::nickname(&nickname) {
            Ok(nickname) => nickname,
            Err(error) => return err(error),
        };

        self.nickname.clone_from(&nickname);
        // Let every connected peer know about the new name.
        self.transport.set_nickname(&self.nickname).await;
        publish(
            events,
            CoreEvent::NicknameChanged {
                nickname: self.nickname.clone(),
            },
        );

        ok(Reply::Updated {
            subject: "nickname".to_string(),
            detail: self.nickname.clone(),
        })
    }

    /// Change (or report) the personal status message.
    fn set_status(
        &mut self,
        text: &str,
        events: &broadcast::Sender<CoreEvent>,
    ) -> super::protocol::ResponseResult {
        let text = text.trim().to_string();
        if text.is_empty() {
            return ok(Reply::Updated {
                subject: "status".to_string(),
                detail: self.status_message.clone(),
            });
        }
        let text = match validation::status(&text) {
            Ok(text) => text,
            Err(error) => return err(error),
        };

        self.status_message.clone_from(&text);
        publish(
            events,
            CoreEvent::StatusChanged {
                text: self.status_message.clone(),
            },
        );

        ok(Reply::Updated {
            subject: "status".to_string(),
            detail: self.status_message.clone(),
        })
    }

    /// Resolve a `/chat` target and activate it.
    ///
    /// An empty target reports the current conversation instead of changing it,
    /// so `/chat` doubles as a status query.
    fn select_conversation(&mut self, target: &str) -> super::protocol::ResponseResult {
        let target = target.trim();
        if target.is_empty() {
            let (active, index) = self.active_conversation();
            return ok(Reply::Conversation { active, index });
        }

        if let Ok(index) = target.parse::<usize>() {
            if index == 0 {
                return err(ErrorInfo::new(
                    ErrorCode::InvalidRequest,
                    "friend numbers start at 1",
                ));
            }
        }

        let Some(index) = find_contact_index(&self.contacts, target) else {
            return err(ErrorInfo::new(
                ErrorCode::NotFound,
                format!("no friend matches '{target}'"),
            ));
        };

        self.active_chat = Some(self.contacts[index].id);
        ok(Reply::Conversation {
            active: Some(self.contacts[index].name.clone()),
            index: Some(index + 1),
        })
    }

    /// Describe the currently selected conversation.
    ///
    /// # Returns
    ///
    /// Returns the contact name and its 1-based position, if one is selected.
    fn active_conversation(&self) -> (Option<String>, Option<usize>) {
        let found = self.active_chat.and_then(|id| {
            self.contacts
                .iter()
                .position(|contact| contact.id == id)
                .map(|position| (position + 1, self.contacts[position].name.clone()))
        });

        match found {
            Some((index, name)) => (Some(name), Some(index)),
            None => (None, None),
        }
    }

    /// Register a peer address and keep it reachable.
    ///
    /// The accepted shape depends on the transport: `host:port` for TCP, a 76
    /// character Tox address for Tox. The validation here mirrors the command
    /// line so a front-end cannot bypass it.
    async fn connect(&self, address: &str) -> super::protocol::ResponseResult {
        let address = address.trim();
        if address.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a peer address is required",
            ));
        }

        let valid = match self.args.transport {
            crate::cli::Transport::Tcp => CliArgs::is_valid_peer_address(address),
            crate::cli::Transport::Tox => CliArgs::is_valid_tox_address(address),
        };
        if !valid {
            let expected = match self.args.transport {
                crate::cli::Transport::Tcp => "a valid host:port address",
                crate::cli::Transport::Tox => "a valid 76 character Tox address",
            };
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                format!("'{address}' is not {expected}"),
            ));
        }

        match self.transport.request_peer(address).await {
            Ok(detail) => ok(Reply::Updated {
                subject: "connection".to_string(),
                detail: detail.unwrap_or_else(|| address.to_string()),
            }),
            Err(error) => err(ErrorInfo::new(
                ErrorCode::Network,
                format!("could not reach {address}: {error}"),
            )),
        }
    }

    /// Validated and routed a chat message, a third-person action or a binary
    /// payload.
    async fn send_message(
        &mut self,
        target: Option<String>,
        text: &str,
        kind: MessageKind,
        content_type: ContentType,
    ) -> super::protocol::ResponseResult {
        let text = text.trim();
        if text.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a message body is required",
            ));
        }

        // A binary blob is never a gesture: `* Alice <opaque bytes>` is
        // meaningless, and refusing the combination here keeps the frame-kind
        // mapping total.
        if content_type.is_binary() && kind == MessageKind::Action {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a binary payload cannot be a third-person action",
            ));
        }

        // Enforce the configured length limit and the character rules before
        // doing any crypto work, so a rejected body has no side effects.
        let max_length = self.config.app.max_message_length;
        let body: Vec<u8> = match content_type {
            ContentType::Text => match validation::message(text, max_length) {
                Ok(text) => text.into_bytes(),
                Err(error) => return err(error),
            },
            ContentType::Binary => {
                // The JSON interface carries binary as hexadecimal, so the limit
                // applies to the decoded bytes, not to their representation.
                match validation::binary(text, max_length) {
                    Ok(bytes) => bytes,
                    Err(error) => return err(error),
                }
            }
        };
        // The stored/echoed body is the canonical form: text as typed, binary as
        // lowercase hexadecimal.
        let stored_body = match content_type {
            ContentType::Text => String::from_utf8_lossy(&body).into_owned(),
            ContentType::Binary => hex::encode(&body),
        };

        // Resolve the destination: an explicit nickname wins, otherwise the
        // active conversation is used.
        let explicit = target
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let destination = match explicit {
            Some(name) => name.to_string(),
            None => match self.active_chat {
                Some(id) => self
                    .contacts
                    .iter()
                    .find(|contact| contact.id == id)
                    .map_or_else(|| id.to_string(), |contact| contact.name.clone()),
                None => {
                    return err(ErrorInfo::new(
                        ErrorCode::NotFound,
                        "no active conversation; select one first",
                    ))
                }
            },
        };

        // The Tox transport already encrypts between friends, so the metaText
        // envelope is applied only when the transport provides none; the size
        // reported to the user is whatever actually crossed the transport.
        //
        // The envelope uses the *contact's* key when the pair has one (both ends
        // announced an identity), and the session key otherwise — a broadcast, an
        // offline peer whose message is buffered, or a peer that announced nothing.
        let (payload, encrypted) = if self.transport.provides_encryption() {
            (body, true)
        } else {
            let key = self.transport.contact_key(&destination);
            let sealed = match key.as_deref() {
                Some(key) => self.crypto.encrypt_with(key, &body),
                None => self.crypto.encrypt(&body),
            };
            match sealed {
                Ok(ciphertext) => (ciphertext, self.crypto.is_enabled()),
                Err(error) => return err(ErrorInfo::from(error)),
            }
        };
        let wire_bytes = payload.len() as u64;

        let outcome = match self
            .transport
            .send_payload(&destination, &payload, kind, content_type)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return err(ErrorInfo::from(error)),
        };

        self.state.statistics.messages_sent += 1;
        self.state.statistics.bytes_sent += wire_bytes;

        // Mirror the message into the durable store when one is available.
        let mut persisted = false;
        if self.database.is_persistent() {
            let record = StoredMessage::new(
                MessageDirection::Outgoing,
                destination.clone(),
                stored_body,
                wire_bytes,
                kind,
                content_type,
            );
            match self.database.save_message(&record).await {
                Ok(()) => persisted = true,
                Err(error) => warn!("⚠️ Failed to persist an outgoing message: {error}"),
            }
        }

        ok(Reply::Sent {
            target: destination,
            report: send_report(outcome, payload.len(), encrypted, persisted),
        })
    }

    /// The most recent persisted messages.
    ///
    /// An absent `limit` is the session's page size: `--history-limit`, or
    /// [`DEFAULT_HISTORY_LIMIT`](super::protocol::DEFAULT_HISTORY_LIMIT) when the session
    /// was started without the flag. The value is clamped because it may also arrive from
    /// an out-of-process front-end, which the core cannot hold to the flag's range.
    async fn history(&self, limit: Option<usize>) -> super::protocol::ResponseResult {
        let persistent = self.database.is_persistent();
        if !persistent {
            return ok(Reply::History {
                persistent,
                messages: Vec::new(),
            });
        }

        let limit = limit
            .unwrap_or(self.args.history_limit)
            .clamp(1, super::protocol::MAX_HISTORY_LIMIT);
        match self.database.recent_messages(limit).await {
            Ok(messages) => ok(Reply::History {
                persistent,
                messages: messages
                    .iter()
                    .map(|message| MessageView {
                        direction: message.direction.as_str().to_string(),
                        peer: message.peer.clone(),
                        body: message.body.clone(),
                        wire_bytes: message.wire_bytes,
                        kind: message.kind,
                        content_type: message.content_type,
                        created_at: message.created_at.to_rfc3339(),
                    })
                    .collect(),
            }),
            Err(error) => err(ErrorInfo::from(error)),
        }
    }

    /// Build the session snapshot consumed by the front-ends.
    fn session_info(&self) -> SessionInfo {
        let uptime_seconds = chrono::Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds()
            .max(0);

        let (active_chat, _) = self.active_conversation();

        SessionInfo {
            nickname: self.nickname.clone(),
            status_message: self.status_message.clone(),
            identity: self.identity.public_hex(),
            identity_fingerprint: self.identity.fingerprint(),
            peer_identities: self
                .transport
                .peer_identities()
                .into_iter()
                .map(|peer| PeerIdentityView {
                    nickname: peer.nickname,
                    identity: peer.public_key,
                    fingerprint: peer.fingerprint,
                    changed: peer.changed,
                })
                .collect(),
            mode: self.args.effective_mode().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Always the configured algorithm; `encryption_enabled` tells the
            // front-end whether it is actually in force.
            encryption: self.crypto.algorithm().to_string(),
            encryption_enabled: self.crypto.is_enabled(),
            network_running: self.transport.is_started(),
            network_port: self.transport.port(),
            network_bootstrap_nodes: self.transport.bootstrap_nodes().len(),
            connected_peers: self.transport.connected_peers(),
            pending_peers: self.transport.pending_peers(),
            max_connections: self.transport.max_connections(),
            queued_messages: self.transport.queued_messages(),
            desired_peers: self.transport.desired_peers(),
            peer_nicknames: self.transport.peer_nicknames(),
            local_address: self.transport.local_address(),
            database_connected: self.database.is_initialized(),
            database: self.database.connection_string().to_string(),
            persistent: self.database.is_persistent(),
            database_max_connections: self.database.max_connections(),
            friend_count: self.contacts.len(),
            max_friends: self.config.app.max_friends,
            active_chat,
            uptime_seconds,
            config_path: self.args.config_path.display().to_string(),
            data_dir: self
                .args
                .data_directory()
                .unwrap_or_else(|| PathBuf::from("."))
                .display()
                .to_string(),
            app_name: self.config.app.name.clone(),
            app_version: self.config.app.version.clone(),
            max_message_length: self.config.app.max_message_length,
            auto_save_interval: self.config.app.auto_save_interval,
            transport: self.transport.name().to_string(),
            public_identity: self.transport.public_identity(),
            pending_requests: self.transport.pending_requests().len(),
            supports_friend_requests: self.transport.supports_friend_requests(),
            groups: self.groups.len(),
            pending_group_invites: self.transport.pending_group_invites().len(),
            supports_groups: self.transport.supports_groups(),
        }
    }

    /// Build the runtime counter snapshot.
    fn statistics_view(&self) -> StatisticsView {
        let counters = &self.state.statistics;
        let uptime_seconds = chrono::Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds()
            .max(0);

        StatisticsView {
            messages_sent: counters.messages_sent,
            messages_received: counters.messages_received,
            active_connections: counters.active_connections,
            bytes_sent: counters.bytes_sent,
            bytes_received: counters.bytes_received,
            uptime_seconds,
        }
    }

    /// Build the operational snapshot consumed by `/metrics`.
    ///
    /// Every value is read from a field or an atomic, so this cannot block and is
    /// safe to poll from a monitor. `events` is needed only for the subscriber
    /// count, which is a property of the broadcast channel rather than of the
    /// service.
    fn metrics_view(&self, events: &broadcast::Sender<CoreEvent>) -> MetricsView {
        let counters = &self.state.statistics;
        let uptime_seconds = chrono::Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds()
            .max(0);

        MetricsView {
            uptime_seconds,
            transport: self.transport.name().to_string(),
            peers_connected: self.transport.connected_peers(),
            peers_pending: self.transport.pending_peers(),
            payloads_queued: self.transport.queued_messages(),
            payloads_expired: self.transport.expired_messages(),
            payloads_dropped: self.transport.dropped_payloads(),
            friend_count: self.contacts.len(),
            max_friends: self.config.app.max_friends,
            friend_requests_pending: self.transport.pending_requests().len(),
            friend_requests_dropped: self.transport.dropped_requests(),
            group_invites_dropped: self.transport.dropped_invites(),
            groups: self.groups.len(),
            groups_supported: self.transport.supports_groups(),
            request_queue_depth: self.command_queue_depth,
            request_queue_capacity: self.options.command_capacity,
            event_subscribers: events.receiver_count(),
            request_wait_last_us: micros(self.request_wait_last),
            request_wait_max_us: micros(self.request_wait_max),
            request_service_last_us: micros(self.request_service_last),
            request_service_max_us: micros(self.request_service_max),
            requests_served: self.requests_served,
            messages_sent: counters.messages_sent,
            messages_received: counters.messages_received,
            bytes_sent: counters.bytes_sent,
            bytes_received: counters.bytes_received,
            database_connected: self.database.is_initialized(),
            persistent: self.database.is_persistent(),
            peer_identities_changed: self.transport.pinned_identity_changes(),
            peer_identities_refused: self.transport.pinned_identity_refusals(),
            greetings_refused: self.transport.refused_greetings(),
        }
    }

    /// Persist nickname, contacts and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or the snapshot
    /// cannot be serialized or written.
    async fn save_session(&self) -> Result<()> {
        if let Some(parent) = self.session_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create data directory {}", parent.display()))?;
        }

        let snapshot = SessionState {
            nickname: self.nickname.clone(),
            status_message: self.status_message.clone(),
            contacts: self.contacts.clone(),
            statistics: crate::types::AppStatistics {
                // The start time is session-only, so it is not persisted.
                start_time: None,
                ..self.state.statistics.clone()
            },
        };

        let json =
            serde_json::to_string_pretty(&snapshot).context("Failed to serialize session state")?;

        // Write a sibling file and rename it over the target. `rename` is atomic
        // on the same filesystem, so a crash or a full disk can no longer leave
        // a truncated `metatext-session.json` behind: readers see either the old
        // snapshot or the complete new one.
        let temporary = self.session_path.with_extension("tmp");
        tokio::fs::write(&temporary, json)
            .await
            .with_context(|| format!("Failed to write {}", temporary.display()))?;
        tokio::fs::rename(&temporary, &self.session_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to replace {} with the new snapshot",
                    self.session_path.display()
                )
            })?;

        debug!("💾 Session saved to {}", self.session_path.display());
        Ok(())
    }

    /// Restore a previously saved session, if one exists.
    async fn load_session(&mut self) {
        let Ok(raw) = tokio::fs::read_to_string(&self.session_path).await else {
            debug!("ℹ️ No session file at {}", self.session_path.display());
            return;
        };

        match serde_json::from_str::<SessionState>(&raw) {
            Ok(snapshot) => {
                // `--nick` wins over the persisted session, matching the
                // "flags always win over file values" contract.
                if !snapshot.nickname.is_empty() && self.args.nickname.is_none() {
                    self.nickname = snapshot.nickname;
                }
                if !snapshot.status_message.is_empty() {
                    self.status_message = snapshot.status_message;
                }
                self.contacts = snapshot.contacts;
                self.state.statistics = snapshot.statistics;
                info!(
                    "✅ Session restored from {} ({} friend(s))",
                    self.session_path.display(),
                    self.contacts.len()
                );
            }
            Err(error) => {
                warn!(
                    "⚠️ Ignoring unreadable session file {}: {error}",
                    self.session_path.display()
                );
            }
        }
    }
}

/// Wrap a reply in a successful [`ResponseResult`].
const fn ok(reply: Reply) -> super::protocol::ResponseResult {
    super::protocol::ResponseResult::ok(reply)
}

/// A duration in whole microseconds, saturating rather than wrapping.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// The error for a group selector that matched nothing.
fn no_such_group() -> ErrorInfo {
    ErrorInfo::new(
        ErrorCode::NotFound,
        "no such group; use /group list to see the groups this session is in",
    )
}

/// Wrap a transport failure in the group context.
///
/// The code comes from the underlying error rather than being forced to
/// `Network`: "this transport has no groups" is a validation failure, and
/// reporting it as a network problem would send a front-end looking for a
/// connection issue that does not exist. Only the message is decorated with the
/// operation that failed.
fn group_error(operation: &str, error: crate::error::MetaTextError) -> ErrorInfo {
    let mut info = ErrorInfo::from(error);
    info.message = format!("could not {operation}: {}", info.message);
    info
}

/// Flatten a group for the wire, abbreviating its identifier for display.
fn group_view(group: &Group) -> GroupView {
    GroupView {
        id: group.id.clone(),
        name: group.name.clone(),
        members: group.members,
        joined: group.joined,
        short_id: crate::utils::abbreviate(&group.id),
    }
}

/// Broadcast an event; a lack of subscribers is not an error.
fn publish(events: &broadcast::Sender<CoreEvent>, event: CoreEvent) {
    // `send` only fails when nobody is subscribed, which is a normal state
    // (for example a headless deployment).
    let _ = events.send(event);
}

/// The text of an incoming payload, or the reason it cannot be shown.
///
/// A body is *printed* by a front-end, so it is the one thing in the protocol that can
/// carry a command to the reader's terminal: `ESC[2J` clears the screen, `ESC]52;…`
/// writes the clipboard, a bare `\r` moves the caret back over text the user already
/// read. The send path has always refused a body containing a control character other
/// than `\n` and `\t` (`ALLOWED_MESSAGE_CONTROLS`), which is exactly the argument — but
/// it guarded the direction that cannot be attacked. This is the receiving side of the
/// same rule, applied once for both transports and both conversation kinds because it is
/// the core, not a front-end, that decides what a body is allowed to contain.
///
/// A failure is a *reason*, not a replacement body: showing the payload with the control
/// characters stripped would display something the peer did not send, which is what the
/// project refuses to do (A11 in `docs/ARCHITECTURE.md`). A binary payload is opaque by
/// contract and is rendered as hexadecimal, so it never fails this check.
fn body_of(payload: Vec<u8>, content_type: ContentType) -> Result<String, &'static str> {
    if content_type.is_binary() {
        return Ok(hex::encode(&payload));
    }

    let Ok(text) = String::from_utf8(payload) else {
        return Err("is not valid UTF-8 text");
    };

    if protocol::contains_forbidden_control(&text, &ALLOWED_MESSAGE_CONTROLS) {
        return Err("contains control characters");
    }

    Ok(text)
}

/// Wrap an error in a failed [`ResponseResult`].
const fn err(error: ErrorInfo) -> super::protocol::ResponseResult {
    super::protocol::ResponseResult::err(error)
}

/// Flatten a [`SendOutcome`] into a wire report.
const fn send_report(
    outcome: SendOutcome,
    wire_bytes: usize,
    encrypted: bool,
    persisted: bool,
) -> SendReport {
    let (kind, message_id, queued_for, queue_position) = match outcome {
        SendOutcome::Sent(receipt) => (
            SendOutcomeKind::Sent,
            Some(receipt.message_id),
            receipt.peers,
            None,
        ),
        SendOutcome::Queued {
            message_id,
            position,
        } => (SendOutcomeKind::Queued, Some(message_id), 0, Some(position)),
        SendOutcome::Dropped => (SendOutcomeKind::Dropped, None, 0, None),
    };

    SendReport {
        outcome: kind,
        message_id,
        queued_for,
        queue_position,
        wire_bytes: wire_bytes as u64,
        // A short preview is enough to show the ciphertext is not plain text.
        ciphertext_preview: String::new(),
        encrypted,
        persisted,
    }
}

/// Flatten a [`Contact`] for the wire.
fn contact_view(contact: &Contact) -> ContactView {
    ContactView {
        id: contact.id.to_string(),
        name: contact.name.clone(),
        status: contact.status.as_name().to_string(),
        online: contact.is_online(),
        note: contact.note.clone(),
        blocked: contact.is_blocked,
        last_seen: contact.last_seen.map(|seen| seen.to_rfc3339()),
    }
}

/// Resolve a 1-based index or a case-insensitive name fragment.
fn find_contact_index(contacts: &[Contact], target: &str) -> Option<usize> {
    if let Ok(index) = target.parse::<usize>() {
        return index
            .checked_sub(1)
            .filter(|position| *position < contacts.len());
    }

    let lowered = target.to_ascii_lowercase();
    contacts
        .iter()
        .position(|contact| contact.name.to_ascii_lowercase().contains(&lowered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an unstarted service in an isolated workspace.
    async fn service() -> (CoreService, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.database.connection_string =
            dir.path().join("core.db").to_string_lossy().into_owned();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            port: Some(0),
            ..CliArgs::default()
        };

        let service = CoreService::new(config, args).await.expect("service");
        (service, dir)
    }

    /// Create an event bus plus a receiver watching it.
    fn bus() -> (broadcast::Sender<CoreEvent>, broadcast::Receiver<CoreEvent>) {
        broadcast::channel(16)
    }

    /// Drain the next event, failing the test if none arrives.
    async fn next_event(receiver: &mut broadcast::Receiver<CoreEvent>) -> CoreEvent {
        tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("an event must arrive")
            .expect("the channel must stay open")
    }

    /// A valid text frame becomes a `MessageReceived` event and is counted.
    #[tokio::test]
    async fn test_valid_message_is_published_and_counted() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        let stop = service
            .on_app_event(
                AppEvent::MessageReceived {
                    peer: "Bob".to_string(),
                    peer_id: "127.0.0.1:1".to_string(),
                    payload: b"hello world".to_vec(),
                    kind: MessageKind::Text,
                    content_type: ContentType::Text,
                },
                &sender,
            )
            .await;
        assert!(!stop);

        match next_event(&mut receiver).await {
            CoreEvent::MessageReceived {
                peer,
                body,
                wire_bytes,
                ..
            } => {
                assert_eq!(peer, "Bob");
                assert_eq!(body, "hello world");
                assert_eq!(wire_bytes, 11);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(service.state.statistics.bytes_received, 11);
    }

    /// A frame that is not valid UTF-8 is reported, never mangled.
    #[tokio::test]
    async fn test_undecodable_message_is_reported_not_mangled() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        // 0xFF is never a valid UTF-8 lead byte.
        let payload = vec![0x41, 0xFF, 0x42];
        let stop = service
            .on_app_event(
                AppEvent::MessageReceived {
                    peer: "Mallory".to_string(),
                    peer_id: "127.0.0.1:2".to_string(),
                    payload: payload.clone(),
                    kind: MessageKind::Text,
                    content_type: ContentType::Text,
                },
                &sender,
            )
            .await;
        assert!(!stop);

        match next_event(&mut receiver).await {
            CoreEvent::MessageUndecodable {
                peer,
                wire_bytes,
                reason,
                group_id,
                group,
                ..
            } => {
                assert_eq!(peer, "Mallory");
                assert_eq!(wire_bytes, payload.len() as u64);
                assert_eq!(reason, "is not valid UTF-8 text");
                assert_eq!(group_id, None, "a direct message has no group");
                assert_eq!(group, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // The attempt is still accounted for, but nothing was stored as text.
        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(service.state.statistics.bytes_received, 3);
    }

    /// A body carrying a terminal control character is reported, not displayed.
    ///
    /// The send path has always refused such a body, and the protocol's own note says
    /// why: a body is printed, so `ESC` in one is a command to the reader's terminal
    /// (clear the screen, write the clipboard, move the caret back over text that was
    /// already read). Nothing checked the *receiving* side — the direction a peer
    /// controls — so a peer (or another implementation, or a front-end that skipped
    /// validation) could put anything it liked on the user's screen. The rule now holds
    /// where the body is turned into text, for both transports and both conversation
    /// kinds, and the frame is reported instead of being shown or quietly dropped.
    #[tokio::test]
    async fn test_a_body_with_control_characters_is_reported_not_displayed() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        // `ESC[2J` clears the screen; the text around it is what a user would see.
        let payload = b"owned\x1b[2J".to_vec();
        let stop = service
            .on_app_event(
                AppEvent::MessageReceived {
                    peer: "Mallory".to_string(),
                    peer_id: "127.0.0.1:9".to_string(),
                    payload: payload.clone(),
                    kind: MessageKind::Text,
                    content_type: ContentType::Text,
                },
                &sender,
            )
            .await;
        assert!(!stop);

        match next_event(&mut receiver).await {
            CoreEvent::MessageUndecodable {
                peer,
                wire_bytes,
                reason,
                group,
                ..
            } => {
                assert_eq!(peer, "Mallory");
                assert_eq!(wire_bytes, payload.len() as u64);
                assert_eq!(reason, "contains control characters");
                assert_eq!(group, None);
            }
            other => panic!("a body with a control character must be reported: {other:?}"),
        }

        // Counted, so the loss is visible in `/stats`, and not stored, so it cannot come
        // back as `/history` either.
        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(
            service.state.statistics.bytes_received,
            payload.len() as u64
        );
        #[cfg(feature = "sqlite")]
        {
            service.start().await.expect("start");
            assert_eq!(
                service.database.message_count().await.expect("count"),
                0,
                "a refused body must not be persisted"
            );
        }
    }

    /// Newlines and tabs are legitimate in a chat body and stay legitimate.
    ///
    /// The rule added above is the rule the send path already applied, so it must not
    /// narrow what a peer can say: a multi-line message is the reason `\n` is on the
    /// allowed list, and a body that used it kept working before this check existed.
    #[tokio::test]
    async fn test_newlines_and_tabs_still_survive_in_a_body() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        let body = "first line\nsecond\tline";
        let payload = body.as_bytes().to_vec();
        let stop = service
            .on_app_event(
                AppEvent::MessageReceived {
                    peer: "Alice".to_string(),
                    peer_id: "127.0.0.1:10".to_string(),
                    payload: payload.clone(),
                    kind: MessageKind::Text,
                    content_type: ContentType::Text,
                },
                &sender,
            )
            .await;
        assert!(!stop);

        match next_event(&mut receiver).await {
            CoreEvent::MessageReceived {
                body: delivered, ..
            } => assert_eq!(delivered, body),
            other => panic!("a multi-line body must still arrive: {other:?}"),
        }
        assert_eq!(
            service.state.statistics.bytes_received,
            payload.len() as u64
        );
    }

    /// The same rule holds inside a group, and the report names the group.
    ///
    /// A group frame that failed this check used to be *silently* dropped (a `warn!` and
    /// nothing else), so a user could lose a message without being told: the direct path
    /// reports it, and the group path now does too — with the group's name, because a user
    /// has to know which conversation lost something.
    #[tokio::test]
    async fn test_a_group_body_with_control_characters_is_reported_with_its_group() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();
        let id = "ab".repeat(32);

        let payload = b"ping\x07".to_vec();
        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupMessageReceived {
                        group_id: id.clone(),
                        name: "Team".to_string(),
                        peer: "Mallory".to_string(),
                        peer_id: format!("{id}#2"),
                        payload: payload.clone(),
                        kind: MessageKind::Text,
                        content_type: ContentType::Text,
                    },
                    &sender,
                )
                .await
        );

        match next_event(&mut receiver).await {
            CoreEvent::MessageUndecodable {
                peer,
                wire_bytes,
                reason,
                group_id,
                group,
                ..
            } => {
                assert_eq!(peer, "Mallory");
                assert_eq!(wire_bytes, payload.len() as u64);
                assert_eq!(reason, "contains control characters");
                assert_eq!(group_id.as_deref(), Some(id.as_str()));
                assert_eq!(group.as_deref(), Some("Team"));
            }
            other => panic!("a refused group body must be reported: {other:?}"),
        }
        assert_eq!(service.state.statistics.messages_received, 1);
    }

    /// Delivery acknowledgements are forwarded verbatim.
    #[tokio::test]
    async fn test_delivery_event_is_forwarded() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        service
            .on_app_event(
                AppEvent::MessageDelivered {
                    peer: "Alice".to_string(),
                    peer_id: "127.0.0.1:3".to_string(),
                    message_id: 42,
                },
                &sender,
            )
            .await;

        match next_event(&mut receiver).await {
            CoreEvent::MessageDelivered {
                peer, message_id, ..
            } => {
                assert_eq!(peer, "Alice");
                assert_eq!(message_id, 42);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Transport connection events are translated with their nickname.
    #[tokio::test]
    async fn test_network_events_are_translated() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        service
            .on_app_event(
                AppEvent::NetworkEvent(crate::types::NetworkEvent::PeerConnected {
                    peer_id: "127.0.0.1:4".to_string(),
                    metadata: HashMap::from([("nickname".to_string(), "Carol".to_string())]),
                }),
                &sender,
            )
            .await;
        match next_event(&mut receiver).await {
            CoreEvent::PeerConnected { nickname, .. } => assert_eq!(nickname, "Carol"),
            other => panic!("unexpected event: {other:?}"),
        }

        service
            .on_app_event(
                AppEvent::NetworkEvent(crate::types::NetworkEvent::PeerDisconnected {
                    peer_id: "127.0.0.1:4".to_string(),
                    reason: "bye".to_string(),
                }),
                &sender,
            )
            .await;
        match next_event(&mut receiver).await {
            CoreEvent::PeerDisconnected { reason, .. } => assert_eq!(reason, "bye"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// A friend status change updates only the matching contact.
    #[tokio::test]
    async fn test_friend_status_updates_the_right_contact() {
        let (mut service, _dir) = service().await;
        let (sender, _receiver) = bus();

        let first = Contact::new("First".to_string(), b"one".to_vec());
        let second = Contact::new("Second".to_string(), b"two".to_vec());
        let second_id = second.id;
        service.contacts.push(first);
        service.contacts.push(second);

        service
            .on_app_event(
                AppEvent::FriendStatusChanged {
                    friend_id: second_id,
                    is_online: true,
                },
                &sender,
            )
            .await;

        assert_eq!(service.contacts[0].status, UserStatus::Offline);
        assert_eq!(service.contacts[1].status, UserStatus::Online);
        assert!(service.contacts[1].last_seen.is_some());
    }

    /// A transport shutdown request stops the actor loop.
    #[tokio::test]
    async fn test_shutdown_event_stops_the_loop() {
        let (mut service, _dir) = service().await;
        let (sender, _receiver) = bus();
        assert!(service.on_app_event(AppEvent::Shutdown, &sender).await);
    }

    /// Unrelated transport events are ignored without stopping the loop.
    #[tokio::test]
    async fn test_unrelated_events_are_ignored() {
        let (mut service, _dir) = service().await;
        let (sender, _receiver) = bus();

        assert!(
            !service
                .on_app_event(AppEvent::UserInput("ls".to_string()), &sender)
                .await
        );
        // A group event for an unknown group is folded into state and published,
        // never treated as a shutdown request.
        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupChanged {
                        group_id: "ab".repeat(32),
                        name: "Team".to_string(),
                        members: 2,
                        joined: true,
                    },
                    &sender,
                )
                .await
        );
    }

    /// A configured default that fails validation is replaced, not trusted.
    #[tokio::test]
    async fn test_invalid_configured_defaults_are_not_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.app.default_nickname = "n".repeat(super::super::protocol::MAX_NICKNAME_LEN + 1);
        config.app.default_status = "s".repeat(super::super::protocol::MAX_STATUS_LEN + 1);
        config.database.connection_string =
            dir.path().join("core.db").to_string_lossy().into_owned();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            port: Some(0),
            ..CliArgs::default()
        };

        let service = CoreService::new(config, args).await.expect("service");
        assert!(
            validation::nickname(&service.nickname).is_ok(),
            "a configured nickname must always satisfy the contract"
        );
        assert!(
            validation::status(&service.status_message).is_ok(),
            "a configured status must always satisfy the contract"
        );
        assert_eq!(service.nickname, FALLBACK_NICKNAME);
    }

    /// The configured friend limit is enforced by every path that creates a
    /// contact, not only by `/add`.
    ///
    /// `max_friends` is 0 here — a value `AppConfig::problems()` refuses at startup,
    /// which is what makes it useful in a test: the guard is the first thing that
    /// can fire, so neither a dialable peer nor a real pending request is needed to
    /// reach it. Accepting is the path that used to skip the check.
    #[tokio::test]
    async fn test_the_friend_limit_is_enforced_when_accepting_a_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.app.max_friends = 0;
        config.database.connection_string =
            dir.path().join("core.db").to_string_lossy().into_owned();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            port: Some(0),
            ..CliArgs::default()
        };
        let mut service = CoreService::new(config, args).await.expect("service");

        let key = "AB".repeat(32);
        for (outcome, operation) in [
            (
                service.add_contact("127.0.0.1:1".to_string(), None).await,
                "add",
            ),
            (service.accept_request(&key).await, "accept"),
        ] {
            let crate::ipc::protocol::ResponseResult::Err { error } = outcome else {
                panic!("a full friend list must refuse the {operation}");
            };
            assert_eq!(
                error.code,
                ErrorCode::Backpressure,
                "{operation}: {error:?}"
            );
            assert!(
                error.message.contains("contact limit reached"),
                "{operation}: {error:?}"
            );
        }
    }

    /// A group membership event is folded into state and republished.
    #[tokio::test]
    async fn test_group_change_is_folded_into_state() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();
        let id = "ab".repeat(32);

        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupChanged {
                        group_id: id.clone(),
                        name: "Team".to_string(),
                        members: 2,
                        joined: true,
                    },
                    &sender,
                )
                .await
        );

        assert_eq!(service.groups.len(), 1);
        assert_eq!(service.groups[0].id, id);
        assert_eq!(service.groups[0].name, "Team");
        assert_eq!(service.groups[0].members, 2);
        assert!(service.groups[0].joined);

        match next_event(&mut receiver).await {
            CoreEvent::GroupChanged {
                group_id,
                group,
                members,
                joined,
            } => {
                assert_eq!(group_id, id);
                assert_eq!(group, "Team");
                assert_eq!(members, 2);
                assert!(joined);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // A later event without a name (a peer-list change that arrived before the
        // title was known) must not erase the name the group already has.
        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupChanged {
                        group_id: id.clone(),
                        name: String::new(),
                        members: 3,
                        joined: true,
                    },
                    &sender,
                )
                .await
        );
        assert_eq!(service.groups.len(), 1, "the group must not be duplicated");
        assert_eq!(service.groups[0].name, "Team", "the name must be kept");
        assert_eq!(service.groups[0].members, 3);
    }

    /// A group selector resolves by index, id and name, and defaults sensibly.
    #[tokio::test]
    async fn test_group_selector_resolution() {
        let (mut service, _dir) = service().await;
        let (sender, _receiver) = bus();
        let first = "aa".repeat(32);
        let second = "bb".repeat(32);

        for (id, name) in [(&first, "Team"), (&second, "Other")] {
            service
                .on_app_event(
                    AppEvent::GroupChanged {
                        group_id: id.clone(),
                        name: name.to_string(),
                        members: 1,
                        joined: true,
                    },
                    &sender,
                )
                .await;
        }

        // By index (1-based, like `/chat 1`), by id and by name.
        assert_eq!(service.resolve_group("1").expect("index").id, first);
        assert_eq!(service.resolve_group("2").expect("index").id, second);
        assert_eq!(service.resolve_group(&first).expect("id").name, "Team");
        assert_eq!(service.resolve_group("team").expect("name").id, first);
        assert_eq!(service.resolve_group("OTHER").expect("name").id, second);

        // An empty selector with two candidates is ambiguous, so it matches none.
        assert!(service.resolve_group("").is_none());

        // With one group, "the group" is that one.
        service.groups.retain(|group| group.id == second);
        assert_eq!(service.resolve_group("").expect("only group").id, second);

        // An index or name that matches nothing resolves to nothing.
        assert!(service.resolve_group("9").is_none());
        assert!(service.resolve_group("nope").is_none());
    }

    /// An incoming group message is published, counted and stored as a group row.
    #[tokio::test]
    async fn test_group_message_is_published_counted_and_persisted() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();
        let id = "cd".repeat(32);

        // The store has to be live for the history assertion below, so the service
        // is started (a `CoreService` does not open its pool until it is). Both
        // the start and the read-back only mean something with a real driver, so
        // they are behind `sqlite`; the event, counters and state above are
        // checked in every build.
        #[cfg(feature = "sqlite")]
        service.start().await.expect("start");

        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupMessageReceived {
                        group_id: id.clone(),
                        name: "Team".to_string(),
                        peer: "Alice".to_string(),
                        peer_id: format!("{id}#2"),
                        payload: b"hello group".to_vec(),
                        kind: MessageKind::Action,
                        content_type: ContentType::Text,
                    },
                    &sender,
                )
                .await
        );

        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(
            service.state.statistics.bytes_received,
            "hello group".len() as u64
        );
        // The message also tells the core that the group exists.
        assert_eq!(service.groups.len(), 1);

        match next_event(&mut receiver).await {
            CoreEvent::GroupMessageReceived {
                group_id,
                group,
                peer,
                peer_id,
                body,
                kind,
                content_type,
                ..
            } => {
                assert_eq!(group_id, id);
                assert_eq!(group, "Team");
                assert_eq!(peer, "Alice");
                assert_eq!(peer_id, format!("{id}#2"));
                assert_eq!(body, "hello group");
                assert_eq!(kind, MessageKind::Action);
                assert_eq!(content_type, ContentType::Text);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // The history row is a group row, keyed by the group and named after it.
        #[cfg(feature = "sqlite")]
        {
            let history = service.database.recent_messages(10).await.expect("history");
            assert_eq!(history.len(), 1);
            assert_eq!(
                history[0].conversation,
                crate::database::ConversationKind::Group
            );
            assert_eq!(history[0].group_id, id);
            assert_eq!(history[0].peer, "Team");
            assert_eq!(history[0].body, "hello group");
            assert_eq!(history[0].kind, MessageKind::Action);
        }
    }

    /// A binary group payload is rendered as hexadecimal, never as undecodable.
    #[tokio::test]
    async fn test_binary_group_message_is_not_undecodable() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();

        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupMessageReceived {
                        group_id: "ef".repeat(32),
                        name: String::new(),
                        peer: "Bob".to_string(),
                        peer_id: "ef".repeat(32) + "#1",
                        payload: vec![0x00, 0xff, 0x10],
                        kind: MessageKind::Text,
                        content_type: ContentType::Binary,
                    },
                    &sender,
                )
                .await
        );

        match next_event(&mut receiver).await {
            CoreEvent::GroupMessageReceived {
                body,
                content_type,
                group,
                ..
            } => {
                assert_eq!(body, "00ff10");
                assert_eq!(content_type, ContentType::Binary);
                // An unnamed group is reported as such rather than with a blank
                // label that a front-end would have to invent a name for.
                assert!(group.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(service.state.statistics.messages_received, 1);
    }

    /// A text group body that is not UTF-8 is reported without being mangled.
    ///
    /// It used to be dropped with a `warn!` and nothing else: the sender was counted and
    /// the user was told nothing at all, so a message could vanish from a conversation
    /// without a trace. The direct path has always published `MessageUndecodable`; the
    /// group path now does too, and names the group, because "something arrived that I
    /// cannot show" is only actionable if the user knows where.
    #[tokio::test]
    async fn test_non_utf8_group_text_is_not_published() {
        let (mut service, _dir) = service().await;
        let (sender, mut receiver) = bus();
        let id = "12".repeat(32);

        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupMessageReceived {
                        group_id: id.clone(),
                        name: "Team".to_string(),
                        peer: "Mallory".to_string(),
                        peer_id: format!("{id}#3"),
                        payload: vec![0xff, 0xfe],
                        kind: MessageKind::Text,
                        content_type: ContentType::Text,
                    },
                    &sender,
                )
                .await
        );

        // The body is not published — showing it at all would mean replacing it with
        // U+FFFD, which is what this project refuses to do — but the *fact* is.
        match next_event(&mut receiver).await {
            CoreEvent::MessageUndecodable {
                peer,
                wire_bytes,
                reason,
                group_id,
                group,
                ..
            } => {
                assert_eq!(peer, "Mallory");
                assert_eq!(wire_bytes, 2);
                assert_eq!(reason, "is not valid UTF-8 text");
                assert_eq!(group_id.as_deref(), Some(id.as_str()));
                assert_eq!(group.as_deref(), Some("Team"));
            }
            other => panic!("an undecodable group body must be reported: {other:?}"),
        }
        // The traffic is still counted, so the loss is visible in `/stats`.
        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(service.state.statistics.bytes_received, 2);
    }

    /// `/whoami` reports the transport's groups, not the session's memory of them.
    ///
    /// The snapshot reads a cache that `/group` fills, and a Tox instance that rejoined a
    /// conference after a restart is not in it until something asks — so the count could
    /// describe an earlier moment of the session rather than its state. The transport is
    /// asked first, the same rule `list_groups` follows. TCP is the transport a test can
    /// check without toxcore, and it is the case where the two sources disagree: it has no
    /// groups, so a cache that said otherwise must not survive the request.
    #[tokio::test]
    async fn test_session_info_answers_with_the_transports_groups() {
        let (mut service, _dir) = service().await;
        let (sender, _receiver) = bus();

        // A group the session was told about but the transport does not have.
        service.remember_group(&"cd".repeat(32), "Team", 2, true);
        assert_eq!(
            service.groups.len(),
            1,
            "the cache is warm before the request"
        );

        let crate::ipc::protocol::ResponseResult::Ok { reply } =
            service.handle(Request::SessionInfo, &sender).await
        else {
            panic!("session info must succeed");
        };
        let Reply::Session { session } = reply else {
            panic!("session info must answer with the snapshot");
        };

        assert!(!session.supports_groups, "TCP has no groups");
        assert_eq!(
            session.groups, 0,
            "a transport without groups has none to report"
        );
        assert!(
            service.groups.is_empty(),
            "the cache follows the transport: {:?}",
            service.groups
        );
    }

    /// An argument-less `/history` is answered with the session's `--history-limit`.
    ///
    /// The flag had no reader at all: it was parsed, defaulted and validated, and then
    /// dropped, so `/history` always showed the protocol default however the session was
    /// started. The configured page is now what a request that names no limit is answered
    /// with, and an explicit limit still wins over it.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_history_without_a_limit_uses_the_configured_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.database.connection_string =
            dir.path().join("core.db").to_string_lossy().into_owned();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            port: Some(0),
            // A page that cannot be confused with the protocol default.
            history_limit: 2,
            ..CliArgs::default()
        };

        let mut service = CoreService::new(config, args).await.expect("service");
        service.start().await.expect("start");

        for body in ["first", "second", "third"] {
            service
                .database
                .save_message(&StoredMessage::new(
                    MessageDirection::Outgoing,
                    "Alice",
                    body,
                    u64::try_from(body.len()).expect("length"),
                    MessageKind::Text,
                    ContentType::Text,
                ))
                .await
                .expect("save");
        }

        let (sender, _receiver) = bus();

        let crate::ipc::protocol::ResponseResult::Ok { reply } = service
            .handle(Request::History { limit: None }, &sender)
            .await
        else {
            panic!("history must succeed");
        };
        let Reply::History { messages, .. } = reply else {
            panic!("history must answer with the records");
        };
        assert_eq!(messages.len(), 2, "the configured page is two");
        assert_eq!(messages[0].body, "second");
        assert_eq!(messages[1].body, "third", "the newest record comes last");

        let crate::ipc::protocol::ResponseResult::Ok { reply } = service
            .handle(Request::History { limit: Some(1) }, &sender)
            .await
        else {
            panic!("history must succeed");
        };
        let Reply::History { messages, .. } = reply else {
            panic!("history must answer with the records");
        };
        assert_eq!(messages.len(), 1, "an explicit limit wins over the flag");
        assert_eq!(messages[0].body, "third");
    }
}
