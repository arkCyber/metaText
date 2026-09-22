/*!
 * ipc/mod.rs
 *
 * The transport independent half of the metaText core protocol: the vocabulary a
 * front-end and the backend agree on, the framing that carries it and the
 * validation that guards its boundaries.
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
 *
 * # Architecture
 *
 * ```text
 *   meta-text-cli   meta-text-tui     presentation (no domain state)
 *      \          /
 *       \        /  ipc protocol   <- this crate
 *        +------+
 *        | core |  backend: crypto, storage, transport, contacts
 *        +------+
 * ```
 *
 * Nothing in this crate knows a transport, a database or a key: it is data plus
 * the rules for reading it, which is what lets a front-end be compiled (and
 * published) without the backend and lets the backend be tested without a
 * terminal.
 */

pub mod framing;
pub mod protocol;
pub mod validation;

pub use protocol::{
    ClientMessage, CoreEvent, ErrorCode, ErrorInfo, Reply, Request, SendOutcomeKind, SendReport,
    ServerMessage, SessionInfo, StatisticsView, PROTOCOL_VERSION,
};
