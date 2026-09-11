/*!
 * text.rs
 *
 * Static help, banner and README text for the metaText front-ends.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Pure functions: no I/O, no state, trivially unit testable
 * - Single source of truth for user facing wording
 */

/// Render the whole number of seconds as `Hh MMm SSs`.
///
/// # Arguments
///
/// * `total_seconds` - Non-negative duration in seconds.
///
/// # Examples
///
/// ```rust
/// use meta_text::ui::text::format_duration;
///
/// assert_eq!(format_duration(3723), "1h 02m 03s");
/// ```
#[must_use]
pub fn format_duration(total_seconds: i64) -> String {
    let clamped = total_seconds.max(0);
    let hours = clamped / 3600;
    let minutes = (clamped % 3600) / 60;
    let seconds = clamped % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

/// The full command list shown by `/help`.
#[must_use]
pub fn help_lines() -> Vec<String> {
    [
        "",
        "metaText commands:",
        "  /help [command]    - show this command list or one command's help",
        "  /readme            - show the metaText introduction",
        "  /info              - show session and subsystem information",
        "  /list              - list your friends",
        "  /peers             - list connected peers",
        "  /connect <addr>    - connect to a peer (host:port)",
        "  /add <DID> [note]  - add a friend by DID address",
        "  /remove <n|name>   - remove a friend from the list",
        "  /chat <n|name>     - talk to friend n or the given name (no arg: show)",
        "  /msg <name> <text> - send <text> to one peer without switching chat",
        "  /nick <name>       - change your nickname (no arg: show it)",
        "  /status <text>     - change your status message (no arg: show it)",
        "  /whoami            - show your nickname, DID and bound address",
        "  /version           - show the application version",
        "  /uptime            - show how long this session has been running",
        "  /stats             - show runtime statistics",
        "  /history [n]       - show the last n stored messages (default 20)",
        "  /save              - persist the current session to disk",
        "  /clear             - clear the screen",
        "  /quit              - leave metaText",
        "  <text>             - send <text> to the active conversation",
        "",
    ]
    .iter()
    .map(|line| (*line).to_string())
    .collect()
}

/// The detailed help for a single command (`/help <command>`).
///
/// An unknown topic is reported instead of silently falling back to the full
/// list, so a typo is easy to spot.
#[must_use]
pub fn command_help_lines(topic: &str) -> Vec<String> {
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
        "" => return help_lines(),
        other => {
            return vec![format!(
                "❓ no help available for '/{other}'. Type /help for the command list."
            )]
        }
    };

    vec![
        String::new(),
        format!("Usage: {usage}"),
        format!("  {description}"),
        String::new(),
    ]
}

/// The metaText introduction shown by `/readme`.
#[must_use]
pub fn readme_lines() -> Vec<String> {
    [
        "",
        ">> metaText ~ a Web3 decentralized instant messenger",
        "   - P2P text messaging with end-to-end encryption",
        "   - Your identity is a self generated key pair (no server needed)",
        "   - Use /add <DID_Address> Hello to invite a friend",
        "   - Use /chat <n> to start talking, /info shows the numbering",
        "   arkDelphi Metaverse Lab ~ arksong2018@gmail.com",
        "",
    ]
    .iter()
    .map(|line| (*line).to_string())
    .collect()
}

/// The startup banner.
///
/// # Arguments
///
/// * `full_screen` - When `true` only plain text is produced, because the full
///   screen interface cannot render raw ANSI escape sequences.
///
/// # Examples
///
/// ```rust
/// use meta_text::ui::text::startup_banner;
///
/// let lines = startup_banner(false);
/// assert!(lines.iter().any(|line| line.contains("Welcome to Metaverse")));
/// ```
#[must_use]
pub fn startup_banner(full_screen: bool) -> Vec<String> {
    if full_screen {
        return [
            "🌟 Welcome to Metaverse Web3 Communication CyberSpace",
            ">> metaText ~ a Simple Web3 Text Instant Messager",
            "   ::: arkDelphi Metaverse Lab",
            "Type `/help` for the command list, `/readme` for an introduction.",
            "",
        ]
        .iter()
        .map(|line| (*line).to_string())
        .collect();
    }

    [
        "",
        "",
        "",
        "🌟 Welcome to Metaverse Web3 Communication CyberSpace",
        "\u{1b}[36m",
        "                __       ___________              __   ",
        "   ___    _____/  |______\\__    ___/___ ___  ____/  |_ ",
        " /     \\_/ __ \\   __\\__  \\ |    |_/ __ \\  \\/  /\\   __",
        "|  Y Y  \\  ___/|  |  / __ \\|    |\\  ___/ >    <  |  |  ",
        "|__|_|  /\\___  >__| (____  /____| \\___  >__/\\_ \\ |__|  ",
        "      \\/     \\/          \\/           \\/      \\/     ",
        "\u{1b}[0m",
        ">> metaText ~ a Simple Web3 Text Instant Messager",
        "   ::: arkDelphi Metaverse Lab",
        "         arksong2018@gmail.com",
        "..................................................................",
        "",
        "Type `/help` to get metaText command list.",
        "Type `/readme` to get metaText introduction.",
        "",
    ]
    .iter()
    .map(|line| (*line).to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Durations are rendered with zero padding.
    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0h 00m 00s");
        assert_eq!(format_duration(63), "0h 01m 03s");
        assert_eq!(format_duration(3723), "1h 02m 03s");
        assert_eq!(format_duration(-5), "0h 00m 00s");
    }

    /// The help list mentions the slash commands users rely on.
    #[test]
    fn test_help_lists_core_commands() {
        let lines = help_lines().join("\n");
        assert!(lines.contains("metaText commands:"));
        for command in ["/help", "/list", "/chat", "/msg", "/quit"] {
            assert!(lines.contains(command), "missing {command}");
        }
    }

    /// Topic help resolves aliases and rejects unknown topics.
    #[test]
    fn test_command_help_lookup() {
        let nick = command_help_lines("nick").join("\n");
        assert!(nick.contains("Usage: /nick [name]"));

        // A leading slash is tolerated.
        let msg = command_help_lines("/msg").join("\n");
        assert!(msg.contains("Usage: /msg <name> <text>"));

        let unknown = command_help_lines("nope").join("\n");
        assert!(unknown.contains("no help available for '/nope'"));

        // An empty topic falls back to the full list.
        assert!(command_help_lines("")
            .join("\n")
            .contains("metaText commands:"));
    }

    /// The plain banner carries ANSI colour, the full screen one does not.
    #[test]
    fn test_startup_banner_variants() {
        let plain = startup_banner(false).join("\n");
        assert!(plain.contains("\u{1b}[36m"));
        assert!(plain.contains("Type `/help` to get metaText command list."));

        let full = startup_banner(true).join("\n");
        assert!(!full.contains('\u{1b}'));
        assert!(full.contains("Welcome to Metaverse"));
    }
}
