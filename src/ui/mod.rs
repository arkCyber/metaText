/*!
 * ui/mod.rs
 *
 * Presentation layer: the CLI and TUI front-ends and their shared logic.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`presenter`]: maps key presses and typed lines onto core requests and
 *   formats the replies (all wording lives here)
 * - [`text`]: static help/banner wording
 * - [`cli`]: the line oriented REPL
 * - [`tui`]: the full screen interface
 *
 * Nothing in this module may access [`crate::crypto`], [`crate::database`] or
 * [`crate::network`]; the only way to reach the backend is
 * [`crate::ipc::client::CoreClient`].
 */

pub mod cli;
pub mod presenter;
pub mod text;
pub mod tui;

pub use cli::CliFrontend;
pub use presenter::Presenter;
pub use tui::TuiFrontend;
