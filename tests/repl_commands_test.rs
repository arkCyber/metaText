/*!
 * repl_commands_test.rs
 *
 * End-to-end tests for the `run` subcommand and every interactive REPL command
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Unlike the in-process unit tests (which call `handle_command` directly) these
 * tests drive the *compiled binary* the way a user would: they spawn
 * `meta-text run`, feed a script of commands through stdin, and assert on the
 * real stdout. That exercises argument parsing, configuration loading, logging
 * setup, the stdin reader, the event loop and graceful shutdown together.
 */

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Path to the binary under test, provided by Cargo.
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_meta-text")
}

/// The repository template configuration, copied into each isolated workspace.
fn config_template() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml")
}

/// Ask the OS for a currently free IPv4 port.
///
/// The listener is dropped immediately, so the port may be reused by another
/// process in theory; in practice the window is tiny and it keeps every test
/// off the default port 33445.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Result of one scripted REPL session.
#[derive(Debug)]
struct SessionOutcome {
    /// Everything the process wrote to stdout.
    stdout: String,
    /// Everything the process wrote to stderr.
    stderr: String,
    /// Exit code, or `None` when the process was killed.
    code: Option<i32>,
    /// Whether the process had to be killed after the timeout elapsed.
    timed_out: bool,
}

impl SessionOutcome {
    /// Both streams concatenated, handy for error assertions.
    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Spawn `meta-text` with `args`, optionally write `input` to its stdin and
/// wait up to `timeout` for it to exit.
///
/// Closing stdin after writing it makes the REPL reader hit EOF, which the
/// application turns into a graceful shutdown; an explicit `/quit` line makes
/// the process stop even earlier. When the timeout elapses the child is killed
/// so a mode that never reads stdin (`--headless`, `server`, `daemon`) cannot
/// hang the test suite.
fn execute(args: &[String], input: Option<&str>, cwd: &Path, timeout: Duration) -> SessionOutcome {
    let mut child = Command::new(binary())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", binary()));

    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("child stdin");
        stdin.write_all(input.as_bytes()).expect("write stdin");
        // Dropping `stdin` here closes the pipe, sending EOF to the reader.
    } else {
        drop(child.stdin.take());
    }

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    let output = child.wait_with_output().expect("collect output");
    SessionOutcome {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
        timed_out,
    }
}

/// Build the standard hermetic arguments: the requested mode plus a copied
/// config, a private data directory, a free port and quiet logging.
fn hermetic_args(cli: &[&str], config_path: &Path, data_dir: &Path, port: u16) -> Vec<String> {
    let mut args: Vec<String> = cli.iter().map(|value| (*value).to_string()).collect();
    args.push("--config".to_string());
    args.push(config_path.to_string_lossy().into_owned());
    args.push("--data-dir".to_string());
    args.push(data_dir.to_string_lossy().into_owned());
    args.push("--port".to_string());
    args.push(port.to_string());
    // `error` keeps informational log lines off stdout so the assertions below
    // only match the REPL output itself.
    args.push("--log-level".to_string());
    args.push("error".to_string());
    args
}

/// Run a hermetic REPL session in a private temp directory.
///
/// `cli` holds the mode selection (`run` or `--mode cli`) plus any extra flags.
/// The standard hermetic arguments (config copy, data dir, free port, quiet
/// logging) are appended automatically.
fn run_session(
    cli: &[&str],
    input: &str,
    timeout: Duration,
) -> (SessionOutcome, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::copy(config_template(), &config_path).expect("copy config");
    let data_dir = dir.path().join("data");
    let args = hermetic_args(cli, &config_path, &data_dir, free_port());

    let outcome = execute(&args, Some(input), dir.path(), timeout);
    (outcome, dir)
}

/// Assert that `haystack` contains `needle`, printing the full output on failure.
fn assert_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        haystack.contains(needle),
        "{context}: expected to find {needle:?}\n----- stdout -----\n{haystack}\n------------------"
    );
}

/// Assert that at least one of `needles` is present (used where a feature flag
/// changes the exact wording).
fn assert_contains_any(haystack: &str, needles: &[&str], context: &str) {
    assert!(
        needles.iter().any(|needle| haystack.contains(needle)),
        "{context}: expected one of {needles:?}\n----- stdout -----\n{haystack}\n------------------"
    );
}

/// One scripted session that walks through every interactive command.
///
/// The order matters: contacts are added before `/chat`/`/msg`, and removed
/// near the end so later `/list` calls can assert the empty state again. The
/// final `/q` exercises the short quit alias and makes the process stop before
/// EOF handling.
const FULL_SCRIPT: &str = "\
/help\n\
/help nick\n\
/help /msg\n\
/help nope\n\
/HELP\n\
/readme\n\
/info\n\
/whoami\n\
/version\n\
/ver\n\
/uptime\n\
/list\n\
/friends\n\
/peers\n\
/connections\n\
/stats\n\
/history\n\
/log 5\n\
/clear\n\
/cls\n\
/nick\n\
/nick Alice\n\
/nickname\n\
/status\n\
/status busy right now\n\
/add\n\
/add DID123 my friend\n\
/add DID123\n\
/chat\n\
/chat 0\n\
/chat did\n\
/chat 1\n\
/to 1\n\
/list\n\
hello world\n\
/msg Bob\n\
/msg Alice hi there\n\
/send Alice second note\n\
/remove\n\
/rm\n\
/del\n\
/remove nobody\n\
/remove 1\n\
/list\n\
/friends\n\
/connect\n\
/connect nope\n\
/join\n\
/unknowncmd\n\
/save\n\
/q\n\
";

/// `run` executes every interactive command and produces the documented output
#[test]
fn test_run_executes_all_repl_commands() {
    let (outcome, _dir) = run_session(&["run"], FULL_SCRIPT, Duration::from_secs(20));
    let stdout = &outcome.stdout;

    // The session must end cleanly through `/q`.
    assert!(!outcome.timed_out, "run timed out\n{stdout}");
    assert_eq!(outcome.code, Some(0), "non-zero exit\n{stdout}");

    // Help: full list, one topic, and an unknown topic.
    assert_contains(stdout, "metaText commands:", "/help list");
    assert_contains(stdout, "Usage: /nick [name]", "/help nick");
    assert_contains(
        stdout,
        "Usage: /msg <name> <text>",
        "/help /msg (leading slash)",
    );
    assert_contains(stdout, "no help available for '/nope'", "/help nope");
    // `/HELP` proves command names are matched case-insensitively.
    assert_contains(stdout, "metaText commands:", "/HELP alias");

    // Introduction and session introspection.
    assert_contains(
        stdout,
        ">> metaText ~ a Web3 decentralized instant messenger",
        "/readme",
    );
    assert_contains(stdout, "metaText status", "/info banner");
    assert_contains(stdout, "mode            : CLI", "/info mode");
    assert_contains(stdout, "metaText identity", "/whoami");
    assert_contains(stdout, "  DID      : ", "/whoami DID");
    assert_contains(
        stdout,
        "metaText v0.4.0 (encryption: ChaCha20-Poly1305)",
        "/version",
    );
    assert_contains(
        stdout,
        "metaText v0.4.0 (encryption: ChaCha20-Poly1305)",
        "/ver alias",
    );
    assert_contains(stdout, "⏱️ uptime:", "/uptime");

    // Friends and peers before anything was added.
    assert_contains(stdout, "You have no friends yet.", "/list empty");
    assert_contains(stdout, "Local address:", "/peers");
    assert_contains(stdout, "No peers connected yet.", "/peers empty");
    assert_contains(stdout, "Local address:", "/connections alias");

    // Statistics and history (wording depends on the `sqlite` feature).
    assert_contains(stdout, "Runtime statistics:", "/stats");
    assert_contains_any(
        stdout,
        &["message history is not available", "no stored messages yet"],
        "/history",
    );
    assert_contains_any(
        stdout,
        &["message history is not available", "no stored messages yet"],
        "/log alias",
    );

    // `/clear` and `/cls` emit the ANSI clear-screen sequence in the plain REPL.
    assert_contains(stdout, "\u{1b}[2J", "/clear");
    assert_contains(stdout, "\u{1b}[2J", "/cls alias");

    // Nickname and status: query with no argument, then set.
    assert_contains(stdout, "nickname: metaText00", "/nick query");
    assert_contains(stdout, "✅ nickname set to 'Alice'", "/nick set");
    assert_contains(stdout, "nickname: Alice", "/nickname alias query");
    assert_contains(stdout, "status: Keep on Metaverse .......", "/status query");
    assert_contains(stdout, "✅ status set to 'busy right now'", "/status set");

    // Adding friends: usage, success and duplicate detection.
    assert_contains(stdout, "Usage: /add <DID_Address> [note]", "/add usage");
    assert_contains(stdout, "✅ added friend #1 : DID123", "/add success");
    assert_contains(stdout, "'DID123' already exists", "/add duplicate");

    // Conversation selection.
    assert_contains(stdout, "no active conversation", "/chat query");
    assert_contains(stdout, "friend numbers start at 1", "/chat 0");
    assert_contains(
        stdout,
        "now chatting with #1 DID123",
        "/chat by name fragment",
    );
    assert_contains(stdout, "now chatting with #1 DID123", "/chat 1");
    assert_contains(stdout, "now chatting with #1 DID123", "/to alias");
    assert_contains(stdout, "← active chat", "/list active marker");

    // Plain text is routed to the active conversation.
    assert_contains(stdout, "📨 → DID123: hello world", "plain message");

    // `/msg` and `/send`: usage and direct delivery to an offline peer.
    assert_contains(stdout, "Usage: /msg <name> <text>", "/msg usage");
    assert_contains(stdout, "📨 → Alice: hi there", "/msg send");
    assert_contains(stdout, "📨 → Alice: second note", "/send alias");

    // Removing friends: usage, unknown target and success.
    assert_contains(stdout, "Usage: /remove <index|name>", "/remove usage");
    assert_contains(stdout, "Usage: /remove <index|name>", "/rm alias usage");
    assert_contains(stdout, "Usage: /remove <index|name>", "/del alias usage");
    assert_contains(stdout, "no friend matches 'nobody'", "/remove unknown");
    assert_contains(stdout, "🗑️ removed friend #1 DID123", "/remove success");
    assert_contains(stdout, "You have no friends yet.", "/list after removal");

    // Connecting to peers: usage and address validation.
    assert_contains(stdout, "Usage: /connect <host:port>", "/connect usage");
    assert_contains(stdout, "Usage: /connect <host:port>", "/join alias usage");
    assert_contains(
        stdout,
        "'nope' is not a valid host:port address",
        "/connect invalid",
    );

    // Unknown commands and persistence.
    assert_contains(stdout, "unknown command '/unknowncmd'", "unknown command");
    assert_contains(stdout, "session saved to", "/save");
}

/// `run` is equivalent to `--mode cli` (README contract)
#[test]
fn test_run_subcommand_matches_cli_mode() {
    let (via_run, _dir_a) = run_session(&["run"], "/info\n/quit\n", Duration::from_secs(15));
    let (via_flag, _dir_b) = run_session(
        &["--mode", "cli"],
        "/info\n/quit\n",
        Duration::from_secs(15),
    );

    for (label, outcome) in [("run", &via_run), ("--mode cli", &via_flag)] {
        assert_eq!(outcome.code, Some(0), "{label} should exit cleanly");
        assert_contains(&outcome.stdout, "mode            : CLI", label);
    }
}

/// An explicit `run` wins over a conflicting `--mode tui`
#[test]
fn test_run_wins_over_conflicting_mode_flag() {
    let (outcome, _dir) = run_session(
        &["--mode", "tui", "run"],
        "/info\n/quit\n",
        Duration::from_secs(15),
    );
    assert_eq!(outcome.code, Some(0));
    assert_contains(
        &outcome.stdout,
        "mode            : CLI",
        "run overrides --mode tui",
    );
}

/// `--nick` overrides the configured default nickname
#[test]
fn test_cli_nickname_override() {
    let (outcome, _dir) = run_session(
        &["run", "--nick", "Bob"],
        "/info\n/whoami\n/quit\n",
        Duration::from_secs(15),
    );
    assert_eq!(outcome.code, Some(0));
    assert_contains(&outcome.stdout, "nickname        : Bob", "/info nickname");
    assert_contains(&outcome.stdout, "nickname : Bob", "/whoami nickname");
}

/// `--no-encryption` disables the crypto pipeline
#[test]
fn test_no_encryption_flag() {
    let (outcome, _dir) = run_session(
        &["run", "--no-encryption"],
        "/info\n/quit\n",
        Duration::from_secs(15),
    );
    assert_eq!(outcome.code, Some(0));
    assert_contains(
        &outcome.stdout,
        "encryption      : disabled",
        "/info encryption",
    );
}

/// With `--mode tui` but a piped stdout the app falls back to the plain REPL
#[test]
fn test_tui_falls_back_to_repl_when_not_a_terminal() {
    let (outcome, _dir) = run_session(
        &["--mode", "tui"],
        "/info\n/quit\n",
        Duration::from_secs(15),
    );
    assert_eq!(outcome.code, Some(0));
    // The banner/REPL output is still produced...
    assert_contains(
        &outcome.stdout,
        "Type `/help` to get metaText command list.",
        "tui fallback banner",
    );
    // ...and the session reports the requested mode.
    assert_contains(
        &outcome.stdout,
        "mode            : TUI",
        "tui fallback mode",
    );
}

/// `--headless` starts the subsystems but never reads stdin or shows the REPL
#[test]
fn test_headless_suppresses_repl() {
    let (outcome, _dir) = run_session(
        &["run", "--headless"],
        "/help\n/quit\n",
        Duration::from_millis(1500),
    );

    // No user interface is started, so the process keeps running until killed.
    assert!(
        outcome.timed_out,
        "headless run should not exit on stdin EOF"
    );
    assert!(
        !outcome.stdout.contains("metaText commands:"),
        "headless must not show the command list\n{}",
        outcome.stdout
    );
    assert!(
        !outcome.stdout.contains("Welcome to Metaverse"),
        "headless must not show the banner\n{}",
        outcome.stdout
    );
}

/// Invalid command line arguments are rejected before the app starts
#[test]
fn test_run_rejects_invalid_arguments() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Port 0 is rejected.
    let port_zero = execute(
        &["run".to_string(), "--port".to_string(), "0".to_string()],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_ne!(port_zero.code, Some(0), "port 0 must fail");
    assert_contains(&port_zero.combined(), "Port cannot be 0", "port 0");

    // A malformed peer address is rejected.
    let bad_peer = execute(
        &[
            "run".to_string(),
            "--peer".to_string(),
            "not-an-address".to_string(),
        ],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_ne!(bad_peer.code, Some(0), "bad peer must fail");
    assert_contains(
        &bad_peer.combined(),
        "Invalid peer address",
        "bad peer address",
    );

    // A non-default config path must exist.
    let missing_config = execute(
        &[
            "run".to_string(),
            "--config".to_string(),
            "/nonexistent/meta-text/missing.toml".to_string(),
        ],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_ne!(missing_config.code, Some(0), "missing config must fail");
    assert_contains(
        &missing_config.combined(),
        "Configuration file does not exist",
        "missing config",
    );

    // clap rejects unknown modes outright (exit code 2).
    let bad_mode = execute(
        &["--mode".to_string(), "nope".to_string()],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_eq!(bad_mode.code, Some(2), "unknown mode must be a clap error");
    assert_contains(&bad_mode.stderr, "invalid value", "unknown mode");
}

/// `--help` and `run --help` document the subcommand and the global options
#[test]
fn test_help_output() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    let top = execute(
        &["--help".to_string()],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_eq!(top.code, Some(0), "--help exits 0");
    assert_contains(&top.combined(), "run", "top-level help lists run");
    assert_contains(&top.combined(), "--mode", "top-level help lists --mode");

    let sub = execute(
        &["run".to_string(), "--help".to_string()],
        None,
        manifest,
        Duration::from_secs(10),
    );
    assert_eq!(sub.code, Some(0), "run --help exits 0");
    // Global options are accepted (and documented) after the subcommand.
    assert_contains(&sub.combined(), "--port", "run help lists --port");
    assert_contains(&sub.combined(), "--data-dir", "run help lists --data-dir");
}

/// A message longer than `max_message_length` is rejected before sending
#[test]
fn test_message_length_limit_is_enforced() {
    let long = "x".repeat(1400);
    let script = format!("/msg Alice {long}\n/stats\n/quit\n");
    let (outcome, _dir) = run_session(&["run"], &script, Duration::from_secs(15));

    assert_eq!(outcome.code, Some(0));
    assert_contains(&outcome.stdout, "the limit is 1372", "length limit warning");
    // The oversized message is neither queued nor counted.
    assert_contains(&outcome.stdout, "messages sent     : 0", "no message sent");
}

/// A long-lived `run` instance used to exercise the real transport.
///
/// Its stdin pipe is left open (no data written), so the REPL never sees EOF
/// and the process keeps running until it is killed.
struct RunningInstance {
    /// The spawned child process.
    child: std::process::Child,
    /// Keeps the private workspace alive for the lifetime of the instance.
    _dir: tempfile::TempDir,
    /// Port the listener is bound to.
    port: u16,
}

impl RunningInstance {
    /// Spawn an instance in an isolated workspace.
    fn spawn(cli: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::copy(config_template(), &config_path).expect("copy config");
        let data_dir = dir.path().join("data");
        let port = free_port();
        let args = hermetic_args(cli, &config_path, &data_dir, port);
        let child = Command::new(binary())
            .args(&args)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn instance");
        Self {
            child,
            _dir: dir,
            port,
        }
    }

    /// Kill the instance and return everything it wrote.
    fn shutdown(mut self) -> SessionOutcome {
        let _ = self.child.kill();
        let output = self.child.wait_with_output().expect("collect output");
        SessionOutcome {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code(),
            timed_out: false,
        }
    }
}

/// Two `run` instances sharing a passphrase talk to each other over the wire
#[test]
fn test_two_run_instances_exchange_messages() {
    let passphrase = "repl-e2e-passphrase";

    // Server: does not read stdin, so it stays up until killed.
    let server = RunningInstance::spawn(&["run", "--nick", "Alice", "--passphrase", passphrase]);
    // Give the listener a moment to bind before the client dials it.
    std::thread::sleep(Duration::from_millis(700));

    let peer = format!("127.0.0.1:{}", server.port);
    let client_cli = vec![
        "run",
        "--nick",
        "Bob",
        "--passphrase",
        passphrase,
        "--peer",
        peer.as_str(),
    ];
    let script = "/peers\n/msg Alice hello from the client\n/quit\n";
    let (client, _client_dir) = run_session(&client_cli, script, Duration::from_secs(20));

    assert_eq!(client.code, Some(0), "client failed: {}", client.stdout);
    // The client sees the server's announced nickname...
    assert_contains(&client.stdout, "1 peer(s) connected:", "client peers");
    assert_contains(&client.stdout, "1. Alice", "client peer nickname");
    // ...and its message is queued for exactly that peer.
    assert_contains(
        &client.stdout,
        "📤 → Alice: hello from the client",
        "client send",
    );
    assert_contains(&client.stdout, "queued for 1 peer", "client send count");

    // Let the server process the inbound frame before it is killed.
    std::thread::sleep(Duration::from_millis(500));
    let server_out = server.shutdown();
    assert_contains(
        &server_out.stdout,
        "📥 Bob: hello from the client",
        "server received",
    );
}
