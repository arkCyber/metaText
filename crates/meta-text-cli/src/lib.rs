/*!
 * lib.rs
 *
 * metaText line oriented REPL front-end.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * This is the smallest layer of the workspace: it reads lines, hands them to
 * the presenter that `meta-text-tui` owns and asks the core to shut down when
 * the loop ends. It holds no domain state and cannot name a key, a row or a
 * socket.
 */

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    // `clippy::cargo` minus `multiple_crate_versions`: the duplicate
    // `wasi`/`getrandom` versions arrive transitively (SQLx and uuid pin
    // different majors) and are unreachable from our code, so the lint only
    // reports a dependency's choice. The metadata lints stay on — they are what
    // caught the member crates' missing README/keywords/categories.
    clippy::cargo_common_metadata,
    clippy::negative_feature_names,
    clippy::redundant_feature_names,
    clippy::wildcard_dependencies,
    // Panic-prone constructs are rejected in the shipped paths: a fault travels as
    // an error value, not as an unwind, because a front end that panics takes the
    // session (and the user's terminal) with it. CI turns warnings into errors, so
    // this list is a gate rather than a suggestion.
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used,
    rust_2018_idioms
)]
// A test may assert by unwrapping: the gate above guards the shipped paths.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod ui;

/// The contract this layer is written against.
pub use meta_text_core::ipc;
pub use meta_text_proto::{error, types, utils};
/// Shared presentation logic owned by `meta-text-tui`.
pub use meta_text_tui::{presenter, tui};

pub use ui::CliFrontend;

/// Current version of the metaText REPL crate.
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text_cli::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
