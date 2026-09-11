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
use crate::network::{NetworkManager, SendOutcome};
use crate::types::{AppEvent, AppState, Contact, UserStatus};

use super::protocol::{
    ContactView, CoreEvent, ErrorCode, ErrorInfo, MessageView, Reply, Request, SendOutcomeKind,
    SendReport, SessionInfo, StatisticsView,
};
use super::validation;

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
            .send(Command { request, reply })
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
    fn from_command(command: Option<Command>) -> Self {
        Self::Command(command)
    }

    /// Classify a transport-channel result.
    fn from_event(event: Option<AppEvent>) -> Self {
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

    /// Peer-to-peer transport.
    network: NetworkManager,

    /// Persistence backend.
    database: DatabaseManager,

    /// Nickname announced to peers.
    nickname: String,

    /// Personal status message.
    status_message: String,

    /// Per-session display identity (never key material).
    identity: String,

    /// Friend list.
    contacts: Vec<Contact>,

    /// Identifier of the active conversation.
    active_chat: Option<uuid::Uuid>,

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
        let network =
            NetworkManager::new(&config.network, Arc::clone(&crypto), event_sender.clone())
                .await
                .context("Failed to initialise the network manager")?;

        let data_dir = args.data_directory().unwrap_or_else(|| PathBuf::from("."));
        let session_path = data_dir.join(SESSION_STATE_FILE);

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
            identity: hex::encode_upper(CryptoManager::generate_key()),
            state: AppState::new(),
            crypto,
            network,
            database,
            contacts: Vec::new(),
            active_chat: None,
            started_at: chrono::Utc::now(),
            last_auto_save: tokio::time::Instant::now(),
            inbox: Some(event_receiver),
            session_path,
            options: CoreServiceOptions::default(),
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
            if !config.network.bootstrap_nodes.contains(node) {
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
        self.network
            .start()
            .await
            .context("Failed to start the network listener")?;

        self.network.set_nickname(&self.nickname).await;

        let mut wanted = self.args.peers.clone();
        if let Some(node) = &self.args.bootstrap_node {
            wanted.push(node.clone());
        }
        if !wanted.is_empty() {
            let connected = self.network.connect_all(&wanted).await;
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
                        received = receiver.recv() => ActorStep::from_event(received),
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
                    let stop = matches!(command.request, Request::Shutdown);
                    let result = self.handle(command.request, &events).await;
                    // A closed reply channel only means the caller gave up.
                    let _ = command.reply.send(result);
                    if stop {
                        break;
                    }
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

        self.finish(events).await;
    }

    /// Terminal cleanup: persist the session and close every subsystem.
    async fn finish(&mut self, events: broadcast::Sender<CoreEvent>) {
        if let Err(error) = self.save_session().await {
            warn!("⚠️ Failed to persist the session during shutdown: {error}");
        }
        if let Err(error) = self.network.shutdown().await {
            warn!("⚠️ Failed to stop the network cleanly: {error}");
        }
        if let Err(error) = self.database.shutdown().await {
            warn!("⚠️ Failed to stop the database cleanly: {error}");
        }

        let _ = events.send(CoreEvent::Shutdown {
            reason: "the core service stopped".to_string(),
        });
        info!("👋 Core service stopped");
    }

    /// Periodic maintenance: reconcile counters and auto-save the session.
    async fn on_tick(&mut self) {
        self.state.statistics.active_connections =
            u32::try_from(self.network.connected_peers()).unwrap_or(u32::MAX);

        let interval = self.config.app.auto_save_interval;
        if interval > 0 && self.last_auto_save.elapsed().as_secs() >= interval {
            if let Err(error) = self.save_session().await {
                warn!("⚠️ Automatic session save failed: {error}");
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
            Request::SessionInfo => ok(Reply::Session {
                session: self.session_info(),
            }),
            Request::Statistics => ok(Reply::Statistics {
                statistics: self.statistics_view(),
            }),
            Request::ListContacts => ok(Reply::Contacts {
                contacts: self.contacts.iter().map(contact_view).collect(),
            }),
            Request::AddContact { identifier, note } => self.add_contact(identifier, note).await,
            Request::RemoveContact { target } => self.remove_contact(&target).await,
            Request::SetNickname { nickname } => self.set_nickname(nickname, events).await,
            Request::SetStatus { text } => self.set_status(text, events),
            Request::SelectConversation { target } => self.select_conversation(&target),
            Request::Connect { address } => self.connect(&address).await,
            Request::SendMessage { target, text } => self.send_message(target, &text).await,
            Request::History { limit } => self.history(limit).await,
            Request::SaveSession => match self.save_session().await {
                Ok(()) => ok(Reply::Saved {
                    path: self.session_path.display().to_string(),
                }),
                Err(error) => err(ErrorInfo::critical(ErrorCode::Internal, format!("{error}"))),
            },
            Request::Shutdown => ok(Reply::ShuttingDown { acknowledged: true }),
        }
    }

    /// Fold a transport event into state and republish it as a [`CoreEvent`].
    ///
    /// # Returns
    ///
    /// Returns `true` when the service must stop.
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
            } => {
                let wire_bytes = payload.len() as u64;

                // Payloads are UTF-8 text by contract. A frame that does not
                // decode is reported as such instead of being mangled into
                // replacement characters, which would show the user something
                // that was never sent.
                let Ok(body) = String::from_utf8(payload) else {
                    warn!(
                        "⚠️ A frame from {peer_id} was not valid UTF-8; \
                         reporting it as undecodable"
                    );
                    self.state.statistics.messages_received += 1;
                    self.state.statistics.bytes_received += wire_bytes;
                    publish(
                        events,
                        CoreEvent::MessageUndecodable {
                            peer,
                            peer_id,
                            wire_bytes,
                        },
                    );
                    return false;
                };

                self.state.statistics.messages_received += 1;
                self.state.statistics.bytes_received += wire_bytes;

                if self.database.is_persistent() {
                    let record = StoredMessage::new(
                        MessageDirection::Incoming,
                        peer.clone(),
                        body.clone(),
                        wire_bytes,
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
            AppEvent::NetworkEvent(network_event) => {
                self.on_network_event(network_event, events);
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
            // The backend has no user interface and no group chat, so group
            // events and user input are not its business.
            AppEvent::GroupEvent { .. } | AppEvent::UserInput(_) => false,
            AppEvent::Shutdown => {
                debug!("🛑 Transport requested a global shutdown");
                true
            }
        }
    }

    /// Translate a transport-level event into the public event stream.
    fn on_network_event(
        &self,
        event: crate::types::NetworkEvent,
        events: &broadcast::Sender<CoreEvent>,
    ) {
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

        let max_friends = self.config.app.max_friends;
        if self.contacts.len() >= max_friends {
            return err(ErrorInfo::new(
                ErrorCode::Backpressure,
                format!("contact limit reached ({max_friends}); cannot add more friends"),
            ));
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
        self.network.set_nickname(&self.nickname).await;
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
        text: String,
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

    /// Dial a peer address and keep it connected.
    async fn connect(&self, address: &str) -> super::protocol::ResponseResult {
        let address = address.trim();
        if address.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a host:port address is required",
            ));
        }
        if !CliArgs::is_valid_peer_address(address) {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                format!("'{address}' is not a valid host:port address"),
            ));
        }

        match self.network.connect(address).await {
            Ok(remote) => ok(Reply::Updated {
                subject: "connection".to_string(),
                detail: remote.to_string(),
            }),
            Err(error) => err(ErrorInfo::new(
                ErrorCode::Network,
                format!("could not connect to {address}: {error}"),
            )),
        }
    }

    /// Encrypt and route a chat message.
    async fn send_message(
        &mut self,
        target: Option<String>,
        text: &str,
    ) -> super::protocol::ResponseResult {
        let text = text.trim();
        if text.is_empty() {
            return err(ErrorInfo::new(
                ErrorCode::InvalidRequest,
                "a message body is required",
            ));
        }

        // Enforce the configured length limit and the character rules before
        // doing any crypto work, so a rejected body has no side effects.
        let max_length = self.config.app.max_message_length;
        let text = match validation::message(text, max_length) {
            Ok(text) => text,
            Err(error) => return err(error),
        };
        let text = text.as_str();

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

        // Encrypt once so the reported wire size reflects the real ciphertext.
        let ciphertext = match self.crypto.encrypt(text.as_bytes()) {
            Ok(ciphertext) => ciphertext,
            Err(error) => return err(ErrorInfo::from(error)),
        };

        let outcome = match self
            .network
            .send_ciphertext_to_peer(&destination, &ciphertext)
        {
            Ok(outcome) => outcome,
            Err(error) => return err(ErrorInfo::from(error)),
        };

        self.state.statistics.messages_sent += 1;
        self.state.statistics.bytes_sent += ciphertext.len() as u64;

        // Mirror the message into the durable store when one is available.
        let mut persisted = false;
        if self.database.is_persistent() {
            let record = StoredMessage::new(
                MessageDirection::Outgoing,
                destination.clone(),
                text,
                ciphertext.len() as u64,
            );
            match self.database.save_message(&record).await {
                Ok(()) => persisted = true,
                Err(error) => warn!("⚠️ Failed to persist an outgoing message: {error}"),
            }
        }

        ok(Reply::Sent {
            target: destination,
            report: send_report(
                outcome,
                ciphertext.len(),
                self.crypto.is_enabled(),
                persisted,
            ),
        })
    }

    /// The most recent persisted messages.
    async fn history(&self, limit: Option<usize>) -> super::protocol::ResponseResult {
        let persistent = self.database.is_persistent();
        if !persistent {
            return ok(Reply::History {
                persistent,
                messages: Vec::new(),
            });
        }

        let limit = limit.unwrap_or(20).clamp(1, 1000);
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
            identity: self.identity.clone(),
            mode: self.args.effective_mode().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Always the configured algorithm; `encryption_enabled` tells the
            // front-end whether it is actually in force.
            encryption: self.crypto.algorithm().to_string(),
            encryption_enabled: self.crypto.is_enabled(),
            network_running: self.network.is_started(),
            network_port: self.network.port(),
            network_bootstrap_nodes: self.network.bootstrap_nodes().len(),
            connected_peers: self.network.connected_peers(),
            pending_peers: self.network.pending_peers(),
            max_connections: self.network.max_connections(),
            queued_messages: self.network.queued_messages(),
            desired_peers: self.network.desired_peers(),
            peer_nicknames: self.network.peer_nicknames(),
            local_address: self.network.local_addr().map(|addr| addr.to_string()),
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
        tokio::fs::write(&self.session_path, json)
            .await
            .with_context(|| format!("Failed to write {}", self.session_path.display()))?;

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
fn ok(reply: Reply) -> super::protocol::ResponseResult {
    super::protocol::ResponseResult::ok(reply)
}

/// Broadcast an event; a lack of subscribers is not an error.
fn publish(events: &broadcast::Sender<CoreEvent>, event: CoreEvent) {
    // `send` only fails when nobody is subscribed, which is a normal state
    // (for example a headless deployment).
    let _ = events.send(event);
}

/// Wrap an error in a failed [`ResponseResult`].
fn err(error: ErrorInfo) -> super::protocol::ResponseResult {
    super::protocol::ResponseResult::err(error)
}

/// Flatten a [`SendOutcome`] into a wire report.
fn send_report(
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
                    payload: "hello world".as_bytes().to_vec(),
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
                },
                &sender,
            )
            .await;
        assert!(!stop);

        match next_event(&mut receiver).await {
            CoreEvent::MessageUndecodable {
                peer, wire_bytes, ..
            } => {
                assert_eq!(peer, "Mallory");
                assert_eq!(wire_bytes, payload.len() as u64);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // The attempt is still accounted for, but nothing was stored as text.
        assert_eq!(service.state.statistics.messages_received, 1);
        assert_eq!(service.state.statistics.bytes_received, 3);
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
        assert!(
            !service
                .on_app_event(
                    AppEvent::GroupEvent {
                        group_id: uuid::Uuid::new_v4(),
                        event_type: crate::types::GroupEventType::TitleChanged("t".to_string()),
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
}
