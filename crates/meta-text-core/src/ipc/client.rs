/*!
 * client.rs
 *
 * Client side of the metaText core protocol.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - [`CoreClient`]: the single trait a front-end programs against
 * - [`LocalClient`]: in-process transport backed by a [`CoreHandle`]
 * - [`RemoteClient`]: TCP transport for out-of-process front-ends
 * - Per-request deadlines so a hung peer cannot wedge a user interface
 */

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, warn};

use super::core::CoreHandle;
use super::framing::{read_message, write_message};
use super::protocol::{
    ClientMessage, CoreEvent, ErrorCode, ErrorInfo, Reply, Request, ResponseResult, ServerMessage,
    PROTOCOL_VERSION,
};

/// Default deadline for a single request.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the handshake to complete.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The contract every front-end is written against.
///
/// Implementations must be cheap to share across tasks. A transport that
/// cannot stream events returns `None` from [`CoreClient::subscribe`].
#[allow(async_fn_in_trait)]
pub trait CoreClient: Send + Sync {
    /// Perform one request and await its reply.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorInfo`] when the transport fails or the service
    /// rejects the request. A rejected request never aborts the session
    /// unless [`ErrorInfo::severity`] is critical.
    async fn request(&self, request: Request) -> Result<Reply, ErrorInfo>;

    /// Subscribe to the event stream, when the transport supports it.
    fn subscribe(&self) -> Option<broadcast::Receiver<CoreEvent>> {
        None
    }
}

/// In-process transport: the core runs on the same tokio runtime.
#[derive(Debug, Clone)]
pub struct LocalClient {
    /// Backend handle.
    core: CoreHandle,
}

impl LocalClient {
    /// Wrap a [`CoreHandle`].
    #[must_use]
    pub const fn new(core: CoreHandle) -> Self {
        Self { core }
    }

    /// Access the underlying handle (for shutdown or diagnostics).
    #[must_use]
    pub const fn handle(&self) -> &CoreHandle {
        &self.core
    }
}

impl From<CoreHandle> for LocalClient {
    fn from(core: CoreHandle) -> Self {
        Self::new(core)
    }
}

impl CoreClient for LocalClient {
    async fn request(&self, request: Request) -> Result<Reply, ErrorInfo> {
        self.core.request(request).await
    }

    fn subscribe(&self) -> Option<broadcast::Receiver<CoreEvent>> {
        Some(self.core.subscribe())
    }
}

/// Out-of-process transport over TCP.
///
/// A background task owns both halves of the socket: it serialises outgoing
/// frames and routes incoming ones, so callers never touch the stream.
pub struct RemoteClient {
    /// Correlation counter.
    next_id: AtomicU64,

    /// Pending replies keyed by correlation id.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,

    /// Outgoing frame queue (bounded: overload is surfaced to the caller).
    outbox: mpsc::Sender<ClientMessage>,

    /// Event fan-out.
    events: broadcast::Sender<CoreEvent>,

    /// Set when the connection is gone.
    closed: Arc<AtomicBool>,

    /// Deadline applied to every request.
    request_timeout: Duration,
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteClient")
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl RemoteClient {
    /// Connect to a core server and complete the handshake.
    ///
    /// # Arguments
    ///
    /// * `address` - `host:port` of the core server.
    /// * `token` - Shared secret; must match the server's token.
    /// * `client_name` - Free-form name used for server side logging.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorInfo`] when the socket cannot be opened, the
    /// handshake times out, or the server rejects the connection.
    pub async fn connect(
        address: &str,
        token: Option<String>,
        client_name: &str,
    ) -> Result<Self, ErrorInfo> {
        let stream = timeout(DEFAULT_HANDSHAKE_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| {
                ErrorInfo::new(
                    ErrorCode::Timeout,
                    format!("connecting to {address} timed out"),
                )
            })?
            .map_err(|error| {
                ErrorInfo::new(
                    ErrorCode::Network,
                    format!("cannot reach {address}: {error}"),
                )
            })?;

        let (mut reader, mut writer) = stream.into_split();

        write_message(
            &mut writer,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client: client_name.to_string(),
            },
        )
        .await
        .map_err(|error| {
            ErrorInfo::new(ErrorCode::Network, format!("handshake failed: {error}"))
        })?;

        let first: Option<ServerMessage> =
            timeout(DEFAULT_HANDSHAKE_TIMEOUT, read_message(&mut reader))
                .await
                .map_err(|_| ErrorInfo::new(ErrorCode::Timeout, "the handshake reply timed out"))?
                .map_err(|error| {
                    ErrorInfo::new(ErrorCode::Network, format!("handshake failed: {error}"))
                })?;

        match first {
            Some(ServerMessage::Welcome { .. }) => {}
            Some(ServerMessage::Rejected { error }) => return Err(error),
            Some(other) => {
                return Err(ErrorInfo::new(
                    ErrorCode::UnsupportedProtocol,
                    format!("unexpected handshake reply: {other:?}"),
                ))
            }
            None => {
                return Err(ErrorInfo::new(
                    ErrorCode::Network,
                    "the server closed the connection during the handshake",
                ))
            }
        }

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (events, _) = broadcast::channel::<CoreEvent>(super::core::DEFAULT_EVENT_CAPACITY);
        let (outbox, inbox) = mpsc::channel::<ClientMessage>(super::core::DEFAULT_COMMAND_CAPACITY);

        let client = Self {
            next_id: AtomicU64::new(1),
            pending: Arc::clone(&pending),
            outbox,
            events: events.clone(),
            closed: Arc::clone(&closed),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        };

        tokio::spawn(pump(reader, writer, inbox, pending, events, closed));
        Ok(client)
    }

    /// Override the per-request deadline.
    #[must_use]
    pub const fn with_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Ask the server to close the connection politely.
    pub async fn goodbye(&self) {
        let _ = self.outbox.send(ClientMessage::Goodbye).await;
    }

    /// Drop a pending entry after a failure.
    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }
}

impl CoreClient for RemoteClient {
    async fn request(&self, request: Request) -> Result<Reply, ErrorInfo> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ErrorInfo::new(
                ErrorCode::Network,
                "the connection to the core service is closed",
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().map_err(|_| {
                ErrorInfo::critical(ErrorCode::Internal, "the pending reply table is poisoned")
            })?;
            pending.insert(id, reply_tx);
        }

        if self
            .outbox
            .send(ClientMessage::Request { id, request })
            .await
            .is_err()
        {
            self.forget(id);
            return Err(ErrorInfo::new(
                ErrorCode::Network,
                "the connection to the core service is closed",
            ));
        }

        match timeout(self.request_timeout, reply_rx).await {
            Ok(Ok(result)) => result.into_result(),
            // The sender was dropped, which is what the pump does when it shuts down and
            // clears the table. A request that inserted its entry *after* that clear — the
            // narrow window between the pump deciding to stop and a concurrent caller
            // reading `closed` — is not covered by it, and is cleaned up by the deadline
            // below instead, so the table cannot grow; the cost is that such a caller waits
            // for its deadline rather than failing at once, which is why the check at the
            // top of this function is the common path.
            Ok(Err(_)) => Err(ErrorInfo::new(
                ErrorCode::Network,
                "the core service closed before replying",
            )),
            Err(_) => {
                self.forget(id);
                Err(ErrorInfo::new(
                    ErrorCode::Timeout,
                    "the core service did not reply in time",
                ))
            }
        }
    }

    fn subscribe(&self) -> Option<broadcast::Receiver<CoreEvent>> {
        Some(self.events.subscribe())
    }
}

/// How many decoded inbound messages may wait before the reader is paused.
const INBOUND_QUEUE_CAPACITY: usize = 128;

/// Own both halves of the socket: write outgoing frames, route incoming ones.
///
/// The read side runs in its own task. `read_message` is **not** cancel safe: it
/// reads a length prefix and then the body, so dropping the future between those
/// two reads silently discards bytes and desynchronises the stream. Selecting on
/// it directly (as this loop used to) corrupted the connection whenever an
/// outgoing write happened to win the race, which is exactly what happens under
/// concurrency. The loop below only selects on channels, both cancel safe.
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    mut outbox: mpsc::Receiver<ClientMessage>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    events: broadcast::Sender<CoreEvent>,
    closed: Arc<AtomicBool>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let (inbound_tx, mut inbound_rx) =
        mpsc::channel::<std::io::Result<Option<ServerMessage>>>(INBOUND_QUEUE_CAPACITY);

    let reader_task = tokio::spawn(async move {
        loop {
            let message = read_message::<_, ServerMessage>(&mut reader).await;
            // A decoded message may be followed by more; end of stream and
            // read errors are terminal.
            let terminal = !matches!(message, Ok(Some(_)));
            if inbound_tx.send(message).await.is_err() || terminal {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            outgoing = outbox.recv() => {
                let Some(message) = outgoing else { break };
                if let Err(error) = write_message(&mut writer, &message).await {
                    debug!("🔌 Core connection write failed: {error}");
                    break;
                }
            }
            incoming = inbound_rx.recv() => {
                match incoming {
                    Some(Ok(Some(ServerMessage::Response { id, result }))) => {
                        if let Ok(mut table) = pending.lock() {
                            if let Some(reply) = table.remove(&id) {
                                let _ = reply.send(result);
                            }
                        }
                    }
                    Some(Ok(Some(ServerMessage::Event { event }))) => {
                        let _ = events.send(event);
                    }
                    Some(Ok(Some(ServerMessage::Rejected { error }))) => {
                        warn!("🔌 Core connection rejected: {error}");
                        break;
                    }
                    // A second Welcome is a protocol violation.
                    Some(Ok(Some(ServerMessage::Welcome { .. }))) => {
                        warn!("🔌 Core connection received an unexpected second welcome");
                        break;
                    }
                    // A closed stream and a lost connection both end the pump.
                    Some(Ok(None)) | None => break,
                    Some(Err(error)) => {
                        debug!("🔌 Core connection read failed: {error}");
                        break;
                    }
                }
            }
        }
    }

    reader_task.abort();

    // Fail every in-flight request instead of letting callers hang.
    closed.store(true, Ordering::SeqCst);
    if let Ok(mut table) = pending.lock() {
        table.clear();
    }
}
