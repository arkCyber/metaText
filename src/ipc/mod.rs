/*!
 * ipc/mod.rs
 *
 * Interface boundary between the metaText user interfaces and the core
 * application service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`protocol`]: the versioned, serializable request/response/event contract
 * - [`framing`]: bounded length-prefixed framing for stream transports
 * - [`validation`]: boundary validation of every front-end supplied value
 * - [`core`]: the headless backend service and its client handle
 * - [`client`]: an in-process or TCP client implementing the contract
 * - [`server`]: a TCP endpoint that exposes the core to out-of-process UIs
 *
 * # Architecture
 *
 * ```text
 *   ui/cli      ui/tui          presentation (no domain state)
 *      \          /
 *       \        /  ipc protocol
 *        +------+
 *        | core |  backend: crypto, storage, transport, contacts
 *        +------+
 * ```
 *
 * The presentation layer may only reach the backend through [`client`]. It
 * never touches [`crate::crypto`], [`crate::database`] or [`crate::network`]
 * directly, which keeps the two halves independently testable and lets a
 * future front-end attach over a socket without touching the backend.
 */

pub mod client;
pub mod core;
pub mod framing;
pub mod protocol;
pub mod server;
pub mod validation;

pub use client::{CoreClient, LocalClient, RemoteClient};
pub use core::{CoreHandle, CoreService, CoreServiceOptions};
pub use protocol::{
    ClientMessage, CoreEvent, ErrorCode, ErrorInfo, Reply, Request, SendOutcomeKind, SendReport,
    ServerMessage, SessionInfo, StatisticsView, PROTOCOL_VERSION,
};
