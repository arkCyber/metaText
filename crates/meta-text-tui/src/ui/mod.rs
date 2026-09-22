/*!
 * ui/mod.rs
 *
 * The full screen front-end of the metaText core service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`tui`]: the full screen interface, driven by the shared
 *   [`crate::presenter`]
 *
 * The presenter, the wording and the rendering engine live one module up
 * ([`crate::presenter`], [`crate::text`], [`crate::tui`]) because the line
 * oriented REPL in `meta-text-cli` renders through exactly the same code; this
 * module only adds the alternate-screen front-end on top of them.
 *
 * Nothing in this module may access a key, a database row or a socket — those
 * live in `meta-text-backend`, which this crate does not depend on. The only
 * way to reach the backend is
 * [`crate::ipc::client::CoreClient`]; the transport (TCP or Tox) is selected by
 * the core, so no front-end has to know which one is in use.
 */

pub mod tui;

/// Shared presentation logic and the rendering engine it draws through.
pub use crate::presenter;

pub use tui::TuiFrontend;
