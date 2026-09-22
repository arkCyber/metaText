/*!
 * lib.rs
 *
 * metaText shared vocabulary: the wire protocol and everything the layers above
 * are allowed to name without owning a subsystem.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * This is the bottom of the workspace. It depends on no other metaText crate,
 * which is what makes the dependency rules of `docs/ARCHITECTURE.md` §2.1
 * mechanical: a crate can only reach what it declares, and nothing can reach
 * back into here from above.
 *
 * # Contents
 *
 * - [`ipc::protocol`]: the versioned, serializable request/reply/event contract
 * - [`ipc::framing`]: bounded length-prefixed framing for stream transports
 * - [`ipc::validation`]: boundary validation of every front-end supplied value
 * - [`error`]: the shared error type every layer reports through
 * - [`types`]: domain vocabulary shared by the protocol, the backend and the UIs
 * - [`utils`]: small helpers (hex, time, truncation) with no state
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

pub mod error;
pub mod ipc;
pub mod types;
pub mod utils;

/// Number of hex characters in a raw 32-byte public key (`2 * 32`).
///
/// The wire carries keys and Tox addresses as lowercase hexadecimal, so the
/// length every validator checks is the hex length, not the byte length.
pub const PUBLIC_KEY_HEX_LEN: usize = 64;

/// Number of hex characters in a Tox address (`2 * 38`).
///
/// A Tox address is a 32-byte public key plus a 4-byte nospam value plus a
/// 2-byte checksum, rendered as 76 hex characters. The protocol never carries
/// the binary form, so this is the length a front-end supplies and the length
/// every validator checks.
pub const TOX_ADDRESS_HEX_LEN: usize = 76;

pub use error::{MetaTextError, MetaTextResult};

/// Current version of the metaText protocol crate.
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text_proto::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
