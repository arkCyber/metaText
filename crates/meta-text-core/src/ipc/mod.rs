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
 * - [`core`]: the headless backend service and its client handle
 * - [`client`]: an in-process or TCP client implementing the contract
 * - [`server`]: a TCP endpoint that exposes the core to out-of-process UIs
 * - [`protocol`], [`framing`], [`validation`]: re-exported from
 *   [`meta_text_proto::ipc`], the wire contract every front-end speaks
 *
 * # Architecture
 *
 * ```text
 *   meta-text-cli   meta-text-tui     presentation (no domain state)
 *      \          /
 *       \        /  ipc protocol
 *        +------+
 *        | core |  backend: crypto, storage, transport, contacts
 *        +------+
 * ```
 *
 * The presentation layer may only reach the backend through [`client`]. The
 * subsystems it must never touch (`crypto`, `database`, `network`,
 * `transport`) are not re-exported by this crate at all, so the boundary is
 * enforced by Cargo rather than by review. That keeps the two halves
 * independently testable and lets a future front-end attach over a socket
 * without touching the backend.
 */

pub mod client;
pub mod core;
pub mod server;

pub use client::{CoreClient, LocalClient, RemoteClient};
pub use core::{CoreHandle, CoreService, CoreServiceOptions};
pub use meta_text_proto::ipc::{framing, protocol, validation};
pub use protocol::{
    ClientMessage, CoreEvent, ErrorCode, ErrorInfo, Reply, Request, SendOutcomeKind, SendReport,
    ServerMessage, SessionInfo, StatisticsView, PROTOCOL_VERSION,
};
