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
    ContactView, ContentType, CoreEvent, ErrorCode, GroupView, MessageKind, MessageView, Reply,
    Request, SendOutcomeKind, SessionInfo, StatisticsView,
};
use crate::ipc::CoreClient;
use crate::tui::{clear_output, write_line, TuiInfo};
use crate::utils::abbreviate;

use super::text;

/// Write one line to whichever output stream currently owns the terminal.
fn emit(line: &str) {
    write_line(line);
}

/// The name to show for a group: its title, or its abbreviated id when unnamed.
fn display_group_name(group: &GroupView) -> String {
    if group.name.is_empty() {
        group.short_id.clone()
    } else {
        group.name.clone()
    }
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

    /// Group this front-end addresses when a `/group` command omits `@group`.
    ///
    /// Kept in the front-end: the core resolves an empty selector itself, so the
    /// two never disagree about which group is "current" for a request, and this
    /// field only decides what the *user* sees as selected.
    active_group: Option<String>,

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
            active_group: None,
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
            Command::Metrics => self.show_metrics().await,
            Command::History(limit) => self.show_history(limit).await,
            Command::Clear => clear_output(),
            Command::Nick(name) => self.set_nickname(name).await,
            Command::Status(text) => self.set_status(text).await,
            Command::Add { identifier, note } => self.add_contact(identifier, note).await,
            Command::Chat(target) => self.select_conversation(&target).await,
            Command::Connect(target) => self.connect(&target).await,
            Command::Accept(target) => self.accept_request(&target).await,
            Command::Reject(target) => self.reject_request(&target).await,
            Command::Requests => self.show_requests().await,
            Command::Message(text) => {
                self.send_message(None, &text, MessageKind::Text, ContentType::Text)
                    .await;
            }
            Command::Action(text) => {
                if text.trim().is_empty() {
                    emit("Usage: /me <action>");
                } else {
                    self.send_message(None, &text, MessageKind::Action, ContentType::Text)
                        .await;
                }
            }
            Command::Binary(text) => {
                if text.trim().is_empty() {
                    emit("Usage: /bin <hexadecimal>");
                } else {
                    self.send_message(None, &text, MessageKind::Text, ContentType::Binary)
                        .await;
                }
            }
            Command::Group {
                subcommand,
                arguments,
            } => self.run_group_command(&subcommand, &arguments).await,
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
            Command::Msg { target, text } => {
                self.send_message(Some(target), &text, MessageKind::Text, ContentType::Text)
                    .await;
            }
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

    /// Dispatch one `/group` (or `/g`) command.
    ///
    /// The subcommand vocabulary is deliberately explicit rather than a free-form
    /// group name, so `/group send` can be distinguished from a group that happens
    /// to be called "send".
    async fn run_group_command(&mut self, subcommand: &str, arguments: &str) {
        match subcommand {
            "" | "list" | "ls" => self.list_groups().await,
            "invites" | "invitations" => self.list_group_invites().await,
            "create" => self.create_group(arguments).await,
            "rename" | "title" => {
                let (group, name) = Self::split_group_and_rest(arguments);
                self.rename_group(group, name).await;
            }
            "join" | "accept" => self.join_group(arguments).await,
            "decline" | "reject" => self.decline_group_invite(arguments).await,
            "invite" => {
                let (group, peer) = Self::split_group_and_rest(arguments);
                self.invite_to_group(group, peer).await;
            }
            "send" | "msg" | "say" => {
                let (group, text) = Self::split_group_and_rest(arguments);
                self.send_group_message(group, text, MessageKind::Text, ContentType::Text)
                    .await;
            }
            "me" | "action" => {
                let (group, text) = Self::split_group_and_rest(arguments);
                self.send_group_message(group, text, MessageKind::Action, ContentType::Text)
                    .await;
            }
            "bin" => {
                let (group, text) = Self::split_group_and_rest(arguments);
                self.send_group_message(group, text, MessageKind::Text, ContentType::Binary)
                    .await;
            }
            "leave" | "quit" => self.leave_group(arguments).await,
            "select" | "chat" => {
                // Selecting just filters what `/history` and a bare `/group send`
                // use, so the answer comes from the core's own list.
                let wanted = arguments.trim();
                if wanted.is_empty() {
                    emit("Usage: /group select <id|name|index>");
                    return;
                }
                self.active_group = Some(wanted.to_string());
                emit(&format!("👥 active group set to {wanted}"));
            }
            other => emit(&format!(
                "❓ unknown /group subcommand '/{other}'. Type /help /group."
            )),
        }
    }

    /// Split `[@group] <rest>` for the group commands.
    ///
    /// A group is addressed with a leading `@`, so a message can never be mistaken
    /// for a group name: `/group send @Team hello` addresses `Team` while
    /// `/group send hello there` goes to the active group. Without the marker the
    /// two would be indistinguishable, and guessing would silently misroute a
    /// message.
    fn split_group_and_rest(arguments: &str) -> (Option<String>, &str) {
        let trimmed = arguments.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            return (Some(head.to_string()), tail.trim());
        }
        (None, trimmed)
    }

    /// Render a [`CoreEvent`] pushed by the core.
    ///
    /// # Returns
    ///
    /// Returns `true` when the event requests application shutdown.
    ///
    /// One arm per event, and each arm renders through the same helpers the
    /// request path uses, so a new event is a compile error here rather than a
    /// silently ignored notification.
    #[allow(clippy::too_many_lines)]
    pub async fn handle_event(&mut self, event: &CoreEvent) -> bool {
        match event {
            CoreEvent::MessageReceived {
                peer,
                body,
                kind,
                content_type,
                ..
            } => {
                match (kind, content_type) {
                    (_, ContentType::Binary) => {
                        emit(&format!("📥 {peer}: {}", describe_binary(body)));
                    }
                    (MessageKind::Action, _) => emit(&format!("* {peer} {body}")),
                    (MessageKind::Text, ContentType::Text) => emit(&format!("📥 {peer}: {body}")),
                }
                self.refresh().await;
            }
            CoreEvent::MessageUndecodable {
                peer,
                wire_bytes,
                reason,
                group,
                ..
            } => {
                // The core decided this payload cannot be shown; the wording says which
                // rule it broke and, for a group message, where it arrived. Nothing of
                // the payload is printed — that is the point of the event.
                let destination = group
                    .as_deref()
                    .map_or_else(String::new, |name| format!(" in group '{name}'"));
                emit(&format!(
                    "⚠️ {peer} sent {wire_bytes} bytes{destination} that {reason}; \
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
            CoreEvent::PeerRequestReceived {
                public_key,
                short_key,
                message,
            } => {
                if message.is_empty() {
                    emit(&format!("📨 friend request from {short_key}"));
                } else {
                    emit(&format!("📨 friend request from {short_key}: {message}"));
                }
                emit(&format!("   accept it with /accept {public_key}"));
                emit(&format!("   or refuse it with /reject {public_key}"));
                self.refresh().await;
            }
            CoreEvent::GroupMessageReceived {
                group_id,
                group,
                peer,
                body,
                kind,
                content_type,
                ..
            } => {
                let label = if group.is_empty() {
                    abbreviate(group_id)
                } else {
                    group.clone()
                };
                match (kind, content_type) {
                    (_, ContentType::Binary) => {
                        emit(&format!("👥 [{label}] {peer}: {}", describe_binary(body)));
                    }
                    (MessageKind::Action, _) => emit(&format!("👥 [{label}] * {peer} {body}")),
                    (MessageKind::Text, ContentType::Text) => {
                        emit(&format!("👥 [{label}] {peer}: {body}"));
                    }
                }
                self.refresh().await;
            }
            CoreEvent::GroupChanged {
                group_id,
                group,
                members,
                joined,
            } => {
                let label = if group.is_empty() {
                    abbreviate(group_id)
                } else {
                    group.clone()
                };
                match (joined, members) {
                    // A freshly joined group has no peers but ourselves yet.
                    (true, 0) => emit(&format!("👥 joined group {label}")),
                    (true, count) => emit(&format!("👥 group {label} now has {count} peer(s)")),
                    // toxcore reports the event before the handshake completes.
                    (false, _) => emit(&format!("👥 group {label} is connecting")),
                }
                self.refresh().await;
            }
            CoreEvent::GroupInviteReceived { peer, token, .. } => {
                let who = if peer.is_empty() { "a peer" } else { peer };
                emit(&format!("👥 {who} invited you to a group"));
                emit("   join it with /group join <token> using:");
                emit(&format!("   {token}"));
                self.refresh().await;
            }
            CoreEvent::Ready { .. } => {}
            CoreEvent::Shutdown { reason } => {
                emit(&format!("🛑 the core service stopped: {reason}"));
                self.stop = true;
            }
        }

        self.stop
    }

    /// List the groups this session is in (`/group`).
    ///
    /// A transport without groups answers `supported: false`, which is reported as
    /// such instead of as an error: "there are none" and "this transport has no
    /// groups" are different facts.
    async fn list_groups(&self) {
        match self.core.request(Request::Groups).await {
            Ok(Reply::Groups { supported, groups }) => {
                if !supported {
                    emit("ℹ️ the tcp transport has no groups. Start with --transport tox.");
                    return;
                }
                if groups.is_empty() {
                    emit("ℹ️ no groups. Create one with /group create <name>.");
                    return;
                }
                emit("");
                emit(&format!("👥 {} group(s):", groups.len()));
                for (index, group) in groups.iter().enumerate() {
                    let status = if group.joined { "joined" } else { "connecting" };
                    let name = if group.name.is_empty() {
                        group.short_id.clone()
                    } else {
                        group.name.clone()
                    };
                    let active = self
                        .active_group
                        .as_deref()
                        .is_some_and(|active| active.eq_ignore_ascii_case(&group.id));
                    emit(&format!(
                        "  {}. {name} [{status}, {} peer(s)]{}",
                        index + 1,
                        group.members,
                        if active { " ← active" } else { "" }
                    ));
                }
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
    }

    /// List the group invitations waiting for an answer (`/group invites`).
    ///
    /// An invitation also arrives as a [`CoreEvent::GroupInviteReceived`] event,
    /// but an event is only seen live; asking for the list is what lets a session
    /// that attached after the invitation still join it. The token is printed in
    /// full and on its own line, because it *is* the capability — truncating it
    /// would make the command useless.
    async fn list_group_invites(&self) {
        match self.core.request(Request::GroupInvites).await {
            Ok(Reply::GroupInvites { supported, invites }) => {
                if !supported {
                    emit("ℹ️ the tcp transport has no groups. Start with --transport tox.");
                    return;
                }
                if invites.is_empty() {
                    emit("ℹ️ no group invitations waiting.");
                    return;
                }
                emit("");
                emit(&format!("👥 {} group invitation(s):", invites.len()));
                for (index, invite) in invites.iter().enumerate() {
                    let who = if invite.peer.is_empty() {
                        abbreviate(&invite.peer_id)
                    } else {
                        invite.peer.clone()
                    };
                    emit(&format!("  {}. {who}", index + 1));
                    emit(&format!("     /group join {}", invite.token));
                }
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
    }

    /// Create a group (`/group create <name>`).
    async fn create_group(&mut self, title: &str) {
        match self
            .core
            .request(Request::CreateGroup {
                title: Some(title.trim().to_string()),
            })
            .await
        {
            Ok(Reply::GroupCreated { group }) => {
                let name = display_group_name(&group);
                emit(&format!("👥 created group {name}"));
                emit(&format!(
                    "   invite a peer with /group invite {name} <peer>"
                ));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        self.refresh().await;
    }

    /// Rename a group (`/group rename [@group] <name>`).
    ///
    /// The name is shared: the transport forwards it to the other participants,
    /// and their front-ends see it as a `GroupChanged` event.
    async fn rename_group(&mut self, group: Option<String>, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            emit("Usage: /group rename [@group] <name>");
            return;
        }
        match self
            .core
            .request(Request::RenameGroup {
                group_id: group,
                name: name.to_string(),
            })
            .await
        {
            Ok(Reply::GroupRenamed { group_id, group }) => {
                emit(&format!(
                    "👥 renamed group {} to {group}",
                    abbreviate(&group_id)
                ));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        self.refresh().await;
    }

    /// Join a group from an invitation token (`/group join <token>`).
    async fn join_group(&mut self, token: &str) {
        match self
            .core
            .request(Request::JoinGroup {
                token: token.trim().to_string(),
            })
            .await
        {
            Ok(Reply::GroupJoined { group }) => {
                emit(&format!("👥 joined group {}", display_group_name(&group)));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        self.refresh().await;
    }

    /// Discard a group invitation without joining (`/group decline <token>`).
    ///
    /// The counterpart of [`Self::join_group`]: an invitation is a capability
    /// handed to us and the list that holds them is bounded, so a user must be
    /// able to discard one that is not wanted — otherwise the only way to make
    /// room was to join the conference and leave it again.
    async fn decline_group_invite(&mut self, token: &str) {
        let token = token.trim();
        if token.is_empty() {
            // The token *is* the argument; without it there is nothing to name.
            emit("Usage: /group decline <token> (see /group invites)");
            return;
        }

        match self
            .core
            .request(Request::DeclineGroupInvite {
                token: token.to_string(),
            })
            .await
        {
            Ok(Reply::GroupInviteDeclined { token }) => {
                emit(&format!(
                    "🚫 discarded the group invitation {}",
                    abbreviate(&token)
                ));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        // The pending-invitation count lives in the session snapshot, so the
        // status block and `/info` have to be refreshed like after `/reject`.
        self.refresh().await;
    }

    /// Invite a peer to a group (`/group invite [@group] <peer>`).
    async fn invite_to_group(&mut self, group: Option<String>, peer: &str) {
        if peer.trim().is_empty() {
            emit("Usage: /group invite [@group] <peer>");
            return;
        }
        match self
            .core
            .request(Request::InviteToGroup {
                group_id: group.unwrap_or_default(),
                peer: peer.trim().to_string(),
            })
            .await
        {
            Ok(Reply::GroupInvited { peer, .. }) => {
                emit(&format!("👥 invited {peer} to the group"));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        self.refresh().await;
    }

    /// Send a message to a group (`/group send|me|bin [@group] <text>`).
    async fn send_group_message(
        &self,
        group: Option<String>,
        text: &str,
        kind: MessageKind,
        content_type: ContentType,
    ) {
        if text.trim().is_empty() {
            emit("Usage: /group send [@group] <text>");
            return;
        }
        match self
            .core
            .request(Request::SendGroupMessage {
                group_id: group,
                text: text.trim().to_string(),
                kind,
                content_type,
            })
            .await
        {
            Ok(Reply::GroupSent {
                group, wire_bytes, ..
            }) => {
                let shown = match (kind, content_type) {
                    (MessageKind::Action, _) => {
                        let who = if self.session.nickname.is_empty() {
                            "you"
                        } else {
                            self.session.nickname.as_str()
                        };
                        format!("* {who} {text}")
                    }
                    (_, ContentType::Binary) => describe_binary(text.trim()),
                    _ => text.trim().to_string(),
                };
                emit(&format!("👥 → [{group}] {shown} ({wire_bytes} B)"));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
    }

    /// Leave a group (`/group leave [@group]`).
    async fn leave_group(&mut self, selector: &str) {
        let selector = selector.trim().trim_start_matches('@');
        match self
            .core
            .request(Request::LeaveGroup {
                group_id: Some(selector.to_string()),
            })
            .await
        {
            Ok(Reply::GroupLeft { group_id }) => {
                emit(&format!("👥 left group {}", abbreviate(&group_id)));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
        self.refresh().await;
    }

    /// Render the operational counters (`/metrics`).
    ///
    /// Deliberately one `key=value` per line so the output can be read by a
    /// person *and* scraped by a script (`/metrics | grep ...`).
    async fn show_metrics(&self) {
        match self.core.request(Request::Metrics).await {
            Ok(Reply::Metrics { metrics }) => {
                let m = &metrics;
                emit("");
                emit("metaText metrics");
                emit(&format!("  uptime_seconds={}", m.uptime_seconds));
                emit(&format!("  transport={}", m.transport));
                emit(&format!("  peers_connected={}", m.peers_connected));
                emit(&format!("  peers_pending={}", m.peers_pending));
                emit(&format!("  payloads_queued={}", m.payloads_queued));
                emit(&format!("  payloads_expired={}", m.payloads_expired));
                emit(&format!("  payloads_dropped={}", m.payloads_dropped));
                emit(&format!("  friend_count={}", m.friend_count));
                emit(&format!("  max_friends={}", m.max_friends));
                emit(&format!(
                    "  friend_requests_pending={}",
                    m.friend_requests_pending
                ));
                emit(&format!(
                    "  friend_requests_dropped={}",
                    m.friend_requests_dropped
                ));
                emit(&format!(
                    "  group_invites_dropped={}",
                    m.group_invites_dropped
                ));
                emit(&format!(
                    "  peer_identities_changed={}",
                    m.peer_identities_changed
                ));
                emit(&format!(
                    "  peer_identities_refused={}",
                    m.peer_identities_refused
                ));
                emit(&format!("  greetings_refused={}", m.greetings_refused));
                emit(&format!("  groups={}", m.groups));
                emit(&format!("  groups_supported={}", m.groups_supported));
                emit(&format!("  request_queue_depth={}", m.request_queue_depth));
                emit(&format!(
                    "  request_queue_capacity={}",
                    m.request_queue_capacity
                ));
                emit(&format!("  event_subscribers={}", m.event_subscribers));
                emit(&format!(
                    "  request_wait_last_us={}",
                    m.request_wait_last_us
                ));
                emit(&format!("  request_wait_max_us={}", m.request_wait_max_us));
                emit(&format!(
                    "  request_service_last_us={}",
                    m.request_service_last_us
                ));
                emit(&format!(
                    "  request_service_max_us={}",
                    m.request_service_max_us
                ));
                emit(&format!("  requests_served={}", m.requests_served));
                emit(&format!("  messages_sent={}", m.messages_sent));
                emit(&format!("  messages_received={}", m.messages_received));
                emit(&format!("  bytes_sent={}", m.bytes_sent));
                emit(&format!("  bytes_received={}", m.bytes_received));
                emit(&format!("  database_connected={}", m.database_connected));
                emit(&format!("  persistent={}", m.persistent));
                emit("");
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ could not read metrics: {error}")),
        }
    }

    /// Render the session status block (`/info`).
    fn show_info(&self) {
        let s = &self.session;
        emit("");
        emit("════════════════ metaText status ════════════════");
        emit(&format!("  nickname        : {}", s.nickname));
        emit(&format!("  status          : {}", s.status_message));
        emit(&format!("  identity        : {}", s.identity));
        if !s.identity_fingerprint.is_empty() {
            emit(&format!("  fingerprint     : {}", s.identity_fingerprint));
        }
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
        emit(&format!("  transport       : {}", s.transport));
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
        emit(&format!("  transport: {}", s.transport));
        emit(&format!("  identity : {}", s.identity));
        if !s.identity_fingerprint.is_empty() {
            emit(&format!("  fingerprint: {}", s.identity_fingerprint));
            emit("             read it out to a peer and compare; the same pair must see");
            emit("             the same two fingerprints every run");
        }
        emit(&format!("  address  : {address}"));
        if let Some(tox) = &s.public_identity {
            if s.local_address.as_deref() != Some(tox.as_str()) {
                emit(&format!("  Tox addr : {tox}"));
            }
            emit("             share the Tox address with a friend so they can /add you");
        }
        if s.pending_requests > 0 {
            emit(&format!(
                "  pending  : {} friend request(s); see /requests",
                s.pending_requests
            ));
        }
        if s.supports_groups {
            // "N groups, M pending" rather than "N joined": a group is listed as
            // soon as it exists, and only completes its handshake later, so the
            // count alone does not mean every one of them is reachable. The count is
            // the transport's own list — the core refreshes it before answering this
            // snapshot — so a conference toxcore rejoined after a restart is included
            // without the session having asked for it.
            emit(&format!(
                "  groups   : {}{}",
                s.groups,
                if s.pending_group_invites > 0 {
                    format!(
                        ", {} invitation(s) pending; see /group invites",
                        s.pending_group_invites
                    )
                } else {
                    String::new()
                }
            ));
        } else {
            emit("  groups   : not supported by this transport (use --transport tox)");
        }
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

        // The identities that have been pinned, whether or not the peer is connected
        // right now: this is what a user compares with their peer out of band, and a
        // peer that announced a different identity than last run is flagged.
        if !s.peer_identities.is_empty() {
            emit("");
            emit(&format!(
                "{} pinned identit(ies) (compare a fingerprint with the peer out of band):",
                s.peer_identities.len()
            ));
            for (index, peer) in s.peer_identities.iter().enumerate() {
                let marker = if peer.changed {
                    " ⚠️ changed since it was pinned"
                } else {
                    ""
                };
                emit(&format!(
                    "  {}. {} — {}{marker}",
                    index + 1,
                    peer.nickname,
                    peer.fingerprint
                ));
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
    async fn show_history(&self, limit: Option<usize>) {
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
            let body = history_body(message, &self.session.nickname);
            emit(&format!(
                "  [{}] {arrow} {}: {} ({} B)",
                format_timestamp(&message.created_at),
                message.peer,
                body,
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
            app_name: s.app_name.clone(),
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
    async fn connect(&self, target: &str) {
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

    /// Resolve a pending-request target: a 1-based index or a public key.
    ///
    /// A 1-based index is resolved against the pending list so the user can answer
    /// the request they just saw without copying a 64 character key. Emits the
    /// reason and returns `None` when it cannot be resolved.
    ///
    /// The caller checks [`SessionInfo::supports_friend_requests`] first: on a
    /// transport without them, "no pending request #1" would be the wrong answer —
    /// the honest one is that the feature does not exist here.
    ///
    /// [`SessionInfo::supports_friend_requests`]: crate::ipc::protocol::SessionInfo::supports_friend_requests
    async fn resolve_pending_request(&self, target: &str) -> Option<String> {
        let trimmed = target.trim();
        let Ok(index) = trimmed.parse::<usize>() else {
            return Some(trimmed.to_string());
        };
        if let Ok(Reply::PeerRequests { requests }) = self.core.request(Request::PeerRequests).await
        {
            let Some(request) = index
                .checked_sub(1)
                .and_then(|position| requests.get(position))
            else {
                emit(&format!("❌ no pending request #{index}; use /requests"));
                return None;
            };
            Some(request.public_key.clone())
        } else {
            emit("❌ could not read the pending friend requests");
            None
        }
    }

    /// Accept a pending friend request by 1-based index or public key.
    async fn accept_request(&mut self, target: &str) {
        if target.trim().is_empty() {
            emit("Usage: /accept <index|public key>  (see /requests)");
            return;
        }
        if !self.session.supports_friend_requests {
            emit(NO_FRIEND_REQUESTS);
            return;
        }
        let Some(public_key) = self.resolve_pending_request(target).await else {
            return;
        };

        match self
            .core
            .request(Request::AcceptPeerRequest { public_key })
            .await
        {
            Ok(Reply::PeerRequestAccepted { public_key }) => {
                emit(&format!("🤝 accepted friend request from {public_key}"));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// Reject a pending friend request by 1-based index or public key.
    ///
    /// The requester is not notified: a request becomes a friendship only when it
    /// is accepted, so dropping it locally is the whole decision. Doing so also
    /// frees a slot in the bounded pending list.
    async fn reject_request(&mut self, target: &str) {
        if target.trim().is_empty() {
            emit("Usage: /reject <index|public key>  (see /requests)");
            return;
        }
        if !self.session.supports_friend_requests {
            emit(NO_FRIEND_REQUESTS);
            return;
        }
        let Some(public_key) = self.resolve_pending_request(target).await else {
            return;
        };

        match self
            .core
            .request(Request::RejectPeerRequest { public_key })
            .await
        {
            Ok(Reply::PeerRequestRejected { public_key }) => {
                emit(&format!(
                    "🚫 rejected friend request from {}",
                    abbreviate(&public_key)
                ));
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }

        self.refresh().await;
    }

    /// List the pending friend requests (`/requests`).
    async fn show_requests(&self) {
        if !self.session.supports_friend_requests {
            emit(NO_FRIEND_REQUESTS);
            return;
        }

        match self.core.request(Request::PeerRequests).await {
            Ok(Reply::PeerRequests { requests }) => {
                if requests.is_empty() {
                    emit("No pending friend requests.");
                    return;
                }
                emit("");
                emit(&format!("{} pending friend request(s):", requests.len()));
                for (index, request) in requests.iter().enumerate() {
                    if request.message.is_empty() {
                        emit(&format!("  {}. {}", index + 1, request.short_key));
                    } else {
                        emit(&format!(
                            "  {}. {} — {}",
                            index + 1,
                            request.short_key,
                            request.message
                        ));
                    }
                }
                emit("");
                emit("Answer one with /accept <index|public key> (become friends)");
                emit("or refuse it with /reject <index|public key>. The list is bounded,");
                emit("so refusing is how room is made for the requests you do want.");
            }
            Ok(_) => {}
            Err(error) => emit(&format!("⚠️ {}", error.message)),
        }
    }

    /// Encrypt and send a message, then report what the transport did with it.
    async fn send_message(
        &mut self,
        target: Option<String>,
        text: &str,
        kind: MessageKind,
        content_type: ContentType,
    ) {
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
                kind,
                content_type,
            })
            .await
        {
            Ok(Reply::Sent { target, report }) => {
                let nickname = self.session.nickname.clone();
                render_send(&target, body, &report, kind, content_type, &nickname);
            }
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
    async fn save_session(&self) {
        match self.core.request(Request::SaveSession).await {
            Ok(Reply::Saved { path }) => emit(&format!("💾 session saved to {path}")),
            Ok(_) => emit("💾 session saved"),
            Err(error) => emit(&format!("❌ could not save session: {error}")),
        }
    }
}

/// Render a delivery report for one outgoing message.
fn render_send(
    target: &str,
    text: &str,
    report: &crate::ipc::protocol::SendReport,
    kind: MessageKind,
    content_type: ContentType,
    sender: &str,
) {
    // An action is shown the way the peer will render it (`* <sender> <action>`),
    // so the sender can tell an action from an ordinary message at a glance and
    // sees the same line the other side does. A binary body is shown by size and
    // a short hexadecimal preview instead of as text.
    let shown = match (kind, content_type) {
        (MessageKind::Action, _) => {
            let who = if sender.trim().is_empty() {
                "you"
            } else {
                sender
            };
            format!("* {who} {text}")
        }
        (MessageKind::Text, ContentType::Binary) => describe_binary(text),
        (MessageKind::Text, ContentType::Text) => text.to_string(),
    };

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
                "📤 → {target}: {shown}  ({display}; queued for {peers}, id #{id})"
            ));
        }
        SendOutcomeKind::Queued => {
            let id = report.message_id.unwrap_or_default();
            let position = report.queue_position.unwrap_or_default();
            emit(&format!(
                "📨 → {target}: {shown}  ({display}; {target} is offline, buffered as #{position}, id #{id})"
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

/// The body of one stored message, rendered the way `/history` prints it.
///
/// A third-person action is shown as the live rendering showed it, so its actor
/// is the local nickname for a message this instance sent and the peer's name for
/// one it received. Naming the peer in both cases made an outgoing line read
/// `→ Bob: * Bob waves`, as if the peer had made the gesture.
///
/// A binary body is described by size and preview instead of being decoded; text
/// is printed as stored.
fn history_body(message: &MessageView, local_nickname: &str) -> String {
    match (message.kind, message.content_type) {
        (_, ContentType::Binary) => describe_binary(&message.body),
        (MessageKind::Action, _) => {
            let actor = if message.direction == "out" {
                local_nickname
            } else {
                message.peer.as_str()
            };
            format!("* {actor} {}", message.body)
        }
        (MessageKind::Text, ContentType::Text) => message.body.clone(),
    }
}

/// Describe a hexadecimal binary body for display: its size and a short preview.
///
/// The body is *not* decoded into text: it is opaque by contract, so rendering it
/// as characters would show something that was never sent. The preview is
/// lowercased, which is also how the body is stored, so a sender that typed
/// uppercase hexadecimal sees the same line the peer does.
fn describe_binary(hex_body: &str) -> String {
    let bytes = hex_body.len() / 2;
    let preview: String = hex_body.chars().take(PREVIEW_DIGITS).collect();
    let preview = preview.to_lowercase();
    if preview.len() < hex_body.len() {
        format!("[binary, {bytes} B] {preview}…")
    } else {
        format!("[binary, {bytes} B] {preview}")
    }
}

/// How many hexadecimal digits of a binary body are shown before an ellipsis.
const PREVIEW_DIGITS: usize = 16;

/// Shown by every friend-request command on a transport that has none.
///
/// It names the remedy rather than saying "no pending request #1", which is what a
/// user would otherwise be told on TCP when they asked about an index in a list
/// that cannot exist there.
const NO_FRIEND_REQUESTS: &str =
    "ℹ️ friend requests only exist on the Tox transport; run with `--transport tox`.";

/// Render an RFC 3339 timestamp the way the REPL always has.
fn format_timestamp(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_string(),
        |stamp| stamp.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary body is described by size and a lowercase hexadecimal preview: it
    /// is never decoded into text, and an uppercase body renders the way it is
    /// stored so the sender and the peer see the same line.
    #[test]
    fn test_binary_description() {
        assert_eq!(describe_binary("00ff10"), "[binary, 3 B] 00ff10");
        assert_eq!(describe_binary("00FF10"), "[binary, 3 B] 00ff10");

        // A long body is truncated with an ellipsis after 16 digits (8 bytes).
        let long = "ab".repeat(20);
        let described = describe_binary(&long);
        assert_eq!(described, format!("[binary, 20 B] {}…", "ab".repeat(8)));
        assert!(described.ends_with('…'));
    }

    /// A stored action names the actor that made the gesture: the local nickname
    /// for an outgoing message, the peer's name for an incoming one. Naming the
    /// peer in both cases made the reader's own action read as the peer's.
    #[test]
    fn test_history_action_names_its_actor() {
        let message = |direction: &str| MessageView {
            direction: direction.to_string(),
            peer: "Bob".to_string(),
            body: "waves at everyone".to_string(),
            wire_bytes: 45,
            kind: MessageKind::Action,
            content_type: ContentType::Text,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };

        assert_eq!(
            history_body(&message("out"), "Alice"),
            "* Alice waves at everyone"
        );
        assert_eq!(
            history_body(&message("in"), "Alice"),
            "* Bob waves at everyone"
        );
    }

    /// Ordinary text and binary bodies keep the rendering they had: text as
    /// stored, binary by size and preview.
    #[test]
    fn test_history_body_keeps_text_and_binary() {
        let text = MessageView {
            direction: "in".to_string(),
            peer: "Bob".to_string(),
            body: "hello".to_string(),
            wire_bytes: 9,
            kind: MessageKind::Text,
            content_type: ContentType::Text,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert_eq!(history_body(&text, "Alice"), "hello");

        let binary = MessageView {
            content_type: ContentType::Binary,
            body: "00ff10".to_string(),
            ..text
        };
        assert_eq!(history_body(&binary, "Alice"), "[binary, 3 B] 00ff10");
    }
}
