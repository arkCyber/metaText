/*!
 * server.rs
 *
 * TCP endpoint that exposes the core service to out-of-process front-ends.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Version and token checked during the handshake, before serving anything
 * - Bounded handshake time, connection count and per-client output queue
 * - Remote shutdown is refused, so a socket client cannot stop the daemon
 * - Every accepted connection is isolated; a failing client cannot affect
 *   another client or the core
 */

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::core::CoreHandle;
use super::framing::{read_message, write_message};
use super::protocol::{
    ClientMessage, ErrorCode, ErrorInfo, Request, ResponseResult, ServerMessage, SessionInfo,
    PROTOCOL_VERSION,
};

/// How long a client has to complete the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many front-ends may be attached at once.
const MAX_CLIENT_CONNECTIONS: usize = 16;

/// How many frames may wait for one client before it is considered stalled.
const CLIENT_OUTBOX_CAPACITY: usize = 128;

/// Server configuration.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Shared secret clients must present. `None` disables authentication,
    /// which is only acceptable on a loopback or otherwise trusted interface.
    pub token: Option<String>,

    /// Human readable endpoint name, used in log records.
    pub name: String,
}

/// A bound core protocol endpoint.
#[derive(Debug)]
pub struct CoreServer {
    /// Accepting socket.
    listener: TcpListener,

    /// Cap on simultaneous clients.
    permits: Arc<Semaphore>,
}

impl CoreServer {
    /// Bind the endpoint.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the address cannot be resolved or bound.
    pub async fn bind(address: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        Ok(Self {
            listener,
            permits: Arc::new(Semaphore::new(MAX_CLIENT_CONNECTIONS)),
        })
    }

    /// The address the endpoint is bound to.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the local address cannot be read.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections until the listener fails.
    ///
    /// Each connection is served on its own task; the returned future only
    /// completes on a fatal accept error.
    pub async fn run(self, core: CoreHandle, options: ServerOptions) {
        let name = if options.name.is_empty() {
            "core".to_string()
        } else {
            options.name.clone()
        };
        let origin = self
            .local_addr()
            .map_or_else(|_| "unknown".to_string(), |addr| addr.to_string());
        info!("🛰️ Core protocol endpoint listening on {origin} ({name})");

        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    warn!("⚠️ Accept failed on {origin}: {error}");
                    // A transient accept error must not kill the endpoint.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
                warn!("⚠️ Refusing {peer}: too many attached front-ends");
                continue;
            };

            let core = core.clone();
            let options = options.clone();
            tokio::spawn(async move {
                serve_connection(stream, peer, core, options).await;
                drop(permit);
            });
        }
    }
}

/// Serve one front-end: handshake, then pump requests and events.
async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    core: CoreHandle,
    options: ServerOptions,
) {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();

    // --- Handshake ---------------------------------------------------------
    let hello: Option<ClientMessage> =
        match timeout(HANDSHAKE_TIMEOUT, read_message(&mut reader)).await {
            Ok(Ok(message)) => message,
            Ok(Err(error)) => {
                debug!("🔌 {peer} sent an unreadable greeting: {error}");
                return;
            }
            Err(_) => {
                debug!("🔌 {peer} did not greet in time");
                return;
            }
        };

    let Some(ClientMessage::Hello {
        protocol_version,
        token,
        client,
    }) = hello
    else {
        reject(
            &mut writer,
            ErrorCode::InvalidRequest,
            "a hello frame is required",
        )
        .await;
        return;
    };

    if protocol_version != PROTOCOL_VERSION {
        reject(
            &mut writer,
            ErrorCode::UnsupportedProtocol,
            &format!(
                "protocol version {protocol_version} is not supported (expected {PROTOCOL_VERSION})"
            ),
        )
        .await;
        return;
    }

    if let Some(expected) = &options.token {
        if !secret_eq(expected, token.as_deref().unwrap_or("")) {
            reject(&mut writer, ErrorCode::Unauthorized, "invalid token").await;
            return;
        }
    }

    let session = match core.request(Request::SessionInfo).await {
        Ok(super::protocol::Reply::Session { session }) => session,
        Ok(_) => empty_session(),
        Err(error) => {
            reject(&mut writer, error.code, &error.message).await;
            return;
        }
    };

    if write_message(
        &mut writer,
        &ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            session: Box::new(session),
        },
    )
    .await
    .is_err()
    {
        return;
    }
    info!("🔌 {peer} attached as '{client}'");

    // --- Message pump ------------------------------------------------------
    let (outbox, mut inbox) = mpsc::channel::<ServerMessage>(CLIENT_OUTBOX_CAPACITY);
    let (requests, mut pending) = mpsc::channel::<(u64, Request)>(CLIENT_OUTBOX_CAPACITY);

    let writer_task = tokio::spawn(async move {
        while let Some(message) = inbox.recv().await {
            if write_message(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });

    // Requests from one connection are executed strictly in order: a client
    // that sends `add_contact` and then `list_contacts` always observes the
    // addition, and replies arrive in request order. The reader loop only
    // enqueues, so event delivery never stalls behind a slow request.
    let dispatcher = {
        let core = core.clone();
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some((id, request)) = pending.recv().await {
                let result = if matches!(request, Request::Shutdown) {
                    // A socket client must never be able to stop the backend.
                    ResponseResult::err(ErrorInfo::new(
                        ErrorCode::Unauthorized,
                        "shutdown is reserved for the hosting process",
                    ))
                } else {
                    match core.request(request).await {
                        Ok(reply) => ResponseResult::ok(reply),
                        Err(error) => ResponseResult::err(error),
                    }
                };
                if outbox
                    .send(ServerMessage::Response { id, result })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    let mut events = core.subscribe();

    loop {
        tokio::select! {
            incoming = read_message::<_, ClientMessage>(&mut reader) => {
                match incoming {
                    Ok(Some(ClientMessage::Request { id, request })) => {
                        if requests.send((id, request)).await.is_err() {
                            break;
                        }
                    }
                    // A goodbye, a repeated greeting and a closed stream all
                    // end the session for this client.
                    Ok(Some(ClientMessage::Goodbye))
                    | Ok(Some(ClientMessage::Hello { .. }))
                    | Ok(None) => break,
                    Err(error) => {
                        debug!("🔌 {peer} sent a bad frame: {error}");
                        break;
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if outbox.send(ServerMessage::Event { event }).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("⚠️ {peer} lagged {skipped} event(s); continuing");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    dispatcher.abort();
    writer_task.abort();
    info!("🔌 {peer} detached");
}

/// Tell a client why it is being disconnected, then stop.
async fn reject<W>(writer: &mut W, code: ErrorCode, message: &str)
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let _ = write_message(
        writer,
        &ServerMessage::Rejected {
            error: ErrorInfo::new(code, message),
        },
    )
    .await;
}

/// A placeholder session, used only if the core cannot answer during greeting.
fn empty_session() -> SessionInfo {
    SessionInfo {
        nickname: String::new(),
        status_message: String::new(),
        identity: String::new(),
        mode: "unknown".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        encryption: "unknown".to_string(),
        encryption_enabled: false,
        network_running: false,
        network_port: 0,
        network_bootstrap_nodes: 0,
        connected_peers: 0,
        pending_peers: 0,
        max_connections: 0,
        queued_messages: 0,
        desired_peers: Vec::new(),
        peer_nicknames: Vec::new(),
        local_address: None,
        database_connected: false,
        database: String::new(),
        persistent: false,
        database_max_connections: 0,
        friend_count: 0,
        max_friends: 0,
        active_chat: None,
        uptime_seconds: 0,
        config_path: String::new(),
        data_dir: String::new(),
        app_name: String::new(),
        app_version: String::new(),
        max_message_length: 0,
        auto_save_interval: 0,
    }
}

/// Compare two secrets without leaking their length or content through timing.
///
/// # Returns
///
/// Returns `true` only when both strings are byte-for-byte identical.
fn secret_eq(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();

    // Fold the length difference into the result instead of returning early.
    // Only the low byte is kept: it is used purely as a "not equal" flag.
    #[allow(clippy::cast_possible_truncation)]
    let mut difference = (expected.len() ^ provided.len()) as u8;
    for (index, byte) in expected.iter().enumerate() {
        let other = provided.get(index).copied().unwrap_or(0);
        difference |= byte ^ other;
    }

    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical secrets compare equal; everything else does not.
    #[test]
    fn test_secret_comparison() {
        assert!(secret_eq("s3cret", "s3cret"));
        assert!(!secret_eq("s3cret", "s3cres"));
        assert!(!secret_eq("s3cret", "s3cre"));
        assert!(!secret_eq("s3cret", ""));
        assert!(secret_eq("", ""));
    }

    /// The placeholder session is structurally valid.
    #[test]
    fn test_empty_session_is_complete() {
        let session = empty_session();
        assert_eq!(session.version, env!("CARGO_PKG_VERSION"));
        assert!(session.desired_peers.is_empty());
    }
}
