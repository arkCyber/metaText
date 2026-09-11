/*!
 * app.rs
 *
 * Core application logic for metaText instant messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Owns and starts every subsystem (crypto, database, network, TUI)
 * - Interactive REPL that reads commands from stdin and from the event channel
 * - Slash command handling (`/help`, `/info`, `/add`, `/chat`, ...)
 * - Periodic maintenance with timed session auto-save
 * - Graceful shutdown of all subsystems
 */

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, instrument, warn};

use crate::cli::CliArgs;
use crate::commands::{self, Command};
use crate::config::AppConfig;
use crate::crypto::CryptoManager;
use crate::database::{DatabaseManager, MessageDirection, StoredMessage};
use crate::network::{NetworkManager, SendOutcome};
use crate::tui::{SharedTuiInfo, TuiInfo, TuiManager};
use crate::types::{AppEvent, AppState, Contact, ShutdownSignal};

/// File name used to persist the session (nickname, contacts, statistics)
const SESSION_STATE_FILE: &str = "metatext-session.json";

/// Serializable snapshot of everything that survives a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionState {
    /// Local user nickname
    nickname: String,

    /// Local user status message
    status_message: String,

    /// Contact list
    contacts: Vec<Contact>,

    /// Runtime statistics carried over from the previous session
    statistics: crate::types::AppStatistics,
}

/// Main application struct that coordinates all subsystems
#[derive(Debug)]
pub struct MetaTextApp {
    /// Application configuration
    config: Arc<RwLock<AppConfig>>,

    /// Command line arguments
    args: CliArgs,

    /// Current application state
    state: Arc<RwLock<AppState>>,

    /// Cryptographic manager shared with the network layer
    crypto: Arc<CryptoManager>,

    /// Network manager for P2P communication
    network: NetworkManager,

    /// Database manager for persistence
    database: DatabaseManager,

    /// Terminal user interface manager
    tui: TuiManager,

    /// Shared snapshot of the information rendered by the full screen TUI
    tui_info: SharedTuiInfo,

    /// Local user nickname
    nickname: String,

    /// Local user status message
    status_message: String,

    /// Per-session public identity shown to the user
    ///
    /// This is deliberately *not* the encryption key: with a shared passphrase
    /// the symmetric key is identical on every peer, and printing it would
    /// leak key material. The identity is a random per-process value used only
    /// for display.
    identity: String,

    /// Contact list (friends)
    contacts: Vec<Contact>,

    /// Event channel for application-wide communication
    event_sender: mpsc::UnboundedSender<AppEvent>,
    event_receiver: Option<mpsc::UnboundedReceiver<AppEvent>>,

    /// Shutdown signal channel
    shutdown_sender: Option<mpsc::Sender<ShutdownSignal>>,

    /// Timestamp of the last automatic session save
    last_auto_save: tokio::time::Instant,
}

impl MetaTextApp {
    /// Create a new `metaText` application instance
    ///
    /// Applies command line overrides on top of the loaded configuration and
    /// constructs every subsystem. The subsystems are only *started* later by
    /// [`MetaTextApp::run`] so that construction stays side-effect free.
    ///
    /// # Arguments
    ///
    /// * `config` - Application configuration loaded from disk
    /// * `args` - Parsed command line arguments
    ///
    /// # Returns
    ///
    /// Returns a fully initialised [`MetaTextApp`].
    ///
    /// # Errors
    ///
    /// Returns an error when any subsystem fails to initialise.
    #[instrument(skip(config, args))]
    pub async fn new(mut config: AppConfig, args: CliArgs) -> Result<Self> {
        info!(
            "🏗️ [{}] Initializing MetaText application components",
            now()
        );

        // Apply command line overrides so the CLI always wins over the file.
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
        // `--data-dir` (or the platform default) also decides where the
        // database lives, instead of silently writing into the current working
        // directory. Absolute paths and in-memory databases are left untouched.
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

        // Create event communication channel
        let (event_sender, event_receiver) = mpsc::unbounded_channel::<AppEvent>();
        debug!("📡 [{}] Event communication channel established", now());

        // Initialize application state
        let state = Arc::new(RwLock::new(AppState::new()));

        // Build the subsystems. Crypto needs to be shared with the network
        // layer because it encrypts every wire message.
        //
        // A shared passphrase makes all peers derive the same key, which is
        // what allows them to decrypt each other; without one every process
        // uses a fresh random key and messages stay local.
        let crypto = Arc::new(match &args.passphrase {
            Some(passphrase) => CryptoManager::from_passphrase(&config.crypto, passphrase)
                .context("Failed to derive the encryption key from the passphrase")?,
            None => CryptoManager::new(&config.crypto)
                .await
                .context("Failed to initialize crypto manager")?,
        });
        let database = DatabaseManager::new(&config.database)
            .await
            .context("Failed to initialize database manager")?;
        let network =
            NetworkManager::new(&config.network, Arc::clone(&crypto), event_sender.clone())
                .await
                .context("Failed to initialize network manager")?;
        let tui = TuiManager::new(&config.app.name, event_sender.clone())
            .await
            .context("Failed to initialize TUI manager")?;
        let tui_info = tui.info_handle();

        let nickname = config.app.default_nickname.clone();
        let status_message = config.app.default_status.clone();

        // A random per-session identity, shown instead of the encryption key.
        let identity = hex::encode_upper(CryptoManager::generate_key());

        let app = Self {
            config: Arc::new(RwLock::new(config)),
            args,
            state,
            crypto,
            network,
            database,
            tui,
            tui_info,
            nickname,
            status_message,
            identity,
            contacts: Vec::new(),
            event_sender,
            event_receiver: Some(event_receiver),
            shutdown_sender: None,
            last_auto_save: tokio::time::Instant::now(),
        };

        info!(
            "✅ [{}] MetaText application initialization completed",
            now()
        );
        Ok(app)
    }

    /// Get a reference to the shared cryptographic manager
    ///
    /// # Returns
    ///
    /// Returns the [`CryptoManager`] used for message encryption.
    #[must_use]
    pub const fn crypto(&self) -> &Arc<CryptoManager> {
        &self.crypto
    }

    /// Get a reference to the database manager
    ///
    /// # Returns
    ///
    /// Returns the [`DatabaseManager`] that owns contact and message
    /// persistence.
    #[must_use]
    pub const fn database(&self) -> &DatabaseManager {
        &self.database
    }

    /// Get a clone of the application event sender
    ///
    /// Subsystems and background tasks use this handle to push
    /// [`AppEvent`] values into the coordinator's event loop.
    ///
    /// # Returns
    ///
    /// Returns a cloned handle to the unbounded application event sender.
    #[must_use]
    pub fn event_sender(&self) -> mpsc::UnboundedSender<AppEvent> {
        self.event_sender.clone()
    }

    /// Get the local nickname currently in use
    ///
    /// # Returns
    ///
    /// Returns the nickname shown to other peers.
    #[must_use]
    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    /// Get the current contact list
    ///
    /// # Returns
    ///
    /// Returns a slice with all known contacts.
    #[must_use]
    pub fn contacts(&self) -> &[Contact] {
        &self.contacts
    }

    /// Request a graceful shutdown from any task holding a handle to the app
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown channel is not initialised or closed.
    pub async fn request_shutdown(&self) -> Result<()> {
        if let Some(sender) = &self.shutdown_sender {
            sender
                .send(ShutdownSignal::UserRequested)
                .await
                .context("Failed to send shutdown signal")?;
        }
        Ok(())
    }
}

impl MetaTextApp {
    /// Start the application and run the main event loop
    ///
    /// Starts every subsystem, optionally spawns the interactive stdin reader,
    /// then dispatches events until the user quits, presses Ctrl+C, or a
    /// shutdown signal is received.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` after a successful graceful shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if a subsystem fails to start or shutdown fails.
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> Result<()> {
        info!("🚀 [{}] Starting MetaText application", now());

        // Set up shutdown channel
        let (shutdown_sender, mut shutdown_receiver) = mpsc::channel::<ShutdownSignal>(1);
        self.shutdown_sender = Some(shutdown_sender);

        // Take ownership of the event receiver for the loop below.
        let mut event_receiver = self
            .event_receiver
            .take()
            .context("Application event receiver was already consumed")?;

        // Restore the previous session (if any) before starting subsystems.
        self.load_session_state().await;

        // Start the subsystems in dependency order.
        self.database
            .start()
            .await
            .context("Failed to start database manager")?;
        self.network
            .start()
            .await
            .context("Failed to start network manager")?;

        // Announce our nickname and dial any peers requested on the command
        // line. Failing to reach a peer is reported but never fatal.
        self.announce_and_connect().await;

        self.tui
            .start(
                self.is_interactive()
                    && matches!(self.args.effective_mode(), crate::cli::AppMode::Tui),
            )
            .await
            .context("Failed to start TUI manager")?;

        // Record the start time for statistics.
        {
            let mut state = self.state.write().await;
            state.statistics.start_time = Some(chrono::Utc::now());
        }

        let interactive = self.is_interactive();

        // Display startup banner (skipped in headless/server/daemon modes).
        if interactive {
            Self::display_startup_banner();
            self.display_info().await;
        } else {
            info!("🤖 [{}] Headless mode: stdin will not be read", now());
        }

        // Publish the initial status snapshot for the full screen interface.
        self.refresh_tui_info().await;

        // Forward lines typed on stdin into the application event channel.
        // When the full screen TUI owns the terminal it produces the events
        // itself, so the stdin reader must not compete for the input stream.
        if interactive && !self.tui.is_active() {
            self.spawn_stdin_reader();
            // Show the prompt right away so it is obvious the REPL is ready
            // and waiting for a command.
            crate::tui::write_prompt();
        }

        info!("🔄 [{}] Entering main event loop", now());

        // Listen for Ctrl+C so the user can stop the client gracefully.
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        // Service modes (`server`/`daemon`) are normally stopped with SIGTERM,
        // so on Unix it is treated as a graceful shutdown request. On other
        // platforms the future simply never resolves.
        #[cfg(unix)]
        let sigterm = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(error) => {
                    warn!("⚠️ [{}] Failed to install SIGTERM handler: {error}", now());
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(unix))]
        let sigterm = std::future::pending::<()>();
        tokio::pin!(sigterm);

        let mut should_stop = false;

        // Main event loop
        while !should_stop {
            tokio::select! {
                // Handle Ctrl+C (SIGINT)
                _ = &mut ctrl_c => {
                    info!("🛑 [{}] Received Ctrl+C, shutting down", now());
                    should_stop = true;
                }

                // Handle SIGTERM (graceful shutdown of service modes)
                () = &mut sigterm => {
                    info!("🛑 [{}] Received SIGTERM, shutting down", now());
                    should_stop = true;
                }

                // Handle shutdown signals
                Some(signal) = shutdown_receiver.recv() => {
                    info!("🛑 [{}] Received shutdown signal: {:?}", now(), signal);
                    should_stop = true;
                }

                // Handle periodic tasks
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                    if let Err(e) = self.handle_periodic_tasks().await {
                        warn!("⚠️ [{}] Error in periodic tasks: {:?}", now(), e);
                    }
                    self.refresh_tui_info().await;
                }

                // Handle application events
                Some(event) = event_receiver.recv() => {
                    match self.process_event(event).await {
                        Ok(stop) => should_stop = stop,
                        Err(e) => warn!("⚠️ [{}] Error while processing event: {:?}", now(), e),
                    }
                    self.refresh_tui_info().await;
                }
            }
        }

        // Perform graceful shutdown
        self.shutdown()
            .await
            .context("Failed to shutdown gracefully")?;

        info!("👋 [{}] MetaText application shutdown completed", now());
        Ok(())
    }

    /// Announce the local nickname and dial the peers requested on the CLI
    ///
    /// `--peer` addresses and an explicitly given `--bootstrap` node are
    /// registered as desired peers, so the transport keeps retrying them in the
    /// background until they come up, and reconnects them if they drop.
    async fn announce_and_connect(&self) {
        self.network.set_nickname(&self.nickname).await;

        let mut wanted = self.args.peers.clone();
        if let Some(node) = &self.args.bootstrap_node {
            wanted.push(node.clone());
        }

        if wanted.is_empty() {
            return;
        }

        // `connect_all` also registers each address as a desired peer.
        let connected = self.network.connect_all(&wanted).await;
        info!(
            "🔗 Established {connected} of {} requested peer connection(s)",
            wanted.len()
        );
    }

    /// Forward lines typed on stdin into the application event channel
    ///
    /// The task stops at EOF (Ctrl+D), which also requests a clean shutdown.
    fn spawn_stdin_reader(&self) {
        let sender = self.tui.event_sender();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if sender.send(AppEvent::UserInput(line)).is_err() {
                    break;
                }
            }
            let _ = sender.send(AppEvent::Shutdown);
        });
    }

    /// Whether the current mode should accept interactive input
    const fn is_interactive(&self) -> bool {
        use crate::cli::AppMode;
        if self.args.headless {
            return false;
        }
        matches!(self.args.effective_mode(), AppMode::Tui | AppMode::Cli)
    }

    /// Whether the plain line based REPL currently owns input and output
    ///
    /// This is true only for an interactive mode that is *not* covered by the
    /// full screen interface, i.e. exactly when [`crate::tui::write_prompt`]
    /// should be shown between commands.
    fn repl_is_active(&self) -> bool {
        self.is_interactive() && !self.tui.is_active()
    }

    /// Process a single application event
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` when the event requests application shutdown.
    async fn process_event(&mut self, event: AppEvent) -> Result<bool> {
        match event {
            AppEvent::Shutdown => {
                info!("🛑 [{}] Shutdown event received", now());
                Ok(true)
            }
            AppEvent::UserInput(line) => {
                let result = self.handle_user_input(&line).await;
                // Re-arm the prompt unless the command asked to quit, so the
                // REPL is visibly waiting for the next command again.
                if !matches!(&result, Ok(true)) {
                    crate::tui::write_prompt();
                }
                result
            }
            AppEvent::MessageReceived {
                peer,
                peer_id,
                payload,
            } => {
                // Count the incoming message and its wire size.
                {
                    let mut state = self.state.write().await;
                    state.statistics.messages_received += 1;
                    state.statistics.bytes_received += payload.len() as u64;
                }

                // Surface the decrypted text; the transport authenticated it
                // before the event was produced.
                let from = if peer.is_empty() { peer_id } else { peer };
                let text = String::from_utf8_lossy(&payload);
                Self::println(&format!("📥 {from}: {text}"));

                // Mirror the message into the durable store when one is
                // available.
                if self.database.is_persistent() {
                    let record = StoredMessage::new(
                        MessageDirection::Incoming,
                        from,
                        text.to_string(),
                        payload.len() as u64,
                    );
                    if let Err(error) = self.database.save_message(&record).await {
                        warn!("⚠️ [{}] Failed to persist incoming message: {error}", now());
                    }
                }

                // Output that arrives in the background must not leave the REPL
                // without a prompt.
                if self.repl_is_active() {
                    crate::tui::write_prompt();
                }

                Ok(false)
            }
            AppEvent::MessageDelivered {
                peer,
                peer_id,
                message_id,
            } => {
                let who = if peer.is_empty() { peer_id } else { peer };
                debug!("✅ [{}] Message {message_id} acknowledged by {who}", now());
                Self::println(&format!("✅ delivered to {who} (#{message_id})"));

                if self.repl_is_active() {
                    crate::tui::write_prompt();
                }

                Ok(false)
            }
            AppEvent::NetworkEvent(event) => {
                debug!("🌐 [{}] Network event: {:?}", now(), event);
                Ok(false)
            }
            AppEvent::FriendStatusChanged {
                friend_id,
                is_online,
            } => {
                if let Some(contact) = self.contacts.iter_mut().find(|c| c.id == friend_id) {
                    contact.status = if is_online {
                        crate::types::UserStatus::Online
                    } else {
                        crate::types::UserStatus::Offline
                    };
                    if is_online {
                        contact.update_last_seen();
                    }
                }
                debug!("👤 [{}] Friend {} online={}", now(), friend_id, is_online);
                Ok(false)
            }
            AppEvent::GroupEvent {
                group_id,
                event_type,
            } => {
                debug!("👥 [{}] Group {} event: {:?}", now(), group_id, event_type);
                Ok(false)
            }
        }
    }

    /// Handle a raw line typed by the user
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` when the line requests application shutdown.
    async fn handle_user_input(&mut self, line: &str) -> Result<bool> {
        match commands::parse(line) {
            // Ignore empty input so pressing Enter does not spam the output.
            Command::Message(text) if text.is_empty() => Ok(false),
            command => self.handle_command(command).await,
        }
    }

    /// Handle a parsed [`Command`]
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` when the command requests application shutdown.
    async fn handle_command(&mut self, command: Command) -> Result<bool> {
        // The commands added for the REPL overhaul are dispatched here so that
        // the table below stays within the clippy line budget.
        if self.handle_extended_command(&command).await? {
            return Ok(false);
        }

        match command {
            Command::Help(topic) => {
                match topic.as_deref() {
                    Some(name) => Self::display_command_help(name),
                    None => Self::display_help(),
                }
                Ok(false)
            }
            Command::Readme => {
                Self::display_readme();
                Ok(false)
            }
            Command::Info => {
                self.display_info().await;
                Ok(false)
            }
            Command::List => {
                self.display_contacts().await;
                Ok(false)
            }
            Command::Peers => {
                self.display_peers();
                Ok(false)
            }
            Command::Stats => {
                self.display_statistics().await;
                Ok(false)
            }
            Command::Clear => {
                // Clears the terminal in REPL mode and the scrollback in TUI mode.
                crate::tui::clear_output();
                Ok(false)
            }
            Command::Nick(name) => {
                if name.is_empty() {
                    // Bare `/nick` reports the current nickname.
                    Self::println(&format!("nickname: {}", self.nickname));
                } else {
                    Self::println(&format!("✅ nickname set to '{name}'"));
                    self.nickname.clone_from(&name);
                    // Let every connected peer know about the new name.
                    self.network.set_nickname(&self.nickname).await;
                }
                Ok(false)
            }
            Command::Status(text) => {
                if text.is_empty() {
                    // Bare `/status` reports the current status message.
                    Self::println(&format!("status: {}", self.status_message));
                } else {
                    Self::println(&format!("✅ status set to '{text}'"));
                    self.status_message.clone_from(&text);
                }
                Ok(false)
            }
            Command::Add { identifier, note } => {
                self.handle_add_contact(identifier, note).await;
                Ok(false)
            }
            Command::Chat(target) => {
                self.handle_chat_target(&target).await;
                Ok(false)
            }
            Command::Connect(target) => {
                self.handle_connect(&target).await;
                Ok(false)
            }
            Command::History(limit) => {
                self.display_history(limit).await;
                Ok(false)
            }
            Command::Message(text) => {
                self.handle_outgoing_message(text).await?;
                Ok(false)
            }
            Command::Quit => {
                info!("👋 [{}] User requested quit", now());
                Ok(true)
            }
            // Already handled by `handle_extended_command` above.
            Command::Whoami
            | Command::Version
            | Command::Uptime
            | Command::Save
            | Command::Msg { .. }
            | Command::Remove(_) => Ok(false),
            Command::Unknown(name) => {
                Self::println(&format!(
                    "❓ unknown command '/{name}'. Type /help for the command list."
                ));
                Ok(false)
            }
        }
    }

    /// Handle the commands introduced for the REPL overhaul
    ///
    /// Keeping them in a helper keeps [`MetaTextApp::handle_command`] focused on
    /// the original dispatch table.
    ///
    /// # Arguments
    ///
    /// * `command` - Parsed command to inspect.
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` when the command was handled here, and `Ok(false)` when
    /// the caller should dispatch it instead.
    ///
    /// # Errors
    ///
    /// Returns an error if sending a direct message fails.
    async fn handle_extended_command(&mut self, command: &Command) -> Result<bool> {
        match command {
            Command::Whoami => self.display_whoami(),
            Command::Version => self.display_version(),
            Command::Uptime => self.display_uptime().await,
            Command::Save => self.handle_save().await,
            Command::Msg { target, text } => self.handle_direct_message(target, text).await?,
            Command::Remove(target) => self.handle_remove_contact(target).await,
            // Not one of the commands handled by this helper.
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Add a contact to the local contact list
    ///
    /// The identifier is treated as the DID-like public identifier of the peer.
    /// Since metaText does not yet perform a real key exchange, the identifier
    /// bytes are stored as the placeholder public key.
    async fn handle_add_contact(&mut self, identifier: String, note: Option<String>) {
        if identifier.is_empty() {
            Self::println("Usage: /add <DID_Address> [note]");
            return;
        }

        let max_friends = self.config.read().await.app.max_friends;
        if self.contacts.len() >= max_friends {
            Self::println(&format!(
                "⚠️ contact limit reached ({max_friends}); cannot add more friends"
            ));
            return;
        }

        // The placeholder public key stores the identifier bytes, so decode it
        // back to a string for a case-insensitive duplicate check.
        let is_duplicate = self.contacts.iter().any(|c| {
            String::from_utf8_lossy(&c.public_key).eq_ignore_ascii_case(&identifier)
                || c.name.eq_ignore_ascii_case(&identifier)
        });
        if is_duplicate {
            Self::println(&format!("ℹ️ contact '{identifier}' already exists"));
            return;
        }

        let mut contact = Contact::new(identifier.clone(), identifier.as_bytes().to_vec());
        contact.note = note;

        // Mirror the new contact into the durable store when one is available.
        if self.database.is_persistent() {
            if let Err(error) = self.database.save_contact(&contact).await {
                warn!(
                    "⚠️ [{}] Failed to persist contact {identifier}: {error}",
                    now()
                );
            }
        }

        self.contacts.push(contact);

        Self::println(&format!(
            "✅ added friend #{} : {identifier}",
            self.contacts.len()
        ));
    }

    /// Resolve a `/chat` target (1-based index or name) and activate it
    ///
    /// When `target` is empty the current conversation is reported instead of
    /// being changed, so `/chat` doubles as a status query.
    async fn handle_chat_target(&self, target: &str) {
        if target.is_empty() {
            let active = self.state.read().await.active_conversation;
            let current = active.and_then(|id| {
                self.contacts
                    .iter()
                    .enumerate()
                    .find(|(_, contact)| contact.id == id)
                    .map(|(index, contact)| format!("#{} {}", index + 1, contact.name))
            });
            match current {
                Some(description) => {
                    Self::println(&format!("💬 active conversation: {description}"));
                }
                None => {
                    Self::println("ℹ️ no active conversation. Use /chat <index|name> to pick one.");
                }
            }
            return;
        }

        let selected = if let Ok(index) = target.parse::<usize>() {
            if index == 0 {
                Self::println("⚠️ friend numbers start at 1. Use /list to see them.");
                return;
            }
            self.contacts
                .get(index - 1)
                .map(|c| (index, c.id, c.name.clone()))
        } else {
            let lowered = target.to_ascii_lowercase();
            self.contacts
                .iter()
                .position(|c| c.name.to_ascii_lowercase().contains(&lowered))
                .map(|position| {
                    (
                        position + 1,
                        self.contacts[position].id,
                        self.contacts[position].name.clone(),
                    )
                })
        };

        let Some((index, id, name)) = selected else {
            Self::println(&format!(
                "❌ no friend matches '{target}'. Use /list to see them."
            ));
            return;
        };

        {
            let mut state = self.state.write().await;
            state.active_conversation = Some(id);
        }
        Self::println(&format!("💬 now chatting with #{index} {name}"));
    }

    /// Encrypt and "send" an outgoing message to the active conversation
    ///
    /// The message is encrypted with the shared [`CryptoManager`] to prove the
    /// crypto pipeline works end to end. Actual peer delivery is still handled
    /// by the (simulated) network manager.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails.
    async fn handle_outgoing_message(&self, text: String) -> Result<()> {
        let active = self.state.read().await.active_conversation;
        let Some(conversation) = active else {
            Self::println("ℹ️ no active conversation. Use /chat <index> first.");
            return Ok(());
        };

        let peer = self
            .contacts
            .iter()
            .find(|c| c.id == conversation)
            .map_or_else(|| conversation.to_string(), |c| c.name.clone());

        self.send_text_to(&peer, &text).await
    }

    /// Encrypt `text` and address it to an explicit peer
    ///
    /// Unlike [`MetaTextApp::handle_outgoing_message`] the destination is given
    /// directly (by nickname) so `/msg` can reach a peer without changing the
    /// active conversation. A peer that is not connected is buffered by the
    /// transport and delivered once it announces itself.
    ///
    /// # Arguments
    ///
    /// * `peer` - Nickname (or identifier) the frame is addressed to.
    /// * `text` - Plaintext body typed by the user.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails or the peer registry is unusable.
    async fn send_text_to(&self, peer: &str, text: &str) -> Result<()> {
        let max_length = self.config.read().await.app.max_message_length;
        let length = text.chars().count();
        if length > max_length {
            Self::println(&format!(
                "⚠️ message is {length} characters, the limit is {max_length}"
            ));
            return Ok(());
        }

        // Encrypt once so the wire size reflects the real ciphertext length.
        let ciphertext = self.crypto.encrypt(text.as_bytes())?;
        let ciphertext_hex = hex::encode(&ciphertext);

        {
            let mut state = self.state.write().await;
            state.statistics.messages_sent += 1;
            state.statistics.bytes_sent += ciphertext.len() as u64;
        }

        // Prefer delivering to the conversation partner (matched by nickname).
        // When that peer is offline the transport buffers the message and sends
        // it as soon as the peer connects.
        let outcome = self.network.send_ciphertext_to_peer(peer, &ciphertext)?;

        let display = if self.crypto.is_enabled() {
            format!(
                "{} bytes ciphertext [{}…]",
                ciphertext.len(),
                &ciphertext_hex[..ciphertext_hex.len().min(16)]
            )
        } else {
            "encryption disabled, sent in clear text".to_string()
        };

        match outcome {
            SendOutcome::Sent(receipt) => {
                let target = match receipt.peers {
                    0 => "no peers connected".to_string(),
                    1 => "1 peer".to_string(),
                    count => format!("{count} peers"),
                };
                Self::println(&format!(
                    "📤 → {peer}: {text}  ({display}; queued for {target}, id #{})",
                    receipt.message_id
                ));
            }
            SendOutcome::Queued {
                message_id,
                position,
            } => {
                Self::println(&format!(
                    "📨 → {peer}: {text}  ({display}; {peer} is offline, buffered as #{position}, id #{message_id})"
                ));
                Self::println(&format!(
                    "   it will be delivered automatically once {peer} connects"
                ));
            }
            SendOutcome::Dropped => {
                Self::println(&format!(
                    "⚠️ the outbox for {peer} is full; the message was not sent"
                ));
            }
        }

        // Mirror the message into the durable store when one is available.
        if self.database.is_persistent() {
            let record = StoredMessage::new(
                MessageDirection::Outgoing,
                peer,
                text,
                ciphertext.len() as u64,
            );
            if let Err(error) = self.database.save_message(&record).await {
                warn!("⚠️ [{}] Failed to persist outgoing message: {error}", now());
            }
        }

        Ok(())
    }

    /// Persist the session on demand (`/save`)
    ///
    /// A failure is reported to the user instead of propagating, so that a
    /// read-only data directory never aborts the interactive loop.
    async fn handle_save(&self) {
        if let Err(error) = self.save_session_state().await {
            Self::println(&format!("❌ could not save session: {error}"));
            return;
        }

        Self::println(&format!(
            "💾 session saved to {}",
            self.session_path().display()
        ));
    }

    /// Send a message to an explicit peer (`/msg <name> <text>`)
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails or the peer registry is unusable.
    async fn handle_direct_message(&self, target: &str, text: &str) -> Result<()> {
        if target.is_empty() || text.is_empty() {
            Self::println("Usage: /msg <name> <text>");
            return Ok(());
        }

        self.send_text_to(target, text).await
    }

    /// Remove a contact by 1-based index or name (`/remove <index|name>`)
    async fn handle_remove_contact(&mut self, target: &str) {
        let target = target.trim();
        if target.is_empty() {
            Self::println("Usage: /remove <index|name>");
            return;
        }

        let Some(index) = Self::find_contact_index(&self.contacts, target) else {
            Self::println(&format!(
                "❌ no friend matches '{target}'. Use /list to see them."
            ));
            return;
        };

        let removed = self.contacts.remove(index);

        // Drop the active conversation if it pointed at the removed contact.
        {
            let mut state = self.state.write().await;
            if state.active_conversation == Some(removed.id) {
                state.active_conversation = None;
            }
        }

        Self::println(&format!(
            "🗑️ removed friend #{} {}",
            index + 1,
            removed.name
        ));

        if let Err(error) = self.save_session_state().await {
            warn!(
                "⚠️ [{}] Failed to persist contacts after removal: {error}",
                now()
            );
            Self::println("⚠️ the removal is in memory only; saving the session failed");
        }
    }

    /// Resolve a `/chat`-style target (1-based index or name) to a list position
    ///
    /// # Arguments
    ///
    /// * `contacts` - Contact list to search.
    /// * `target` - Either a 1-based index or a case-insensitive name fragment.
    ///
    /// # Returns
    ///
    /// Returns the zero-based position of the first match, if any.
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

    /// Render a line of output
    ///
    /// In the plain REPL the line is printed to `stdout`; while the full screen
    /// TUI owns the terminal it is buffered into the conversation pane instead
    /// (see [`crate::tui::write_line`]).
    fn println(message: &str) {
        crate::tui::write_line(message);
    }

    /// Show the list of available commands
    fn display_help() {
        Self::println("");
        Self::println("metaText commands:");
        Self::println("  /help [command]    - show this command list or one command's help");
        Self::println("  /readme            - show the metaText introduction");
        Self::println("  /info              - show session and subsystem information");
        Self::println("  /list              - list your friends");
        Self::println("  /peers             - list connected peers");
        Self::println("  /connect <addr>    - connect to a peer (host:port)");
        Self::println("  /add <DID> [note]  - add a friend by DID address");
        Self::println("  /remove <n|name>   - remove a friend from the list");
        Self::println("  /chat <n|name>     - talk to friend n or the given name (no arg: show)");
        Self::println("  /msg <name> <text> - send <text> to one peer without switching chat");
        Self::println("  /nick <name>       - change your nickname (no arg: show it)");
        Self::println("  /status <text>     - change your status message (no arg: show it)");
        Self::println("  /whoami            - show your nickname, DID and bound address");
        Self::println("  /version           - show the application version");
        Self::println("  /uptime            - show how long this session has been running");
        Self::println("  /stats             - show runtime statistics");
        Self::println("  /history [n]       - show the last n stored messages (default 20)");
        Self::println("  /save              - persist the current session to disk");
        Self::println("  /clear             - clear the screen");
        Self::println("  /quit              - leave metaText");
        Self::println("  <text>             - send <text> to the active conversation");
        Self::println("");
    }

    /// Show the detailed help for a single command (`/help <command>`)
    ///
    /// Unknown topics are reported instead of silently falling back to the full
    /// list, so a typo is easy to spot.
    fn display_command_help(topic: &str) {
        let name = topic.trim().trim_start_matches('/').to_ascii_lowercase();
        let (usage, description) = match name.as_str() {
            "help" | "h" | "?" => (
                "/help [command]",
                "Show all commands, or the help for one of them",
            ),
            "readme" => ("/readme", "Show the metaText introduction"),
            "info" => ("/info", "Show session and subsystem information"),
            "list" | "friends" => ("/list", "List your friends"),
            "peers" | "connections" => ("/peers", "List connected peers and the local address"),
            "stats" => ("/stats", "Show runtime statistics"),
            "history" | "log" => (
                "/history [n]",
                "Show the last n stored messages (default 20)",
            ),
            "clear" | "cls" => ("/clear", "Clear the screen"),
            "nick" | "nickname" => (
                "/nick [name]",
                "Change your nickname; without a name the current one is shown",
            ),
            "status" => (
                "/status [text]",
                "Change your status message; without text the current one is shown",
            ),
            "add" => ("/add <DID> [note]", "Add a friend by DID address"),
            "chat" | "to" => (
                "/chat [n|name]",
                "Talk to friend n or the given name; without a target the active chat is shown",
            ),
            "connect" | "join" => ("/connect <host:port>", "Connect to a peer at any time"),
            "msg" | "send" => (
                "/msg <name> <text>",
                "Send <text> to one peer without switching the active conversation",
            ),
            "remove" | "rm" | "del" => ("/remove <n|name>", "Remove a friend from the list"),
            "whoami" => ("/whoami", "Show your nickname, DID and bound address"),
            "version" | "ver" => ("/version", "Show the application version"),
            "uptime" => ("/uptime", "Show how long this session has been running"),
            "save" => ("/save", "Persist the current session to disk"),
            "quit" | "exit" | "q" => ("/quit", "Leave metaText"),
            "" => {
                Self::display_help();
                return;
            }
            other => {
                Self::println(&format!(
                    "❓ no help available for '/{other}'. Type /help for the command list."
                ));
                return;
            }
        };

        Self::println("");
        Self::println(&format!("Usage: {usage}"));
        Self::println(&format!("  {description}"));
        Self::println("");
    }

    /// Show the metaText introduction
    fn display_readme() {
        Self::println("");
        Self::println(">> metaText ~ a Web3 decentralized instant messenger");
        Self::println("   - P2P text messaging with end-to-end encryption");
        Self::println("   - Your identity is a self generated key pair (no server needed)");
        Self::println("   - Use /add <DID_Address> Hello to invite a friend");
        Self::println("   - Use /chat <n> to start talking, /info shows the numbering");
        Self::println("   arkDelphi Metaverse Lab ~ arksong2018@gmail.com");
        Self::println("");
    }

    /// Print the local identity (`/whoami`)
    fn display_whoami(&self) {
        let local = self
            .network
            .local_addr()
            .map_or_else(|| "not listening".to_string(), |addr| addr.to_string());

        Self::println("");
        Self::println("metaText identity");
        Self::println(&format!("  nickname : {}", self.nickname));
        Self::println(&format!("  status   : {}", self.status_message));
        Self::println(&format!("  DID      : {}", self.identity));
        Self::println(&format!("  address  : {local}"));
        Self::println("");
    }

    /// Print the application version and crypto algorithm (`/version`)
    fn display_version(&self) {
        Self::println(&format!(
            "metaText v{} (encryption: {})",
            env!("CARGO_PKG_VERSION"),
            self.crypto.algorithm()
        ));
    }

    /// Print how long the session has been running (`/uptime`)
    async fn display_uptime(&self) {
        let start = self.state.read().await.statistics.start_time;
        let elapsed = start.map_or_else(
            || "unknown".to_string(),
            |started| {
                let seconds = chrono::Utc::now()
                    .signed_duration_since(started)
                    .num_seconds()
                    .max(0);
                Self::format_duration(seconds)
            },
        );

        Self::println(&format!("⏱️ uptime: {elapsed}"));
    }

    /// Render a whole number of seconds as `Hh MMm SSs`
    ///
    /// # Arguments
    ///
    /// * `total_seconds` - Non-negative duration in seconds.
    ///
    /// # Returns
    ///
    /// Returns the formatted duration, e.g. `1h 02m 03s`.
    fn format_duration(total_seconds: i64) -> String {
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let seconds = total_seconds % 60;
        format!("{hours}h {minutes:02}m {seconds:02}s")
    }

    /// Show session and subsystem information
    async fn display_info(&self) {
        let config = self.config.read().await;
        let state = self.state.read().await;

        Self::println("");
        Self::println("════════════════ metaText status ════════════════");
        Self::println(&format!("  nickname        : {}", self.nickname));
        Self::println(&format!("  status          : {}", self.status_message));
        Self::println(&format!("  identity (DID)  : {}", self.identity));
        Self::println(&format!(
            "  encryption      : {} ({})",
            if self.crypto.is_enabled() {
                "enabled"
            } else {
                "disabled"
            },
            self.crypto.algorithm()
        ));
        Self::println(&format!(
            "  mode            : {}",
            self.args.effective_mode()
        ));
        Self::println(&format!(
            "  network         : {} (port {}, {} peer(s) connected, max {} conn)",
            if self.network.is_started() {
                "running"
            } else {
                "stopped"
            },
            self.network.port(),
            self.network.connected_peers(),
            self.network.max_connections()
        ));
        Self::println(&format!(
            "  database        : {} ({}, max {} conn)",
            if self.database.is_initialized() {
                "connected"
            } else {
                "disconnected"
            },
            self.database.connection_string(),
            self.database.max_connections()
        ));
        Self::println(&format!("  friends         : {}", self.contacts.len()));
        Self::println(&format!(
            "  active chat     : {}",
            state.active_conversation.map_or_else(
                || "none".to_string(),
                |id| {
                    self.contacts
                        .iter()
                        .find(|c| c.id == id)
                        .map_or_else(|| id.to_string(), |c| c.name.clone())
                }
            )
        ));
        Self::println(&format!(
            "  messages sent   : {}",
            state.statistics.messages_sent
        ));
        Self::println(&format!(
            "  messages received: {}",
            state.statistics.messages_received
        ));
        Self::println(&format!(
            "  config file     : {}",
            self.args.config_path.display()
        ));
        Self::println(&format!(
            "  data directory  : {}",
            self.data_dir().display()
        ));
        Self::println(&format!(
            "  app             : {} v{}",
            config.app.name, config.app.version
        ));
        Self::println("═════════════════════════════════════════════════");
        Self::println("");
    }

    /// Connect to a peer address entered interactively (`/connect`)
    async fn handle_connect(&self, target: &str) {
        let address = target.trim();
        if address.is_empty() {
            Self::println("Usage: /connect <host:port>");
            return;
        }

        if !crate::cli::CliArgs::is_valid_peer_address(address) {
            Self::println(&format!("⚠️ '{address}' is not a valid host:port address"));
            return;
        }

        match self.network.connect(address).await {
            Ok(remote) => Self::println(&format!("🔗 connected to peer {remote}")),
            Err(error) => Self::println(&format!("❌ could not connect to {address}: {error}")),
        }
    }

    /// Print the peers currently connected over the transport
    fn display_peers(&self) {
        let local = self
            .network
            .local_addr()
            .map_or_else(|| "not listening".to_string(), |addr| addr.to_string());
        let nicknames = self.network.peer_nicknames();
        let connected = self.network.connected_peers();

        Self::println("");
        Self::println(&format!("Local address: {local}"));

        let queued = self.network.queued_messages();
        if queued > 0 {
            Self::println(&format!("{queued} message(s) waiting to be delivered"));
        }

        if connected == 0 {
            Self::println("No peers connected yet.");
            let pending = self.network.pending_peers();
            if pending > 0 {
                Self::println(&format!(
                    "{pending} address(es) are being retried in the background:"
                ));
                for address in self.network.desired_peers() {
                    Self::println(&format!("  - {address}"));
                }
            } else {
                Self::println("Start another instance with the same --passphrase and point it");
                Self::println("here using --peer <host:port> (or /connect).");
            }
        } else {
            Self::println(&format!("{connected} peer(s) connected:"));
            for (index, name) in nicknames.iter().enumerate() {
                Self::println(&format!("  {}. {name}", index + 1));
            }
            let unnamed = connected.saturating_sub(nicknames.len());
            if unnamed > 0 {
                Self::println(&format!("  (+{unnamed} still completing the handshake)"));
            }
        }
        Self::println("");
    }

    /// Print the contact list with 1-based numbering
    ///
    /// The friend selected with `/chat` is tagged with `← active chat` so it is
    /// obvious where a plain (unprefixed) message will be sent.
    async fn display_contacts(&self) {
        Self::println("");
        if self.contacts.is_empty() {
            Self::println("You have no friends yet. Use /add <DID_Address> to invite one.");
            Self::println("");
            return;
        }

        let active = self.state.read().await.active_conversation;
        Self::println("Friends:");
        for (index, contact) in self.contacts.iter().enumerate() {
            let note = contact
                .note
                .as_ref()
                .map_or(String::new(), |n| format!("  // {n}"));
            let marker = if active == Some(contact.id) {
                "  ← active chat"
            } else {
                ""
            };
            Self::println(&format!(
                "  {}. {} [{}]{}{}",
                index + 1,
                contact.name,
                contact.status.as_name(),
                note,
                marker
            ));
        }
        Self::println("");
    }

    /// Print runtime statistics
    async fn display_statistics(&self) {
        let state = self.state.read().await;
        let counters = &state.statistics;
        let uptime = counters.start_time.map_or_else(
            || "unknown".to_string(),
            |start| {
                let seconds = chrono::Utc::now()
                    .signed_duration_since(start)
                    .num_seconds()
                    .max(0);
                format!("{seconds}s")
            },
        );

        Self::println("");
        Self::println("Runtime statistics:");
        Self::println(&format!("  uptime            : {uptime}"));
        Self::println(&format!("  messages sent     : {}", counters.messages_sent));
        Self::println(&format!(
            "  messages received : {}",
            counters.messages_received
        ));
        Self::println(&format!(
            "  active connections: {}",
            counters.active_connections
        ));
        Self::println(&format!("  bytes sent        : {}", counters.bytes_sent));
        Self::println(&format!(
            "  bytes received    : {}",
            counters.bytes_received
        ));
        Self::println("");
    }

    /// Show the most recent persisted messages
    ///
    /// When the crate was built without a persistent backend (or the database
    /// failed to start) a short notice explains how to enable history.
    async fn display_history(&self, limit: Option<usize>) {
        if !self.database.is_persistent() {
            Self::println("ℹ️ message history is not available in this build.");
            Self::println("   Rebuild with `--features sqlite` to persist messages.");
            return;
        }

        let limit = limit.unwrap_or(20).clamp(1, 1000);
        match self.database.recent_messages(limit).await {
            Ok(messages) if messages.is_empty() => {
                Self::println("ℹ️ no stored messages yet.");
            }
            Ok(messages) => {
                Self::println("");
                Self::println(&format!("Last {} stored message(s):", messages.len()));
                for message in &messages {
                    let arrow = match message.direction {
                        MessageDirection::Outgoing => "→",
                        MessageDirection::Incoming => "←",
                    };
                    Self::println(&format!(
                        "  [{}] {arrow} {}: {} ({} B)",
                        message.created_at.format("%Y-%m-%d %H:%M:%S"),
                        message.peer,
                        message.body,
                        message.wire_bytes
                    ));
                }
                Self::println("");
            }
            Err(error) => {
                warn!("⚠️ [{}] Failed to read message history: {error}", now());
                Self::println(&format!("⚠️ could not read message history: {error}"));
            }
        }
    }

    /// Display startup banner and information
    fn display_startup_banner() {
        // The full screen TUI already has a status header and cannot render
        // raw ANSI escape codes, so it gets a short plain-text variant.
        if crate::tui::output_sink().is_enabled() {
            Self::println("🌟 Welcome to Metaverse Web3 Communication CyberSpace");
            Self::println(">> metaText ~ a Simple Web3 Text Instant Messager");
            Self::println("   ::: arkDelphi Metaverse Lab");
            Self::println("Type `/help` for the command list, `/readme` for an introduction.");
            Self::println("");
            return;
        }

        println!("\n\n\n");
        println!("🌟 Welcome to Metaverse Web3 Communication CyberSpace");
        println!("\x1b[36m");
        println!("                __       ___________              __   ");
        println!("   ___    _____/  |______\\__    ___/___ ___  ____/  |_ ");
        println!(" /     \\_/ __ \\   __\\__  \\ |    |_/ __ \\  \\/  /\\   __");
        println!("|  Y Y  \\  ___/|  |  / __ \\|    |\\  ___/ >    <  |  |  ");
        println!("|__|_|  /\\___  >__| (____  /____| \\___  >__/\\_ \\ |__|  ");
        println!("      \\/     \\/          \\/           \\/      \\/     ");
        println!("\x1b[0m");
        println!(">> metaText ~ a Simple Web3 Text Instant Messager");
        println!("   ::: arkDelphi Metaverse Lab");
        println!("         arksong2018@gmail.com");
        println!("..................................................................");
        println!();
        println!("Type `/help` to get metaText command list.");
        println!("Type `/readme` to get metaText introduction.\n");
    }

    /// Publish the latest status into the shared TUI snapshot
    ///
    /// The full screen interface renders this snapshot on every frame. The
    /// method is a no-op when the interface is not active, so it is cheap to
    /// call after every processed event.
    async fn refresh_tui_info(&self) {
        if !self.tui.is_active() {
            return;
        }

        let state = self.state.read().await;
        let active_chat = state.active_conversation.and_then(|id| {
            self.contacts
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
        });

        let info = TuiInfo {
            nickname: self.nickname.clone(),
            status_message: self.status_message.clone(),
            identity: self.identity.clone(),
            mode: self.args.effective_mode().to_string(),
            network: format!(
                "{} (port {}, {} bootstrap, max {} conn)",
                if self.network.is_started() {
                    "running"
                } else {
                    "stopped"
                },
                self.network.port(),
                self.network.bootstrap_nodes().len(),
                self.network.max_connections()
            ),
            database: format!(
                "{} ({})",
                if self.database.is_initialized() {
                    "connected"
                } else {
                    "disconnected"
                },
                self.database.connection_string()
            ),
            encryption: format!(
                "{} ({})",
                if self.crypto.is_enabled() {
                    "enabled"
                } else {
                    "disabled"
                },
                self.crypto.algorithm()
            ),
            friends: self.contacts.iter().map(|c| c.name.clone()).collect(),
            peers: self.network.peer_nicknames(),
            pending_peers: self.network.pending_peers(),
            active_chat,
            messages_sent: state.statistics.messages_sent,
            messages_received: state.statistics.messages_received,
            bytes_sent: state.statistics.bytes_sent,
            bytes_received: state.statistics.bytes_received,
        };
        drop(state);

        if let Ok(mut guard) = self.tui_info.lock() {
            *guard = info;
        }
    }

    /// Handle periodic maintenance tasks
    ///
    /// Runs once per second from the main event loop: it mirrors the network
    /// connection count into the statistics and performs the timed session
    /// auto-save.
    ///
    /// # Errors
    ///
    /// Returns an error if the session file cannot be written.
    #[instrument(skip(self))]
    async fn handle_periodic_tasks(&mut self) -> Result<()> {
        // Mirror live subsystem state into the runtime statistics.
        {
            let mut state = self.state.write().await;
            state.statistics.active_connections =
                u32::try_from(self.network.connected_peers()).unwrap_or(u32::MAX);
        }

        // Timed auto-save of the session (contacts, nickname, statistics).
        let interval = self.config.read().await.app.auto_save_interval;
        if interval > 0 && self.last_auto_save.elapsed().as_secs() >= interval {
            self.save_session_state().await?;
            self.last_auto_save = tokio::time::Instant::now();
        }

        Ok(())
    }

    /// Directory that stores user data (session file, logs, database)
    fn data_dir(&self) -> PathBuf {
        self.args
            .data_directory()
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Full path of the persisted session file
    fn session_path(&self) -> PathBuf {
        self.data_dir().join(SESSION_STATE_FILE)
    }

    /// Persist the current session (nickname, contacts, statistics) to disk
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot
    /// be serialized or written.
    pub async fn save_session_state(&self) -> Result<()> {
        let path = self.session_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create data directory {}", parent.display()))?;
        }

        let statistics = {
            let state = self.state.read().await;
            crate::types::AppStatistics {
                // The start time is session-only, so it is not persisted.
                start_time: None,
                ..state.statistics.clone()
            }
        };

        let snapshot = SessionState {
            nickname: self.nickname.clone(),
            status_message: self.status_message.clone(),
            contacts: self.contacts.clone(),
            statistics,
        };

        let json =
            serde_json::to_string_pretty(&snapshot).context("Failed to serialize session state")?;
        tokio::fs::write(&path, json)
            .await
            .with_context(|| format!("Failed to write session file {}", path.display()))?;

        debug!("💾 [{}] Session saved to {}", now(), path.display());
        Ok(())
    }

    /// Restore a previously saved session, if one exists
    ///
    /// A missing or corrupt session file is ignored (with a warning) so that a
    /// fresh user always reaches the prompt.
    async fn load_session_state(&mut self) {
        let path = self.session_path();
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            debug!("ℹ️ [{}] No session file at {}", now(), path.display());
            return;
        };

        match serde_json::from_str::<SessionState>(&raw) {
            Ok(snapshot) => {
                // A nickname passed on the command line wins over the persisted
                // session, matching the "flags always win over file values"
                // contract. Only fall back to the stored name when the user did
                // not request one with `--nick`.
                if !snapshot.nickname.is_empty() && self.args.nickname.is_none() {
                    self.nickname = snapshot.nickname;
                }
                if !snapshot.status_message.is_empty() {
                    self.status_message = snapshot.status_message;
                }
                self.contacts = snapshot.contacts;
                {
                    let mut state = self.state.write().await;
                    state.statistics = snapshot.statistics;
                }
                info!(
                    "✅ [{}] Session restored from {} ({} friend(s))",
                    now(),
                    path.display(),
                    self.contacts.len()
                );
            }
            Err(error) => {
                warn!(
                    "⚠️ [{}] Ignoring unreadable session file {}: {}",
                    now(),
                    path.display(),
                    error
                );
            }
        }
    }

    /// Perform graceful shutdown of all subsystems
    ///
    /// # Errors
    ///
    /// Returns an error if a subsystem fails to stop cleanly.
    #[instrument(skip(self))]
    async fn shutdown(&mut self) -> Result<()> {
        info!("🔄 [{}] Beginning graceful shutdown", now());

        // Save the session before tearing anything down.
        if let Err(error) = self.save_session_state().await {
            warn!("⚠️ [{}] Failed to save session: {:?}", now(), error);
        }

        self.network
            .shutdown()
            .await
            .context("Failed to stop network manager")?;
        self.database
            .shutdown()
            .await
            .context("Failed to stop database manager")?;
        self.tui
            .shutdown()
            .await
            .context("Failed to stop TUI manager")?;

        info!("✅ [{}] Graceful shutdown completed successfully", now());
        Ok(())
    }
}

/// Format the current UTC timestamp for log and console output
fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    /// Build a test application with an isolated data directory
    async fn test_app() -> (MetaTextApp, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.network.bootstrap_nodes.clear();
        config.database.connection_string =
            dir.path().join("test.db").to_string_lossy().to_string();
        config.app.auto_save_interval = 1;

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            mode: crate::cli::AppMode::Tui,
            ..CliArgs::default()
        };

        let app = MetaTextApp::new(config, args).await.expect("app");
        (app, dir)
    }

    /// A freshly created application owns a usable crypto manager
    #[tokio::test]
    async fn test_app_initialization() -> Result<()> {
        let (app, _dir) = test_app().await;
        assert!(!app.event_sender().is_closed());
        assert!(app.event_receiver.is_some());
        assert_eq!(app.crypto.key().len(), crate::crypto::KEY_LENGTH);
        assert!(app.contacts().is_empty());
        Ok(())
    }

    /// The displayed identity is random and never the encryption key
    #[tokio::test]
    async fn test_identity_is_random_and_not_the_key() -> Result<()> {
        let (first, _dir_a) = test_app().await;
        let (second, _dir_b) = test_app().await;

        // 32 random bytes rendered as uppercase hex.
        assert_eq!(first.identity.len(), 64);
        // The identity must not leak the (possibly passphrase derived) key.
        assert_ne!(first.identity, first.crypto.key_hex().to_uppercase());
        // Each session gets its own identity.
        assert_ne!(first.identity, second.identity);
        Ok(())
    }

    /// Peers configured with the same passphrase share the message key
    #[tokio::test]
    async fn test_passphrase_shares_the_message_key() -> Result<()> {
        async fn app_with_passphrase(passphrase: &str) -> Result<(MetaTextApp, tempfile::TempDir)> {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = AppConfig::default();
            config.network.port = 0;
            config.network.bootstrap_nodes.clear();
            let args = CliArgs {
                data_dir: Some(dir.path().to_path_buf()),
                passphrase: Some(passphrase.to_string()),
                ..CliArgs::default()
            };
            Ok((MetaTextApp::new(config, args).await?, dir))
        }

        let (alice, _dir_a) = app_with_passphrase("shared secret").await?;
        let (bob, _dir_b) = app_with_passphrase("shared secret").await?;
        let (eve, _dir_c) = app_with_passphrase("other secret").await?;

        // Same passphrase derives the same key, so peers can decrypt.
        assert_eq!(alice.crypto.key(), bob.crypto.key());
        assert_ne!(alice.crypto.key(), eve.crypto.key());

        // Even so, the shared key is never published as the identity.
        assert_ne!(alice.identity, alice.crypto.key_hex().to_uppercase());
        assert_ne!(alice.identity, bob.identity);
        Ok(())
    }

    /// A relative database path is resolved inside the configured data directory
    #[tokio::test]
    async fn test_relative_database_path_uses_data_dir() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.database.database_type = "sqlite".to_string();
        config.database.connection_string = "relative-meta.db".to_string();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            ..CliArgs::default()
        };

        let app = MetaTextApp::new(config, args).await?;
        assert_eq!(
            app.database().connection_string(),
            dir.path().join("relative-meta.db").to_string_lossy()
        );
        Ok(())
    }

    /// Absolute database paths and in-memory databases are left untouched
    #[tokio::test]
    async fn test_absolute_and_memory_database_paths_are_kept() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");

        for configured in [
            dir.path().join("absolute.db").to_string_lossy().to_string(),
            ":memory:".to_string(),
        ] {
            let mut config = AppConfig::default();
            config.database.database_type = "sqlite".to_string();
            config.database.connection_string = configured.clone();

            let args = CliArgs {
                data_dir: Some(dir.path().to_path_buf()),
                ..CliArgs::default()
            };

            let app = MetaTextApp::new(config, args).await?;
            assert_eq!(app.database().connection_string(), configured);
        }

        Ok(())
    }

    /// `/add` records a contact and `/chat` activates it
    #[tokio::test]
    async fn test_add_and_chat_contact() -> Result<()> {
        let (mut app, _dir) = test_app().await;

        app.handle_command(Command::Add {
            identifier: "DID123".to_string(),
            note: Some("friend".to_string()),
        })
        .await?;
        assert_eq!(app.contacts().len(), 1);
        assert_eq!(app.contacts()[0].name, "DID123");

        // Adding the same contact twice must not duplicate it.
        app.handle_command(Command::Add {
            identifier: "DID123".to_string(),
            note: None,
        })
        .await?;
        assert_eq!(app.contacts().len(), 1);

        app.handle_command(Command::Chat("1".to_string())).await?;
        assert!(app.state.read().await.active_conversation.is_some());
        Ok(())
    }

    /// Sending a message updates counters and encrypts the payload
    #[tokio::test]
    async fn test_outgoing_message_updates_statistics() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Add {
            identifier: "DID123".to_string(),
            note: None,
        })
        .await?;
        app.handle_command(Command::Chat("1".to_string())).await?;
        app.handle_command(Command::Message("hello".to_string()))
            .await?;

        let state = app.state.read().await;
        assert_eq!(state.statistics.messages_sent, 1);
        assert!(state.statistics.bytes_sent > 0);
        Ok(())
    }

    /// `/nick` and `/status` update the local profile
    #[tokio::test]
    async fn test_nick_and_status() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Nick("Alice".to_string()))
            .await?;
        app.handle_command(Command::Status("brb".to_string()))
            .await?;

        assert_eq!(app.nickname(), "Alice");
        assert_eq!(app.status_message, "brb");
        Ok(())
    }

    /// Bare `/nick`, `/status` and `/chat` are read-only queries
    #[tokio::test]
    async fn test_bare_profile_and_chat_commands_are_queries() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Nick("Alice".to_string()))
            .await?;
        app.handle_command(Command::Status("busy".to_string()))
            .await?;
        app.handle_command(Command::Add {
            identifier: "DID123".to_string(),
            note: None,
        })
        .await?;
        app.handle_command(Command::Chat("1".to_string())).await?;

        let active = app.state.read().await.active_conversation;
        assert!(active.is_some());

        // The empty argument form must report, never mutate, the current state.
        assert!(!app.handle_command(Command::Nick(String::new())).await?);
        assert!(!app.handle_command(Command::Status(String::new())).await?);
        assert!(!app.handle_command(Command::Chat(String::new())).await?);

        assert_eq!(app.nickname(), "Alice");
        assert_eq!(app.status_message, "busy");
        assert_eq!(app.state.read().await.active_conversation, active);
        Ok(())
    }

    /// Bare `/chat` without a selection leaves the (empty) conversation untouched
    #[tokio::test]
    async fn test_bare_chat_without_selection_is_a_noop() -> Result<()> {
        let (mut app, _dir) = test_app().await;

        assert!(!app.handle_command(Command::Chat(String::new())).await?);
        assert!(app.state.read().await.active_conversation.is_none());
        Ok(())
    }

    /// `/quit` asks the event loop to stop
    #[tokio::test]
    async fn test_quit_requests_shutdown() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        assert!(app.handle_command(Command::Quit).await?);
        Ok(())
    }

    /// Unknown commands and empty input never stop the loop
    #[tokio::test]
    async fn test_unknown_and_empty_input_do_not_stop() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        assert!(
            !app.handle_command(Command::Unknown("nope".to_string()))
                .await?
        );
        assert!(!app.handle_user_input("   ").await?);
        assert!(!app.handle_user_input("hello world").await?);
        Ok(())
    }

    /// The whole session (nickname + contacts) round-trips through disk
    #[tokio::test]
    async fn test_session_state_roundtrip() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Nick("Bob".to_string())).await?;
        app.handle_command(Command::Add {
            identifier: "DIDABC".to_string(),
            note: Some("note".to_string()),
        })
        .await?;
        app.save_session_state().await?;

        // Reset in-memory state and load it back.
        app.nickname = "reset".to_string();
        app.contacts.clear();
        app.load_session_state().await;

        assert_eq!(app.nickname(), "Bob");
        assert_eq!(app.contacts().len(), 1);
        assert_eq!(app.contacts()[0].name, "DIDABC");
        Ok(())
    }

    /// An explicit `--nick` overrides the nickname stored in the session file
    #[tokio::test]
    async fn test_cli_nickname_wins_over_session() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.network.bootstrap_nodes.clear();
        config.database.connection_string =
            dir.path().join("test.db").to_string_lossy().to_string();

        // First run persists a session announcing a different nickname.
        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            ..CliArgs::default()
        };
        let mut app = MetaTextApp::new(config.clone(), args).await?;
        app.handle_command(Command::Nick("Stored".to_string()))
            .await?;
        app.save_session_state().await?;

        // A later start with `--nick` must keep the command line value.
        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            nickname: Some("FromCli".to_string()),
            ..CliArgs::default()
        };
        let mut app = MetaTextApp::new(config, args).await?;
        assert_eq!(app.nickname(), "FromCli");

        app.load_session_state().await;
        assert_eq!(app.nickname(), "FromCli");
        Ok(())
    }

    /// Periodic tasks keep statistics in sync with the network state
    #[tokio::test]
    async fn test_periodic_tasks_reflect_network_state() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.network.start().await.expect("network start");
        app.handle_periodic_tasks().await?;

        // With no peers dialled in, the live connection count is zero.
        assert_eq!(app.state.read().await.statistics.active_connections, 0);
        Ok(())
    }

    /// The `run` subcommand makes the application interactive
    #[tokio::test]
    async fn test_run_subcommand_is_interactive() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.network.bootstrap_nodes.clear();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            // `--mode` is deliberately left at its default here.
            command: Some(crate::cli::CliCommand::Run),
            ..CliArgs::default()
        };
        let app = MetaTextApp::new(config, args).await?;

        assert!(app.is_interactive());
        Ok(())
    }

    /// `--headless` still suppresses every user interface, including `run`
    #[tokio::test]
    async fn test_headless_overrides_run_subcommand() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.network.port = 0;
        config.network.bootstrap_nodes.clear();

        let args = CliArgs {
            data_dir: Some(dir.path().to_path_buf()),
            headless: true,
            command: Some(crate::cli::CliCommand::Run),
            ..CliArgs::default()
        };
        let app = MetaTextApp::new(config, args).await?;

        assert!(!app.is_interactive());
        Ok(())
    }

    /// `/chat`-style targets resolve by 1-based index or name fragment
    #[test]
    fn test_find_contact_index_by_number_and_name() {
        let contacts = vec![
            Contact::new("Alice".to_string(), vec![1]),
            Contact::new("Bob".to_string(), vec![2]),
        ];

        assert_eq!(MetaTextApp::find_contact_index(&contacts, "1"), Some(0));
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "2"), Some(1));
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "alice"), Some(0));
        // Case-insensitive substring match.
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "BO"), Some(1));
        // Out of range and unknown names resolve to nothing.
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "0"), None);
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "9"), None);
        assert_eq!(MetaTextApp::find_contact_index(&contacts, "carol"), None);
    }

    /// Uptime formatting is zero padded and never negative
    #[test]
    fn test_format_duration_renders_hours_minutes_seconds() {
        assert_eq!(MetaTextApp::format_duration(0), "0h 00m 00s");
        assert_eq!(MetaTextApp::format_duration(63), "0h 01m 03s");
        assert_eq!(MetaTextApp::format_duration(3723), "1h 02m 03s");
    }

    /// `/remove` drops the contact, clears the active chat and persists
    #[tokio::test]
    async fn test_remove_command_drops_contact_and_clears_active_chat() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Add {
            identifier: "DIDABC".to_string(),
            note: None,
        })
        .await?;
        app.handle_chat_target("1").await;
        assert!(app.state.read().await.active_conversation.is_some());

        assert!(!app.handle_command(Command::Remove("1".to_string())).await?);

        assert!(app.contacts().is_empty());
        assert!(app.state.read().await.active_conversation.is_none());
        // The removal is written through to the session file.
        assert!(app.session_path().exists());
        Ok(())
    }

    /// `/remove` on an unknown target is harmless
    #[tokio::test]
    async fn test_remove_unknown_contact_is_a_noop() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        assert!(
            !app.handle_command(Command::Remove("nobody".to_string()))
                .await?
        );
        assert!(app.contacts().is_empty());
        Ok(())
    }

    /// `/msg` queues the frame for a peer that is not connected yet
    #[tokio::test]
    async fn test_direct_message_queues_for_offline_peer() -> Result<()> {
        let (mut app, _dir) = test_app().await;

        assert!(
            !app.handle_command(Command::Msg {
                target: "Alice".to_string(),
                text: "hello".to_string(),
            })
            .await?
        );

        assert_eq!(app.state.read().await.statistics.messages_sent, 1);
        assert_eq!(app.network.queued_messages(), 1);
        Ok(())
    }

    /// `/msg` without a body only prints the usage line
    #[tokio::test]
    async fn test_direct_message_without_body_is_rejected() -> Result<()> {
        let (mut app, _dir) = test_app().await;

        assert!(
            !app.handle_command(Command::Msg {
                target: "Alice".to_string(),
                text: String::new(),
            })
            .await?
        );

        assert_eq!(app.state.read().await.statistics.messages_sent, 0);
        assert_eq!(app.network.queued_messages(), 0);
        Ok(())
    }

    /// `/save` writes the session file and never stops the loop
    #[tokio::test]
    async fn test_save_command_persists_session() -> Result<()> {
        let (mut app, _dir) = test_app().await;
        app.handle_command(Command::Nick("Zoe".to_string())).await?;

        assert!(!app.handle_command(Command::Save).await?);
        assert!(app.session_path().exists());
        Ok(())
    }

    /// The inspection commands return to the prompt instead of exiting
    #[tokio::test]
    async fn test_inspection_commands_do_not_stop_the_loop() -> Result<()> {
        let (mut app, _dir) = test_app().await;

        assert!(!app.handle_command(Command::Whoami).await?);
        assert!(!app.handle_command(Command::Version).await?);
        assert!(!app.handle_command(Command::Uptime).await?);
        assert!(
            !app.handle_command(Command::Help(Some("msg".to_string())))
                .await?
        );
        assert!(
            !app.handle_command(Command::Help(Some("nope".to_string())))
                .await?
        );
        Ok(())
    }
}
