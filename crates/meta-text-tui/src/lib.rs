/*!
 * lib.rs
 *
 * metaText presentation: the wording, the presenter both front-ends share, the
 * terminal rendering engine and the full screen front-end.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * # Layering
 *
 * This crate depends on [`meta_text_core`] (for the client contract) and
 * [`meta_text_proto`] (for the vocabulary), and on nothing else in the
 * workspace. It cannot name a key, a database row or a socket: the only way to
 * reach the backend is [`crate::ipc::client::CoreClient`].
 *
 * # Contents
 *
 * - [`presenter`]: maps typed lines and core events onto requests and formats
 *   the replies (all user-visible wording lives here)
 * - [`text`]: static help/banner wording
 * - [`commands`]: interactive slash command parsing
 * - [`tui`]: the rendering engine and console output routing
 * - [`ui::tui`]: the full screen front-end
 *
 * The rendering engine is here rather than next to the line oriented REPL
 * because the presenter renders through it; the REPL crate therefore depends on
 * this one, never the other way round.
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

pub mod commands;
pub mod presenter;
pub mod text;
pub mod tui;
pub mod ui;

/// The contract this layer is written against.
pub use meta_text_core::ipc;
pub use meta_text_proto::{error, types, utils};

pub use ui::TuiFrontend;

/// Current version of the metaText presentation crate.
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text_tui::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
