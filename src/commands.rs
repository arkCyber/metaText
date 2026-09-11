/*!
 * commands.rs
 *
 * Interactive command parsing for the metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Slash command parsing (`/help`, `/info`, `/chat`, ...)
 * - Plain text detection for outgoing chat messages
 * - Pure, side-effect free parsing that is easy to unit test
 */

/// A single command entered by the user during an interactive session.
///
/// Commands always start with a `/` character; any other input is treated as
/// an outgoing chat message ([`Command::Message`]).
///
/// # Examples
///
/// ```rust
/// use meta_text::commands::{parse, Command};
///
/// assert_eq!(parse("/help"), Command::Help(None));
/// assert_eq!(parse("hello world"), Command::Message("hello world".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Show the list of available commands, or the help for one of them
    /// (`/help [command]`).
    Help(Option<String>),

    /// Show the metaText introduction text.
    Readme,

    /// Show current session and subsystem information.
    Info,

    /// Print the contact (friend) list.
    List,

    /// Print the peers currently connected over the transport.
    Peers,

    /// Print runtime statistics.
    Stats,

    /// Show the stored message history (`/history [n]`).
    History(Option<usize>),

    /// Clear the terminal screen.
    Clear,

    /// Change the local nickname (`/nick <name>`).
    Nick(String),

    /// Change the personal status message (`/status <text>`).
    Status(String),

    /// Add a new contact (`/add <identifier> [note]`).
    Add {
        /// Public identifier (DID-like hex string) of the contact.
        identifier: String,
        /// Optional human readable note stored with the contact.
        note: Option<String>,
    },

    /// Start or switch the active conversation (`/chat <index|name>`).
    Chat(String),

    /// Connect to a peer address (`/connect <host:port>`).
    Connect(String),

    /// An outgoing chat message destined for the active conversation.
    Message(String),

    /// Print the local identity: nickname, DID and bound address.
    Whoami,

    /// Print the application version and the active encryption algorithm.
    Version,

    /// Print how long the current session has been running.
    Uptime,

    /// Persist the current session (nickname, contacts, statistics).
    Save,

    /// Send a message to one peer without changing the active conversation
    /// (`/msg <name> <text>`).
    Msg {
        /// Nickname the message is addressed to.
        target: String,
        /// Message body.
        text: String,
    },

    /// Remove a contact from the friend list (`/remove <index|name>`).
    Remove(String),

    /// Leave the application (`/quit`, `/exit`).
    Quit,

    /// An unrecognised slash command, carrying the command name.
    Unknown(String),
}

/// Parse a single line of interactive input into a [`Command`].
///
/// The parser is intentionally forgiving: surrounding whitespace is ignored and
/// command names are matched case-insensitively. Unknown slash commands are
/// preserved as [`Command::Unknown`] so the caller can report them.
///
/// # Arguments
///
/// * `line` - Raw line typed by the user, without the trailing newline.
///
/// # Returns
///
/// Returns the parsed [`Command`]. Empty input yields a [`Command::Message`]
/// with an empty payload so callers can skip it.
///
/// # Examples
///
/// ```rust
/// use meta_text::commands::{parse, Command};
///
/// assert_eq!(parse("/help"), Command::Help(None));
/// assert_eq!(parse("/help nick"), Command::Help(Some("nick".to_string())));
/// assert_eq!(parse("/nick Alice"), Command::Nick("Alice".to_string()));
/// assert_eq!(parse("/unknown"), Command::Unknown("unknown".to_string()));
/// ```
#[must_use]
pub fn parse(line: &str) -> Command {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return Command::Message(trimmed.to_string());
    }

    // Strip the leading slash and split the command name from its arguments.
    let body = trimmed[1..].trim_start();
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args = parts.next().unwrap_or("").trim();

    match name.as_str() {
        "help" | "h" | "?" => Command::Help((!args.is_empty()).then(|| args.to_string())),
        "readme" => Command::Readme,
        "info" => Command::Info,
        "list" | "friends" => Command::List,
        "peers" | "connections" => Command::Peers,
        "stats" => Command::Stats,
        "history" | "log" => Command::History(args.parse::<usize>().ok()),
        "clear" | "cls" => Command::Clear,
        "quit" | "exit" | "q" => Command::Quit,
        "nick" | "nickname" => Command::Nick(args.to_string()),
        "status" => Command::Status(args.to_string()),
        "whoami" => Command::Whoami,
        "version" | "ver" => Command::Version,
        "uptime" => Command::Uptime,
        "save" => Command::Save,
        "msg" | "send" => {
            let mut msg_parts = args.splitn(2, char::is_whitespace);
            let target = msg_parts.next().unwrap_or("").trim().to_string();
            let text = msg_parts.next().unwrap_or("").trim().to_string();
            Command::Msg { target, text }
        }
        "remove" | "rm" | "del" => Command::Remove(args.to_string()),
        "add" => {
            let mut add_parts = args.splitn(2, char::is_whitespace);
            let identifier = add_parts.next().unwrap_or("").trim().to_string();
            let note = add_parts
                .next()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            Command::Add { identifier, note }
        }
        "chat" | "to" => Command::Chat(args.to_string()),
        "connect" | "join" => Command::Connect(args.to_string()),
        _ => Command::Unknown(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain text without a leading slash becomes an outgoing message
    #[test]
    fn test_parse_plain_message() {
        assert_eq!(
            parse("  hello metaText  "),
            Command::Message("hello metaText".to_string())
        );
    }

    /// Empty or whitespace-only input yields an empty message
    #[test]
    fn test_parse_empty_input_is_empty_message() {
        assert_eq!(parse(""), Command::Message(String::new()));
        assert_eq!(parse("   "), Command::Message(String::new()));
    }

    /// Simple commands are recognised regardless of case
    #[test]
    fn test_parse_simple_commands_are_case_insensitive() {
        assert_eq!(parse("/help"), Command::Help(None));
        assert_eq!(parse("/HELP"), Command::Help(None));
        assert_eq!(parse("/?"), Command::Help(None));
        assert_eq!(parse("/readme"), Command::Readme);
        assert_eq!(parse("/info"), Command::Info);
        assert_eq!(parse("/list"), Command::List);
        assert_eq!(parse("/stats"), Command::Stats);
        assert_eq!(parse("/clear"), Command::Clear);
        assert_eq!(parse("/quit"), Command::Quit);
        assert_eq!(parse("/exit"), Command::Quit);
    }

    /// Commands with arguments keep the raw argument text
    #[test]
    fn test_parse_nick_and_status_keep_arguments() {
        assert_eq!(parse("/nick Alice"), Command::Nick("Alice".to_string()));
        assert_eq!(
            parse("/status busy right now"),
            Command::Status("busy right now".to_string())
        );
        assert_eq!(parse("/nick"), Command::Nick(String::new()));
    }

    /// `/add` supports an optional trailing note
    #[test]
    fn test_parse_add_with_and_without_note() {
        assert_eq!(
            parse("/add DID123"),
            Command::Add {
                identifier: "DID123".to_string(),
                note: None,
            }
        );
        assert_eq!(
            parse("/add DID123 my friend"),
            Command::Add {
                identifier: "DID123".to_string(),
                note: Some("my friend".to_string()),
            }
        );
    }

    /// `/chat` and its `/to` alias accept an index or a name
    #[test]
    fn test_parse_chat_target() {
        assert_eq!(parse("/chat 2"), Command::Chat("2".to_string()));
        assert_eq!(parse("/to Alice"), Command::Chat("Alice".to_string()));
    }

    /// Unknown slash commands are preserved for error reporting
    #[test]
    fn test_parse_unknown_command() {
        assert_eq!(
            parse("/frobnicate now"),
            Command::Unknown("frobnicate".to_string())
        );
    }

    /// `/history` accepts an optional numeric limit
    #[test]
    fn test_parse_history_command() {
        assert_eq!(parse("/history"), Command::History(None));
        assert_eq!(parse("/history 50"), Command::History(Some(50)));
        // Non-numeric arguments fall back to the default limit.
        assert_eq!(parse("/history yesterday"), Command::History(None));
        assert_eq!(parse("/log 5"), Command::History(Some(5)));
    }

    /// `/peers` and its alias list live connections
    #[test]
    fn test_parse_peers_command() {
        assert_eq!(parse("/peers"), Command::Peers);
        assert_eq!(parse("/connections"), Command::Peers);
    }

    /// `/help` optionally carries the command it should explain
    #[test]
    fn test_parse_help_with_and_without_topic() {
        assert_eq!(parse("/help"), Command::Help(None));
        assert_eq!(parse("/help nick"), Command::Help(Some("nick".to_string())));
        assert_eq!(parse("/h  msg "), Command::Help(Some("msg".to_string())));
    }

    /// The new inspection and persistence commands are recognised
    #[test]
    fn test_parse_identity_and_session_commands() {
        assert_eq!(parse("/whoami"), Command::Whoami);
        assert_eq!(parse("/version"), Command::Version);
        assert_eq!(parse("/ver"), Command::Version);
        assert_eq!(parse("/uptime"), Command::Uptime);
        assert_eq!(parse("/save"), Command::Save);
    }

    /// `/msg` splits the target from the body and keeps the body intact
    #[test]
    fn test_parse_msg_command() {
        assert_eq!(
            parse("/msg Alice hello there"),
            Command::Msg {
                target: "Alice".to_string(),
                text: "hello there".to_string(),
            }
        );
        assert_eq!(
            parse("/send Bob"),
            Command::Msg {
                target: "Bob".to_string(),
                text: String::new(),
            }
        );
        assert_eq!(
            parse("/msg"),
            Command::Msg {
                target: String::new(),
                text: String::new(),
            }
        );
    }

    /// `/remove` accepts an index or a name
    #[test]
    fn test_parse_remove_command() {
        assert_eq!(parse("/remove 2"), Command::Remove("2".to_string()));
        assert_eq!(parse("/rm Alice"), Command::Remove("Alice".to_string()));
        assert_eq!(parse("/del"), Command::Remove(String::new()));
    }
}
