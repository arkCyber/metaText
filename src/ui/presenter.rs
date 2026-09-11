/*!
 * presenter.rs
 *
 * Shared, UI-agnostic presentation of core replies for the CLI and TUI.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Parses interactive lines and maps them onto [`Request`]s
 * - Formats data replies into readable lines (all wording lives here)
 * - Keeps a cached session/contact snapshot for headers and TUI panels
 * - Never touches crypto, storage or the transport directly
 */

use tokio::sync::broadcast;
use tracing::info;

use crate::commands::{self, Command};
use crate::ipc::client::LocalClient;
use crate::ipc::protocol::{
    ContactView, CoreEvent, ErrorCode, Reply, Request, SendOutcomeKind, SessionInfo, StatisticsView,
};
use crate::ipc::CoreClient;
use crate::tui::{clear_output, write_line, TuiInfo};

use super::text;

/// Write one line to whichever output stream currently owns the terminal.
fn emit(line: &str) {
    write_line(line);
}

/// Write a block of lines.
fn emit_all(lines: &[String]) {
    for line in lines {
        emit(line);
    }
}

/// Shared presentation state for both front-ends.
#[derive(Debug)]
pub struct Presenter {
    /// Core client (never a subsystem).
    core: LocalClient,

    /// Latest session snapshot.
    session: SessionInfo,

    /// Latest friend list snapshot.
    contacts: Vec<ContactView>,

    /// Latest runtime counters snapshot.
    statistics: StatisticsView,

    /// Event stream, taken by the front-end that owns the loop.
    events: Option<broadcast::Receiver<CoreEvent>>,

    /// Set when the user asked to leave.
    stop: bool,

    /// Whether the app already announced itself.
    announced: bool,
}

impl Presenter {
    /// Create a presenter bound to a core client.
    #[must_use]
    pub fn new(core: LocalClient) -> Self {
        let events = core.subscribe();
        Self {
            core,
            session: SessionInfo::default(),
            contacts: Vec::new(),
            statistics: StatisticsView::default(),
            events,
            stop: false,
            announced: false,
        }
    }

    /// Take the event stream.
    ///
    /// The front-end that owns the event loop calls this once. A transport
    /// without event support yields a receiver that never produces anything.
    pub fn take_events(&mut self) -> broadcast::Receiver<CoreEvent> {
        self.events
            .take()
            .unwrap_or_else(|| broadcast::channel(1).1)
    }

    /// Whether the user requested shutdown.
    #[must_use]
    pub const fn wants_stop(&self) -> bool {
        self.stop
    }

    /// The latest session snapshot.
    #[must_use]
    pub const fn session(&self) -> &SessionInfo {
        &self.session
    }

    /// Refresh the cached session and contact snapshots.
    pub async fn refresh(&mut self) {
        if let Ok(Reply::Session { session }) = self.core.request(Request::SessionInfo).await {
            self.session = session;
        }
        if let Ok(Reply::Contacts { contacts }) = self.core.request(Request::ListContacts).await {
            self.contacts = contacts;
        }
        if let Ok(Reply::Statistics { statistics }) = self.core.request(Request::Statistics).await {
            self.statistics = statistics;
        }
    }

    /// Show the startup banner and the initial status block.
    ///
    /// `full_screen` selects the plain variant used while the alternate screen
    /// owns the terminal.
    pub async fn announce(&mut self, full_screen: bool) {
        if self.announced {
            return;
        }
        self.announced = true;

        emit_all(&text::startup_banner(full_screen));
        self.refresh().await;
        self.show_info();
    }

    /// Ask the core to stop.
    pub async fn request_shutdown(&self) {
        if let Err(error) = self.core.request(Request::Shutdown).await {
            emit(&format!("⚠️ shutdown was not acknowledged: {error}"));
        }
    }

    /// Handle one raw line typed by the user.
    ///
    /// # Returns
    ///
    /// Returns `true` when the line requests application shutdown.
    pub async fn handle_line(&mut self, line: &str) -> bool {
        match commands::parse(line) {
            // Ignore empty input so pressing Enter does not spam the output.
            Command::Message(text) if text.is_empty() => false,
            command => self.handle_command(command).await,
        }
    }

    /// Handle a parsed [`Command`].
    ///
    /// # Returns
    ///
    /// Returns `true` when the command requests application shutdown.
    pub async fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Help(topic) => match topic.as_deref() {
                Some(name) => emit_all(&text::command_help_lines(name)),
                None => emit_all(&text::help_lines()),
            },
            Command::Readme => emit_all(&text::readme_lines()),
            Command::Info => {
                self.refresh().await;
                self.show_info();
            }
            Command::List => {
                self.refresh().await;
                self.show_contacts();
            }
            Command::Peers => {
                self.refresh().await;
                self.show_peers();
            }
            Command::Stats => self.show_statistics().await,
            Command::History(limit) => self.show_history(limit).await,
            Command::Clear => clear_output(),
            Command::Nick(name) => self.set_nickname(name).await,
            Command::Status(text) => self.set_status(text).await,
            Command::Add { identifier, note } => self.add_contact(identifier, note).await,
            Command::Chat(target) => self.select_conversation(&target).await,
            Command::Connect(target) => self.connect(&target).await,
            Command::Message(text) => self.send_message(None, &text).await,
            Command::Whoami => {
                self.refresh().await;
                self.show_whoami();
            }
            Command::Version => self.show_version(),
            Command::Uptime => {
                self.refresh().await;
                emit(&format!(
                    "⏱️ uptime: {}",
                    text::format_duration(self.session.uptime_seconds)
                ));
            }
            Command::Save => self.save_session().await,
            Command::Msg { target, text } => self.send_message(Some(target), &text).await,
            Command::Remove(target) => self.remove_contact(&target).await,
            Command::Quit => {
                info!("👋 User requested quit");
                self.stop = true;
            }
            Command::Unknown(name) => emit(&format!(
                "❓ unknown command '/{name}'. Type /help for the command list."
            )),
        }

        self.stop
    }

    /// Render a [`CoreEvent`] pushed by the core.
    ///
    /// # Returns
    ///
    /// Returns `true` when the event requests application shutdown.
    pub async fn handle_event(&mut self, event: &CoreEvent) -> bool {
        match event {
            CoreEvent::MessageReceived { peer, body, .. } => {
                emit(&format!("📥 {peer}: {body}"));
                self.refresh().await;
            }
            CoreEvent::MessageUndecodable {
                peer, wire_bytes, ..
            } => {
                emit(&format!(
                    "⚠️ {peer} sent {wire_bytes} bytes that are not valid UTF-8 text; \
                     the frame was not displayed"
                ));
            }
            CoreEvent::MessageDelivered {
                peer, message_id, ..
            } => {
                emit(&format!("✅ delivered to {peer} (#{message_id})"));
            }
            CoreEvent::PeerConnected { nickname, .. } => {
                if !nickname.is_empty() {
                    emit(&format!("🔗 {nickname} connected"));
                }
                self.refresh().await;
            }
            CoreEvent::PeerDisconnected { peer_id, reason } => {
                emit(&format!("🔌 {peer_id} disconnected: {reason}"));
                self.refresh().await;
            }
            CoreEvent::ConversationChanged { target } => {
                emit(&format!("💬 now chatting with {target}"));
                self.refresh().await;
            }
            CoreEvent::NicknameChanged { nickname } => {
                self.session.nickname.clone_from(nickname);
            }
            CoreEvent::StatusChanged { text } => {
                self.session.status_message.clone_from(text);
            }
            CoreEvent::Ready { .. } => {}
            CoreEvent::Shutdown { reason } => {
                emit(&format!("🛑 the core service stopped: {reason}"));
                self.stop = true;
            }
        }

        self.stop
    }

    /// Render the session status block (`/info`).
    fn show_info(&self) {
        let s = &self.session;
        emit("");
        emit("════════════════ metaText status ════════════════");
        emit(&format!("  nickname        : {}", s.nickname));
        emit(&format!("  status          : {}", s.status_message));
        emit(&format!("  identity (DID)  : {}", s.identity));
        emit(&format!(
            "  encryption      : {} ({})",
            if s.encryption_enabled {
                "enabled"
            } else {
                "disabled"
            },
            s.encryption
        ));
        emit(&format!("  mode            : {}", s.mode));
        emit(&format!(
            "  network         : {} (port {}, {} peer(s) connected, max {} conn)",
            if s.network_running {
                "running"
            } else {
                "stopped"
            },
            s.network_port,
            s.connected_peers,
            s.max_connections
        ));
        emit(&format!(
            "  database        : {} ({}, max {} conn)",
            if s.database_connected {
                "connected"
            } else {
                "disconnected"
            },
            s.database,
            s.database_max_connections
        ));
        emit(&format!("  friends         : {}", s.friend_count));
        emit(&format!(
            "  active chat     : {}",
            s.active_chat.as_deref().unwrap_or("none")
        ));
        emit(&format!(
            "  messages sent   : {}",
            self.statistics.messages_sent
        ));
        emit(&format!(
            "  messages received: {}",
            self.statistics.messages_received
        ));
        emit(&format!("  config file     : {}", s.config_path));
        emit(&format!("  data directory  : {}", s.data_dir));
        emit(&format!(
            "  app             : {} v{}",
            s.app_name, s.app_version
        ));
        emit("═════════════════════════════════════════════════");
        emit("");
    }

    /// Render the identity block (`/whoami`).
    fn show_whoami(&self) {
        let s = &self.session;
        let address = s.local_address.as_deref().unwrap_or("not listening");
        emit("");
        emit("metaText identity");
        emit(&format!("  nickname : {}", s.nickname));
        emit(&format!("  status   : {}", s.status_message));
        emit(&format!("  DID      : {}", s.identity));
        emit(&format!("  address  : {address}"));
        emit("");
    }

    /// Render the version line (`/version`).
    fn show_version(&self) {
        emit(&format!(
            "metaText v{} (encryption: {})",
            self.session.version, self.session.encryption
        ));
    }

    /// Render the connected peers (`/peers`).
    fn show_peers(&self) {
        let s = &self.session;
        let local = s.local_address.as_deref().unwrap_or("not listening");

        emit("");
        emit(&format!("Local address: {local}"));

        if s.queued_messages > 0 {
            emit(&format!(
                "{} message(s) waiting to be delivered",
                s.queued_messages
            ));
        }

        if s.connected_peers == 0 {
            emit("No peers connected yet.");
            if s.pending_peers > 0 {
                emit(&format!(
                    "{} address(es) are being retried in the background:",
                    s.pending_peers
                ));
                for address in &s.desired_peers {
                    emit(&format!("  - {address}"));
                }
            } else {
                emit("Start another instance with the same --passphrase and point it");
                emit("here using --peer <host:port> (or /connect).");
            }
        } else {
            emit(&format!("{} peer(s) connected:", s.connected_peers));
            for (index, name) in s.peer_nicknames.iter().enumerate() {
                emit(&format!("  {}. {name}", index + 1));
            }
            let unnamed = s.connected_peers.saturating_sub(s.peer_nicknames.len());
            if unnamed > 0 {
                emit(&format!("  (+{unnamed} still completing the handshake)"));
            }
        }
        emit("");
    }

    /// Render the friend list (`/list`).
    fn show_contacts(&self) {
        emit("");
        if self.contacts.is_empty() {
            emit("You have no friends yet. Use /add <DID_Address> to invite one.");
            emit("");
            return;
        }

        let active = self.session.active_chat.as_deref();
        emit("Friends:");
        for (index, contact) in self.contacts.iter().enumerate() {
            let note = contact
                .note
                .as_ref()
                .map_or(String::new(), |n| format!("  // {n}"));
            let marker = if active == Some(contact.name.as_str()) {
                "  ← active chat"
            } else {
                ""
            };
            emit(&format!(
                "  {}. {} [{}]{}{}",
                index + 1,
                contact.name,
                contact.status,
                note,
                marker
            ));
        }
        emit("");
    }

    /// Render the runtime counters (`/stats`).
    async fn show_statistics(&mut self) {
        self.refresh().await;
        let c = self.statistics;

        emit("");
        emit("Runtime statistics:");
        emit(&format!("  uptime            : {}s", c.uptime_seconds));
        emit(&format!("  messages sent     : {}", c.messages_sent));
        emit(&format!("  messages received : {}", c.messages_received));
        emit(&format!("  active connections: {}", c.active_connections));
        emit(&format!("  bytes sent        : {}", c.bytes_sent));
        emit(&format!("  bytes received    : {}", c.bytes_received));
        emit("");
    }

    /// Render the stored message history (`/history`).
    async fn show_history(&mut self, limit: Option<usize>) {
        let reply = self.core.request(Request::History { limit }).await;

        let (persistent, messages) = match reply {
            Ok(Reply::History {
                persistent,
                messages,
            }) => (persistent, messages),
            Ok(_) => (false, Vec::new()),
            Err(error) => {
                emit(&format!("⚠️ could not read message history: {error}"));
                return;
            }
        };

        if !persistent {
            emit("ℹ️ message history is not available in this build.");
            emit("   Rebuild with `--features sqlite` to persist messages.");
            return;
        }

        if messages.is_empty() {
            emit("ℹ️ no stored messages yet.");
            return;
        }

        emit("");
        emit(&format!("Last {} stored message(s):", messages.len()));
        for message in &messages {
            let arrow = if message.direction == "out" {
                "→"
            } else {
                "←"
            };
            emit(&format!(
                "  [{}] {arrow} {}: {} ({} B)",
                format_timestamp(&message.created_at),
                message.peer,
                message.body,
                message.wire_bytes
            ));
        }
        emit("");
    }

    /// Build the snapshot rendered by the full screen header and side panels.
    #[must_use]
    pub fn tui_info(&self) -> TuiInfo {
        let s = &self.session;
        TuiInfo {
            nickname: s.nickname.clone(),
            status_message: s.status_message.clone(),
            identity: s.identity.clone(),
            mode: s.mode.clone(),
            network: format!(
                "{} (port {}, {} bootstrap, max {} conn)",
                if s.network_running {
                    "running"
                } else {
                    "stopped"
                },
                s.network_port,
                s.network_bootstrap_nodes,
                s.max_connections
            ),
            database: format!(
                "{} ({})",
                if s.database_connected {
                    "connected"
                } else {
                    "disconnected"
                },
                s.database
            ),
            encryption: format!(
                "{} ({})",
                if s.encryption_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                s.encryption
            ),
            friends: self.contacts.iter().map(|c| c.name.clone()).collect(),
            peers: s.peer_nicknames.clone(),
            pending_peers: s.pending_peers,
            active_chat: s.active_chat.clone(),
            messages_sent: self.statistics.messages_sent,
            messages_received: self.statistics.messages_received,
            bytes_sent: self.statistics.bytes_sent,
            bytes_received: self.statistics.bytes_received,
        }
    }

    /// Change (or report) the nickname.
    async fn set_nickname(&mut self, nickname: String) {
        let reporting = nickname.trim().is_empty();
        match self.core.request(Request::SetNickname { nickname }).await {
            Ok(Reply::Updated { detail, .. }) => {
                if reporting {
                    emit(&format!("nickname: {detail}"));
                } else {
                    emit(&format!("✅ nickname set to '{detail}'"));
                }
                self.session.nickname = detail;
            }
            Ok(_) => {}
            Err(error) => emit(&format!("❌ could not change the nickname: {error}")),
        }
    }

    /// Change (or report) the status message.
    async fn set_status(&mut self, text: String) {
        let reporting = text.trim().is_empty();
        match self.core.request(Request::SetStatus { text }).await {
            Ok(Reply::Updated { detail, .. }) => {
                if reporting {
                    emit(&format!("status: {detail}"));
                } else {
                    emit(&format!("✅ status set to '{detail}'"));
                }
                self.session.status_message = detail;
            }
            Ok(_) => {}
            Err(error) => emit(&format!("❌ could not change the status: {error}")),
        }
    }

    /// Add a friend.
    async fn add_contact(&mut self, identifier: String, note: Option<String>) {
        if identifier.trim().is_empty() {
            emit("Usage: /add <DID_Address> [note]");
            return;
        }

        match self
            .core
            .request(Request::AddContact { identifier, note })
            .await
        {
            Ok(Reply::ContactAdded { index, identifier }) => {
                emit(&format!("✅ added friend #{index} : {identifier}"));
            }
            Ok(Reply::ContactExists { identifier }) => {
                emit(&format!("ℹ️ contact '{identifier}' already exists"));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// Select (or report) the active conversation.
    async fn select_conversation(&mut self, target: &str) {
        let trimmed = target.trim();

        // Reject friend number 0 before asking the core, so the hint text can
        // mention `/list` without the core needing presentation knowledge.
        if trimmed.parse::<usize>() == Ok(0) {
            emit("⚠️ friend numbers start at 1. Use /list to see them.");
            return;
        }

        match self
            .core
            .request(Request::SelectConversation {
                target: trimmed.to_string(),
            })
            .await
        {
            Ok(Reply::Conversation { active, index }) => match (active, index) {
                (Some(name), Some(index)) => {
                    if trimmed.is_empty() {
                        emit(&format!("💬 active conversation: #{index} {name}"));
                    } else {
                        emit(&format!("💬 now chatting with #{index} {name}"));
                    }
                    self.session.active_chat = Some(name);
                }
                _ => emit("ℹ️ no active conversation. Use /chat <index|name> to pick one."),
            },
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::NotFound => {
                emit(&format!(
                    "❌ no friend matches '{trimmed}'. Use /list to see them."
                ));
            }
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// Dial a peer address.
    async fn connect(&mut self, target: &str) {
        let address = target.trim();
        if address.is_empty() {
            emit("Usage: /connect <host:port>");
            return;
        }

        match self
            .core
            .request(Request::Connect {
                address: address.to_string(),
            })
            .await
        {
            Ok(Reply::Updated { detail, .. }) => {
                emit(&format!("🔗 connected to peer {detail}"));
            }
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::InvalidRequest => {
                emit(&format!("⚠️ {}", error.message));
            }
            Err(error) => emit(&format!("❌ {}", error.message)),
        }
    }

    /// Encrypt and send a message, then report what the transport did with it.
    async fn send_message(&mut self, target: Option<String>, text: &str) {
        let body = text.trim();

        if target.as_deref().is_some_and(|t| t.trim().is_empty()) || body.is_empty() {
            emit("Usage: /msg <name> <text>");
            return;
        }

        // An unprefixed message needs an active conversation; report that
        // without troubling the core.
        if target.is_none() && self.session.active_chat.is_none() {
            emit("ℹ️ no active conversation. Use /chat <index> first.");
            return;
        }

        match self
            .core
            .request(Request::SendMessage {
                target: target.clone(),
                text: body.to_string(),
            })
            .await
        {
            Ok(Reply::Sent { target, report }) => render_send(&target, body, &report),
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::NotFound => {
                emit("ℹ️ no active conversation. Use /chat <index> first.");
            }
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// Remove a friend.
    async fn remove_contact(&mut self, target: &str) {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            emit("Usage: /remove <index|name>");
            return;
        }

        match self
            .core
            .request(Request::RemoveContact {
                target: trimmed.to_string(),
            })
            .await
        {
            Ok(Reply::ContactRemoved { index, name }) => {
                emit(&format!("🗑️ removed friend #{index} {name}"));
            }
            Ok(_) => {}
            Err(error) if error.code == ErrorCode::NotFound => {
                emit(&format!(
                    "❌ no friend matches '{trimmed}'. Use /list to see them."
                ));
            }
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// Persist the session (`/save`).
    async fn save_session(&mut self) {
        match self.core.request(Request::SaveSession).await {
            Ok(Reply::Saved { path }) => emit(&format!("💾 session saved to {path}")),
            Ok(_) => emit("💾 session saved"),
            Err(error) => emit(&format!("❌ could not save session: {error}")),
        }
    }
}

/// Render a delivery report for one outgoing message.
fn render_send(target: &str, text: &str, report: &crate::ipc::protocol::SendReport) {
    let display = if report.encrypted {
        let preview: String = report.ciphertext_preview.chars().take(16).collect();
        format!("{} bytes ciphertext [{preview}…]", report.wire_bytes)
    } else {
        "encryption disabled, sent in clear text".to_string()
    };

    match report.outcome {
        SendOutcomeKind::Sent => {
            let peers = match report.queued_for {
                0 => "no peers connected".to_string(),
                1 => "1 peer".to_string(),
                count => format!("{count} peers"),
            };
            let id = report.message_id.unwrap_or_default();
            emit(&format!(
                "📤 → {target}: {text}  ({display}; queued for {peers}, id #{id})"
            ));
        }
        SendOutcomeKind::Queued => {
            let id = report.message_id.unwrap_or_default();
            let position = report.queue_position.unwrap_or_default();
            emit(&format!(
                "📨 → {target}: {text}  ({display}; {target} is offline, buffered as #{position}, id #{id})"
            ));
            emit(&format!(
                "   it will be delivered automatically once {target} connects"
            ));
        }
        SendOutcomeKind::Dropped => {
            emit(&format!(
                "⚠️ the outbox for {target} is full; the message was not sent"
            ));
        }
    }
}

/// Render an RFC 3339 timestamp the way the REPL always has.
fn format_timestamp(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_string(),
        |stamp| stamp.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}
