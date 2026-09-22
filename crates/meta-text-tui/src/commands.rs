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
/// use meta_text_tui::commands::{parse, Command};
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

    /// Print operational counters for monitoring.
    Metrics,

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

    /// Reject a pending friend request (`/reject <public key|index>`).
    Reject(String),

    /// Accept a pending friend request (`/accept <public key|index>`).
    ///
    /// Only the Tox transport produces friend requests; the TCP transport never
    /// does, so it reports the command as inapplicable.
    Accept(String),

    /// List the pending friend requests (`/requests`).
    ///
    /// Only the Tox transport produces friend requests.
    Requests,

    /// An outgoing chat message destined for the active conversation.
    Message(String),

    /// An outgoing third-person action (`/me waves`).
    Action(String),

    /// An outgoing binary payload, as lowercase hexadecimal (`/bin 00ff`).
    Binary(String),

    /// Group chat management (`/group create|list|join|invite|send|leave`).
    Group {
        /// Subcommand (`create`, `list`, `join`, ...); empty means `list`.
        subcommand: String,

        /// Remaining arguments, unparsed.
        arguments: String,
    },

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
/// use meta_text_tui::commands::{parse, Command};
///
/// assert_eq!(parse("/help"), Command::Help(None));
/// assert_eq!(parse("/help nick"), Command::Help(Some("nick".to_string())));
/// assert_eq!(parse("/nick Alice"), Command::Nick("Alice".to_string()));
/// assert_eq!(parse("/unknown"), Command::Unknown("unknown".to_string()));
/// ```
#[must_use]
pub fn parse(line: &str) -> Command {
    let trimmed = line.trim();
    // `strip_prefix` rather than `trimmed[1..]`: the slice form is only safe
    // because `/` is ASCII, and the type system cannot say so. A line that does not
    // start with the slash is a message, exactly as before.
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Command::Message(trimmed.to_string());
    };

    // Split the command name from its arguments.
    let body = rest.trim_start();
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
        "metrics" => Command::Metrics,
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
        "accept" => Command::Accept(args.to_string()),
        "reject" | "decline" => Command::Reject(args.to_string()),
        "requests" | "pending" => Command::Requests,
        "me" | "action" | "emote" => Command::Action(args.to_string()),
        "bin" | "binary" | "hex" => Command::Binary(args.to_string()),
        "group" | "groups" | "g" => {
            let (subcommand, arguments) = args
                .split_once(char::is_whitespace)
                .map_or((args, ""), |(head, tail)| (head, tail.trim()));
            Command::Group {
                subcommand: subcommand.to_lowercase(),
                arguments: arguments.to_string(),
            }
        }
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
        assert_eq!(parse("/metrics"), Command::Metrics);
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

    /// `/accept` carries the requester's key or index
    #[test]
    fn test_parse_accept_command() {
        assert_eq!(parse("/accept"), Command::Accept(String::new()));
        assert_eq!(parse("/accept AB12"), Command::Accept("AB12".to_string()));
    }

    /// `/reject` carries the requester's key or index, and takes `/decline` too
    #[test]
    fn test_parse_reject_command() {
        assert_eq!(parse("/reject"), Command::Reject(String::new()));
        assert_eq!(parse("/reject 1"), Command::Reject("1".to_string()));
        assert_eq!(parse("/decline AB12"), Command::Reject("AB12".to_string()));
    }

    /// `/me` and its aliases carry a third-person action
    #[test]
    fn test_parse_action_command() {
        assert_eq!(parse("/me waves"), Command::Action("waves".to_string()));
        assert_eq!(parse("/action  nods "), Command::Action("nods".to_string()));
        assert_eq!(parse("/emote"), Command::Action(String::new()));
    }

    /// `/bin` and its aliases carry a hexadecimal binary payload
    #[test]
    fn test_parse_binary_command() {
        assert_eq!(parse("/bin 00ff10"), Command::Binary("00ff10".to_string()));
        assert_eq!(
            parse("/hex  deadbeef"),
            Command::Binary("deadbeef".to_string())
        );
        assert_eq!(parse("/binary"), Command::Binary(String::new()));
    }

    /// `/group` splits a subcommand from its arguments
    #[test]
    fn test_parse_group_command() {
        assert_eq!(
            parse("/group create Team"),
            Command::Group {
                subcommand: "create".to_string(),
                arguments: "Team".to_string(),
            }
        );
        // A bare `/group` (and its aliases) means "list".
        assert_eq!(
            parse("/group"),
            Command::Group {
                subcommand: String::new(),
                arguments: String::new(),
            }
        );
        assert_eq!(
            parse("/groups"),
            Command::Group {
                subcommand: String::new(),
                arguments: String::new(),
            }
        );
        // The subcommand is case-insensitive and extra spacing is collapsed.
        assert_eq!(
            parse("/g  INVITE   Alice"),
            Command::Group {
                subcommand: "invite".to_string(),
                arguments: "Alice".to_string(),
            }
        );
        // `send` keeps the whole message, spaces included.
        assert_eq!(
            parse("/group send hello  there"),
            Command::Group {
                subcommand: "send".to_string(),
                arguments: "hello  there".to_string(),
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
