/*!
 * doc_contract_test.rs
 *
 * The numbers the shipped documents promise, checked against the constants.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * `README.md` and `docs/ARCHITECTURE.md` are read by users and by anyone writing a
 * client in another language, and both quote numbers that also exist as constants:
 * the protocol version in the handshake examples, the window the endpoint serves,
 * the width of the length prefix, and the frame and message bounds. Nothing
 * compared the two, so the documents drifted twice - the protocol went to 8 while
 * the examples kept saying 7, then it went to 9 while both examples kept saying 8
 * (`docs/ARCHITECTURE.md` §5, A52). The five member crates ship a README of their
 * own to crates.io - the page a reader who depends on one layer alone opens - and
 * `meta-text-proto` quotes the version there too, so those are read back as well.
 *
 * `ipc_socket_test::test_protocol_version_is_announced` pins the *constant* by
 * hand, which is what catches a missing bump. This file pins the *documents*
 * against it, so the next bump cannot leave a reader with a literal the build does
 * not speak. Every assertion names the document, the line, the value the document
 * states and the constant it has to match.
 *
 * The documents are part of the repository, so they are read from
 * `CARGO_MANIFEST_DIR` - the pattern `repl_commands_test` already uses for the
 * shipped `config.toml`. The wording is matched as the documents write it rather
 * than as a paraphrase, because a paraphrase is what drifts unnoticed.
 */

use std::fs;
use std::path::PathBuf;

use meta_text::config::MAX_MESSAGE_LENGTH;
use meta_text::ipc::framing::FRAME_HEADER_LEN;
use meta_text::ipc::protocol::{MAX_FRAME_LEN, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION};

/// The user-facing frame example.
const README: &str = "README.md";

/// The interface control document.
const ARCHITECTURE: &str = "docs/ARCHITECTURE.md";

/// Read one of the shipped documents.
///
/// The path is resolved from `CARGO_MANIFEST_DIR` so the test does not depend on
/// the working directory cargo happens to have been started in.
fn document(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Every decimal number that follows `marker`, with the line it was found on.
///
/// A marker that is *not* followed by a number is prose about the claim rather
/// than the claim itself - both documents describe these rules in the very
/// paragraphs a reader uses to check them - so it is skipped. Each caller then
/// asserts that at least one claim was found, which is what keeps a wholesale
/// rewording from turning the check into a no-op.
fn claims(text: &str, marker: &str) -> Vec<(usize, u64)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let digits: String = rest
                .chars()
                .skip_while(|character| character.is_whitespace())
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let Ok(number) = digits.parse::<u64>() {
                found.push((index + 1, number));
            }
        }
    }
    found
}

/// How many times `marker` appears at all, claimed or not.
fn occurrences(text: &str, marker: &str) -> usize {
    text.match_indices(marker).count()
}

/// How a marker is expected to appear in a document.
///
/// The distinction is what keeps this file from failing on its own subject matter:
/// an *example* carries a literal, so a marker without a number after it is a
/// broken example, while a *phrase* is also used in prose that describes the rule
/// (the audit ledger quotes the claim to record what drifted).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// A literal inside an example: every occurrence must state a number.
    Literal,
    /// A phrase a document also quotes when describing the rule.
    Prose,
}

/// Assert every `marker` claim in `document_name` states `expected`.
fn assert_claims(
    document_name: &str,
    text: &str,
    marker: &str,
    expected: u64,
    what: &str,
    kind: Marker,
) {
    let found = claims(text, marker);
    assert!(
        !found.is_empty(),
        "{document_name}: no claim about {what} was found; expected {marker:?} followed by a \
         number. If the wording changed, update this test with it (A52)"
    );
    if kind == Marker::Literal {
        assert_eq!(
            found.len(),
            occurrences(text, marker),
            "{document_name}: a {what} claim is written without a number after {marker:?}, which \
             a reader copying the example would copy verbatim"
        );
    }
    for (line, stated) in found {
        assert_eq!(
            stated, expected,
            "{document_name}:{line} says {what} is {stated}, while the constant is {expected}. \
             Update the document, or the constant if the wire really changed (A52)"
        );
    }
}

/// The handshake examples speak the version this build announces.
///
/// These lines are the contract for a client written in another language, and both
/// documents had drifted: the constant was 9 while the examples still showed 8.
/// A literal that is *older* on purpose (an example of a stale client being
/// refused) would need its own test rather than an exception here.
#[test]
fn test_the_documented_handshake_carries_the_current_protocol_version() {
    for name in [README, ARCHITECTURE] {
        let text = document(name);
        assert_claims(
            name,
            &text,
            r#""protocol_version":"#,
            u64::from(PROTOCOL_VERSION),
            "the protocol version of a handshake example",
            Marker::Literal,
        );
    }
}

/// The member crates' landing pages.
///
/// Each of these is shipped to crates.io as the crate's README, so it is read by
/// whoever depends on that layer alone — a narrower audience than `README.md`, but
/// the same kind of claim: `meta-text-proto` states the protocol version its
/// vocabulary speaks. None of them was compared against the constant, which is how
/// the proto README came to say 8 while the constant had moved to 9 (A52, one level
/// down from the drift it recorded).
const CRATE_READMES: [&str; 5] = [
    "crates/meta-text-proto/README.md",
    "crates/meta-text-backend/README.md",
    "crates/meta-text-core/README.md",
    "crates/meta-text-tui/README.md",
    "crates/meta-text-cli/README.md",
];

/// A crate README that states the protocol version states the current one.
///
/// Not every crate README has a reason to mention the wire contract (`meta-text-cli`
/// and `meta-text-tui` cannot even reach it), so a README without the claim is
/// skipped rather than required to invent one. The last assertion is what keeps that
/// skip from turning the check into a no-op: at least one crate README must still
/// state the window, and today that is `meta-text-proto`.
#[test]
fn test_the_crate_readmes_state_the_current_protocol_version() {
    let mut claiming = Vec::new();
    for name in CRATE_READMES {
        let text = document(name);
        if !text.contains("`PROTOCOL_VERSION` ") {
            continue;
        }
        claiming.push(name);
        assert_claims(
            name,
            &text,
            "`PROTOCOL_VERSION` ",
            u64::from(PROTOCOL_VERSION),
            "the protocol version the crate speaks",
            Marker::Prose,
        );
        let window = format!("`{MIN_SUPPORTED_PROTOCOL_VERSION}..={PROTOCOL_VERSION}`");
        assert!(
            text.contains(&window),
            "{name}: a README that names `PROTOCOL_VERSION` must also state the window it serves \
             as {window:?} (A52)"
        );
    }
    assert!(
        !claiming.is_empty(),
        "no member crate README states the protocol version; if {CRATE_READMES:?} were renamed or \
         reworded, update this test rather than letting it pass vacuously (A52)"
    );
}

/// The prose that describes the compatibility window matches the constants.
///
/// §6.4 states the window in words (`PROTOCOL_VERSION is 9`) and the header states
/// it as a summary (`protocol version 9 (serves 3..=9)`); both are claims about the
/// same two constants, so both are read back.
#[test]
fn test_the_documented_compatibility_window_matches_the_constants() {
    let text = document(ARCHITECTURE);
    assert_claims(
        ARCHITECTURE,
        &text,
        "`PROTOCOL_VERSION` is ",
        u64::from(PROTOCOL_VERSION),
        "`PROTOCOL_VERSION`",
        Marker::Prose,
    );
    assert_claims(
        ARCHITECTURE,
        &text,
        "`MIN_SUPPORTED_PROTOCOL_VERSION` is ",
        u64::from(MIN_SUPPORTED_PROTOCOL_VERSION),
        "`MIN_SUPPORTED_PROTOCOL_VERSION`",
        Marker::Prose,
    );
    assert_claims(
        ARCHITECTURE,
        &text,
        "`PROTOCOL_VERSION` (",
        u64::from(PROTOCOL_VERSION),
        "`PROTOCOL_VERSION`",
        Marker::Prose,
    );
    assert_claims(
        ARCHITECTURE,
        &text,
        "`MIN_SUPPORTED_PROTOCOL_VERSION` (",
        u64::from(MIN_SUPPORTED_PROTOCOL_VERSION),
        "`MIN_SUPPORTED_PROTOCOL_VERSION`",
        Marker::Prose,
    );

    // The summary line is anchored to the header rather than searched for in the
    // whole file: §5 quotes the window as well, and a hit inside an audit row would
    // keep this assertion green after the header itself went stale.
    let header = format!(
        "protocol version {PROTOCOL_VERSION} (serves {}..={PROTOCOL_VERSION})",
        MIN_SUPPORTED_PROTOCOL_VERSION
    );
    let version_line = text
        .lines()
        .find(|line| line.starts_with("Version: "))
        .unwrap_or_default();
    assert!(
        version_line.contains(&header),
        "{ARCHITECTURE}: the header says {version_line:?}, which must state the window as \
         {header:?} (A52)"
    );
}

/// The wire bounds the documents quote match the constants.
///
/// A reader sizes a buffer from these numbers: the prefix is what a foreign client
/// writes, `MAX_FRAME_LEN` is what the endpoint accepts before allocating, and the
/// message ceiling is the largest `app.max_message_length` the configuration
/// accepts. The §4 table also quotes the *default* `app.max_message_length` the
/// shipped `config.toml` sets, so that number is read from the type rather than
/// trusted.
#[test]
fn test_the_documented_wire_bounds_match_the_constants() {
    let readme = document(README);
    let prefix = format!("{FRAME_HEADER_LEN}-byte big-endian length prefix");
    assert!(
        readme.contains(&prefix),
        "{README}: the frame example must state {prefix:?} (A52)"
    );

    let architecture = document(ARCHITECTURE);
    let frame = format!("| Frame size | {} |", human_size(u64::from(MAX_FRAME_LEN)));
    assert!(
        architecture.contains(&frame),
        "{ARCHITECTURE}: the bounds table must state {frame:?} (A52)"
    );

    let ceiling = human_size(u64::try_from(MAX_MESSAGE_LENGTH).unwrap_or(u64::MAX));
    let row = format!("hard ceiling {ceiling}");
    assert!(
        architecture.contains(&row),
        "{ARCHITECTURE}: the message-length row must state {row:?} (A52)"
    );

    let default_length = format!(
        "`app.max_message_length` ({})",
        meta_text::config::AppConfig::default()
            .app
            .max_message_length
    );
    assert!(
        architecture.contains(&default_length),
        "{ARCHITECTURE}: the message-length row must state the shipped default, \
         {default_length:?} (A52)"
    );
}

/// Bytes as the documents write them: `1 MiB`, `32 KiB`.
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// The long options the README's "Useful CLI options" table documents.
///
/// Only the *first* cell of each table row is read, so a description that names
/// another flag cannot add one, and the cell is split on commas and spaces because
/// it spells the short and long form together (`-c, --config <FILE>`).
fn documented_options(readme: &str) -> Vec<String> {
    let mut options = Vec::new();
    for line in readme.lines() {
        let row = line.trim_start();
        let Some(cell) = row
            .strip_prefix("| `")
            .and_then(|rest| rest.split('`').next())
        else {
            continue;
        };
        for token in cell.split(|character: char| character == ',' || character.is_whitespace()) {
            if token.starts_with("--") {
                options.push(token.to_string());
            }
        }
    }
    options
}

/// Every option the README's table documents is one the binary accepts.
///
/// The table is the user-facing reference and the `--help` text is generated from
/// `CliArgs`, so comparing the two catches a renamed or withdrawn flag that left the
/// README describing a command line the binary no longer has — the shape of defect
/// the `--history-limit` entry in the changelog describes from the other side (a
/// documented flag that was parsed and then read by nothing). The binary is the one
/// cargo built for this package, so the check is against the shipped command line
/// rather than against a second copy of the parser.
///
/// A documented flag that needs a feature to *work* is fine here: `--help` offers it
/// either way, and the refusal is reported when the mode is used.
#[test]
fn test_every_documented_option_is_accepted_by_the_binary() {
    let readme = document(README);
    let documented = documented_options(&readme);
    assert!(
        documented.len() > 10,
        "{README}: the option table looks empty ({documented:?}); update this test with the new shape"
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_meta-text"))
        .arg("--help")
        .output()
        .expect("run the binary's --help");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "meta-text --help exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    for option in documented {
        assert!(
            help.contains(&option),
            "{README} documents {option}, which the binary's --help does not offer. Update the \
             README, or the argument if it was meant to exist"
        );
    }
}
