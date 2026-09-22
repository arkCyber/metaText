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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::core::CoreHandle;
use super::framing::{read_message, write_message};
use super::protocol::{
    is_supported_protocol, ClientMessage, ErrorCode, ErrorInfo, Request, ResponseResult,
    ServerMessage, SessionInfo, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION,
};

/// How long a client has to complete the handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many front-ends may be attached at once.
const MAX_CLIENT_CONNECTIONS: usize = 16;

/// How many frames may wait for one client before it is considered stalled.
const CLIENT_OUTBOX_CAPACITY: usize = 128;

/// Default sustained request rate one client may use, per second.
///
/// `0` disables the limit. The value is far above what an interactive front-end
/// needs, so it only ever bites a misbehaving or hostile client.
///
/// Re-exported from [`meta_text_backend::config`], which owns the default it
/// backs (a value in a configuration file is the same number), so the endpoint
/// and the configuration cannot drift apart.
pub use meta_text_backend::config::DEFAULT_REQUESTS_PER_SECOND;

/// Default burst any client may spend above the sustained rate.
pub use meta_text_backend::config::DEFAULT_REQUEST_BURST;

/// How many rejected requests a client may accumulate before it is detached.
const RATE_LIMIT_STRIKES: u32 = 256;

/// How long an idle client address keeps its spent allowance.
///
/// The point of a per-address budget is that *reconnecting* does not hand out a
/// fresh allowance, so an entry has to outlive the connection that created it. It
/// cannot live forever: a remote peer chooses its source address, so the table has
/// to stay bounded (see [`MAX_TRACKED_PEERS`]). Ten minutes is long enough that a
/// reconnect loop gains nothing — it is still the same budget — and short enough
/// that a burst of one-off addresses ages out on its own.
const BUDGET_IDLE_TTL: Duration = Duration::from_secs(600);

/// How many client addresses are tracked at once.
///
/// The table is keyed by a value a remote peer controls, and the endpoint is
/// reachable from outside a loopback interface, so it must never grow without
/// bound. When it is full the least recently seen address is dropped; a *new*
/// address is therefore always served, which matters because refusing new arrivals
/// would let an attacker who fills the table deny service to everybody else.
const MAX_TRACKED_PEERS: usize = 1024;

/// A leaky token bucket that bounds how much CPU one client can spend.
///
/// The bounded queues of §3.7 bound *memory*, not the work a client can make the
/// core do; without this a socket client could keep the actor busy with rejected
/// requests. The bucket is deliberately pure (it takes `now` as an argument) so
/// the policy is unit-testable without sleeping.
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Sustained requests per second; `0` disables the limit.
    rate: u32,

    /// Maximum tokens that may accumulate.
    burst: u32,

    /// Tokens currently available.
    tokens: f64,

    /// When the bucket was last refilled.
    last: Instant,
}

impl TokenBucket {
    /// Create a full bucket.
    fn new(rate: u32, burst: u32) -> Self {
        let burst = if rate == 0 { 0 } else { burst.max(1) };
        Self {
            rate,
            burst,
            tokens: f64::from(burst),
            last: Instant::now(),
        }
    }

    /// Take one token, refilling according to the elapsed time.
    ///
    /// # Returns
    ///
    /// Returns `true` when the request may be served, `false` when the client has
    /// to slow down.
    fn try_acquire(&mut self, now: Instant) -> bool {
        if self.rate == 0 {
            return true;
        }

        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = elapsed
            .mul_add(f64::from(self.rate), self.tokens)
            .min(f64::from(self.burst));

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// One client address's spent allowance.
#[derive(Debug)]
struct AddressBudget {
    /// The bucket itself.
    bucket: TokenBucket,

    /// When this address was last seen, used for expiry and eviction.
    last_seen: Instant,
}

/// Request budgets, one per client address.
///
/// The limiter used to be per *connection*, which a client could reset simply by
/// reconnecting: a flood that was shed on one socket was free to start again on the
/// next. Keying the budget by the peer's address closes that — reconnecting keeps
/// the same, already spent bucket — at the cost of two front-ends on one host
/// sharing a budget, which is what [`DEFAULT_REQUESTS_PER_SECOND`] is sized for and
/// what the `[ipc]` section exists to raise.
///
/// The table is bounded in both directions a remote peer can push it: idle entries
/// expire after [`BUDGET_IDLE_TTL`], and a full table evicts the least recently
/// seen address.
#[derive(Debug)]
struct PeerBudgets {
    /// Sustained requests per second per address; `0` disables the limit.
    rate: u32,

    /// Burst an address may spend above the sustained rate.
    burst: u32,

    /// How long an unseen address keeps its budget.
    ttl: Duration,

    /// How many addresses may be tracked at once.
    capacity: usize,

    /// The budgets, keyed by client address.
    entries: HashMap<IpAddr, AddressBudget>,
}

impl PeerBudgets {
    /// Create an empty table for the given rate.
    fn new(rate: u32, burst: u32) -> Self {
        Self::with_limits(rate, burst, BUDGET_IDLE_TTL, MAX_TRACKED_PEERS)
    }

    /// Create a table with explicit limits, so the expiry and eviction paths can be
    /// tested without waiting ten minutes or filling a thousand entries.
    fn with_limits(rate: u32, burst: u32, ttl: Duration, capacity: usize) -> Self {
        Self {
            rate,
            burst,
            ttl,
            capacity: capacity.max(1),
            entries: HashMap::new(),
        }
    }

    /// Take one token from `peer`'s budget, creating it on first contact.
    ///
    /// # Returns
    ///
    /// Returns `true` when the request may be served, `false` when this address has
    /// to slow down.
    fn try_acquire(&mut self, peer: IpAddr, now: Instant) -> bool {
        if self.rate == 0 {
            return true;
        }

        self.sweep(now);
        if !self.entries.contains_key(&peer) && self.entries.len() >= self.capacity {
            self.evict_oldest();
        }

        let budget = self.entries.entry(peer).or_insert_with(|| AddressBudget {
            bucket: TokenBucket::new(self.rate, self.burst),
            last_seen: now,
        });
        budget.last_seen = now;
        budget.bucket.try_acquire(now)
    }

    /// Drop the addresses that have not been seen for `ttl`.
    fn sweep(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, budget| now.saturating_duration_since(budget.last_seen) < ttl);
    }

    /// Drop the least recently seen address, so a new one always fits.
    fn evict_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, budget)| budget.last_seen)
            .map(|(peer, _)| *peer);
        if let Some(peer) = oldest {
            self.entries.remove(&peer);
        }
    }

    /// How many addresses are tracked (test and diagnostic use).
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.entries.len()
    }
}

/// Lock the budget table, treating a poisoned mutex as usable.
///
/// A poisoned table means some connection panicked while holding the lock. The only
/// mutations are an insert, a retain and a remove, all of which leave the table
/// consistent, so refusing every request from then on would turn one panic into an
/// outage.
fn lock_budgets(budgets: &Mutex<PeerBudgets>) -> MutexGuard<'_, PeerBudgets> {
    budgets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Server configuration.
///
/// `Default` is implemented by hand rather than derived: a derived one would set
/// `handshake_timeout` to zero, i.e. reject every client that is not already
/// waiting with its greeting.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Shared secret clients must present. `None` disables authentication,
    /// which is only acceptable on a loopback or otherwise trusted interface.
    pub token: Option<String>,

    /// Human readable endpoint name, used in log records.
    pub name: String,

    /// How long a freshly accepted connection may stay silent before it is
    /// dropped. Defaults to [`DEFAULT_HANDSHAKE_TIMEOUT`]; tests lower it so
    /// they never depend on wall-clock scheduling under load.
    pub handshake_timeout: Duration,

    /// Sustained requests per second a client may use (`0` disables the limit).
    pub requests_per_second: u32,

    /// Burst a client may spend above the sustained rate.
    pub request_burst: u32,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            token: None,
            name: "meta-text".to_string(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            requests_per_second: DEFAULT_REQUESTS_PER_SECOND,
            request_burst: DEFAULT_REQUEST_BURST,
        }
    }
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
        if options.requests_per_second == 0 {
            warn!("⚠️ The request rate limit is disabled (requests_per_second = 0)");
        }

        // One budget table for the whole endpoint: the limiter is per client
        // *address*, so a client cannot reset its allowance by reconnecting.
        let budgets = Arc::new(Mutex::new(PeerBudgets::new(
            options.requests_per_second,
            options.request_burst,
        )));

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
            let budgets = Arc::clone(&budgets);
            tokio::spawn(async move {
                serve_connection(stream, peer, core, options, budgets).await;
                drop(permit);
            });
        }
    }
}

/// Serve one front-end: handshake, then pump requests and events.
///
/// One function rather than a struct with steps: the handshake, the per-client
/// rate limit and the two-way pump share the connection's state (writer, token
/// bucket, outbox), and splitting them would mean passing that state around or
/// holding it in a type whose only purpose is to be split again.
#[allow(clippy::too_many_lines)]
async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    core: CoreHandle,
    options: ServerOptions,
    budgets: Arc<Mutex<PeerBudgets>>,
) {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();

    // --- Handshake ---------------------------------------------------------
    let hello: Option<ClientMessage> =
        match timeout(options.handshake_timeout, read_message(&mut reader)).await {
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

    if !is_supported_protocol(protocol_version) {
        reject(
            &mut writer,
            ErrorCode::UnsupportedProtocol,
            &format!(
                "protocol version {protocol_version} is not supported \
                 (this build serves {MIN_SUPPORTED_PROTOCOL_VERSION}..={PROTOCOL_VERSION})"
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
    // `None` means "this request was shed by the rate limiter"; it travels
    // through the same queue as real requests so replies keep their order.
    let (requests, mut pending) = mpsc::channel::<(u64, Option<Request>)>(CLIENT_OUTBOX_CAPACITY);

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
                let result = match request {
                    // Shed by the rate limiter: reported, not silently dropped,
                    // so a client can back off instead of timing out.
                    None => ResponseResult::err(ErrorInfo::new(
                        ErrorCode::Backpressure,
                        "request rate limit exceeded; slow down",
                    )),
                    // A socket client must never be able to stop the backend.
                    Some(Request::Shutdown) => ResponseResult::err(ErrorInfo::new(
                        ErrorCode::Unauthorized,
                        "shutdown is reserved for the hosting process",
                    )),
                    Some(request) => match core.request(request).await {
                        Ok(reply) => ResponseResult::ok(reply),
                        Err(error) => ResponseResult::err(error),
                    },
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
    let mut strikes = 0_u32;

    // The reader runs in its own task because `read_message` is **not** cancel
    // safe: it reads a length prefix and then the body, so a `select!` that drops
    // it between those reads silently discards bytes and desynchronises the
    // client's stream. Selecting on the read directly (as this loop used to) broke
    // connections whenever an outbound event won the race.
    let (inbound_tx, mut inbound_rx) =
        mpsc::channel::<std::io::Result<Option<ClientMessage>>>(CLIENT_OUTBOX_CAPACITY);
    let reader_task = tokio::spawn(async move {
        loop {
            let message = read_message::<_, ClientMessage>(&mut reader).await;
            // A decoded message may be followed by more; end of stream and read
            // errors are terminal.
            let terminal = !matches!(message, Ok(Some(_)));
            if inbound_tx.send(message).await.is_err() || terminal {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            incoming = inbound_rx.recv() => {
                match incoming {
                    Some(Ok(Some(ClientMessage::Request { id, request }))) => {
                        // Rate limit before the request reaches the actor: the
                        // bounded queue limits memory, this limits the work a
                        // client can make the core do. The budget belongs to the
                        // client's *address*, so reconnecting does not reset it.
                        if lock_budgets(&budgets).try_acquire(peer.ip(), Instant::now()) {
                            if requests.send((id, Some(request))).await.is_err() {
                                break;
                            }
                        } else {
                            strikes += 1;
                            if requests.send((id, None)).await.is_err() {
                                break;
                            }
                            if strikes >= RATE_LIMIT_STRIKES {
                                warn!("⚠️ {peer} exceeded the request rate limit; detaching");
                                break;
                            }
                        }
                    }
                    // A goodbye, a repeated greeting, a closed stream and a lost
                    // connection all end the session for this client.
                    Some(Ok(Some(ClientMessage::Goodbye | ClientMessage::Hello { .. }) | None))
                    | None => break,
                    Some(Err(error)) => {
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

    reader_task.abort();
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
        transport: "unknown".to_string(),
        public_identity: None,
        pending_requests: 0,
        supports_friend_requests: false,
        groups: 0,
        pending_group_invites: 0,
        supports_groups: false,
        identity_fingerprint: String::new(),
        peer_identities: Vec::new(),
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

    // Fold the length difference into the result instead of returning early,
    // and keep it at full width: truncating to `u8` would let two secrets of
    // different length compare equal whenever the lengths differ by a multiple
    // of 256 and the longer one starts with the shorter one.
    let mut difference = expected.len() ^ provided.len();

    // Walk the longer of the two, treating missing bytes as zero. Every byte is
    // still visited, so the comparison does not get faster on a prefix match.
    let longest = expected.len().max(provided.len());
    for index in 0..longest {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = provided.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }

    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget table for a single address that may send one request per second.
    fn tight_budgets() -> PeerBudgets {
        PeerBudgets::with_limits(1, 1, Duration::from_secs(600), MAX_TRACKED_PEERS)
    }

    /// Addresses, so the tests do not have to build one by hand.
    fn address(last: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, last])
    }

    /// Reconnecting does not hand out a fresh allowance: the budget follows the
    /// address, not the connection.
    ///
    /// This is the behaviour the per-connection limiter could not provide — a flood
    /// that was shed on one socket was free to start again on the next.
    #[test]
    fn test_a_budget_follows_the_address_not_the_connection() {
        let mut budgets = tight_budgets();
        let start = Instant::now();

        // The burst is one, so the first request passes and the second is shed.
        assert!(budgets.try_acquire(address(1), start));
        assert!(!budgets.try_acquire(address(1), start));

        // A new connection from the same address — a new `try_acquire` call is all
        // the endpoint does for one — is still the spent budget.
        assert!(
            !budgets.try_acquire(address(1), start),
            "reconnecting must not refill the allowance"
        );

        // Another address has its own allowance, so one client cannot starve the
        // rest of the host's front-ends.
        assert!(budgets.try_acquire(address(2), start));
        assert!(!budgets.try_acquire(address(2), start));

        // Time, on the other hand, does refill it: one second buys one request.
        let later = start + Duration::from_secs(1);
        assert!(budgets.try_acquire(address(1), later));
    }

    /// An idle address is forgotten, so the table cannot grow by sitting still.
    #[test]
    fn test_an_idle_address_is_forgotten() {
        let mut budgets =
            PeerBudgets::with_limits(1, 1, Duration::from_secs(60), MAX_TRACKED_PEERS);
        let start = Instant::now();

        assert!(budgets.try_acquire(address(1), start));
        assert_eq!(budgets.tracked(), 1);

        // Half a second later the address is still remembered, and still spent: the
        // refill is one token per second, so nothing accumulates. (The offsets stay
        // under a second on purpose: a full second would hand out a fresh token and
        // prove nothing about the table.)
        assert!(!budgets.try_acquire(address(1), start + Duration::from_millis(500)));
        assert_eq!(budgets.tracked(), 1);

        // A minute of silence expires the entry: it was last seen at +500 ms, so
        // the sweep has to be past +60.5 s (the clock advances on every request).
        budgets.sweep(start + Duration::from_secs(61));
        assert_eq!(budgets.tracked(), 0, "the idle address must be gone");

        // ...so the next request from it starts from a full bucket again.
        assert!(budgets.try_acquire(address(1), start + Duration::from_secs(61)));
        assert_eq!(budgets.tracked(), 1);
    }

    /// A full table evicts the least recently seen address, so a new client is
    /// always served — and the evicted one starts again.
    #[test]
    fn test_a_full_table_evicts_the_least_recently_seen_address() {
        let mut budgets = PeerBudgets::with_limits(1, 1, Duration::from_secs(600), 2);
        let start = Instant::now();
        // Every offset below is under a second, so no bucket refills and each
        // assertion is about the table rather than about elapsed time.
        let soon = |millis| start + Duration::from_millis(millis);

        assert!(budgets.try_acquire(address(1), soon(0)));
        assert!(budgets.try_acquire(address(2), soon(100)));
        assert_eq!(budgets.tracked(), 2);

        // A third address cannot be served without making room, and the oldest of
        // the two is the one that goes.
        assert!(budgets.try_acquire(address(3), soon(200)));
        assert_eq!(budgets.tracked(), 2, "the table stays bounded");

        // A tracked address keeps its spent budget...
        assert!(!budgets.try_acquire(address(3), soon(250)));
        // ...while the evicted one starts again, which is the trade-off of a bounded
        // table: it is the price of never refusing a new address.
        assert!(budgets.try_acquire(address(1), soon(250)));
    }

    /// `requests_per_second = 0` means "no limit", and tracks nothing at all.
    #[test]
    fn test_a_zero_rate_disables_the_limit() {
        let mut budgets = PeerBudgets::with_limits(0, 0, Duration::from_secs(60), 1);
        let start = Instant::now();

        for _ in 0..100 {
            assert!(budgets.try_acquire(address(1), start));
            assert!(budgets.try_acquire(address(2), start));
        }
        assert_eq!(budgets.tracked(), 0, "a disabled limit needs no state");
    }

    /// Identical secrets compare equal; everything else does not.
    #[test]
    fn test_secret_comparison() {
        assert!(secret_eq("s3cret", "s3cret"));
        assert!(!secret_eq("s3cret", "s3cres"));
        assert!(!secret_eq("s3cret", "s3cre"));
        assert!(!secret_eq("s3cret", ""));
        assert!(secret_eq("", ""));
    }

    /// A longer secret that starts with the expected one is still rejected, even
    /// when the extra length is a multiple of 256 (which a `u8` fold would lose).
    #[test]
    fn test_secret_comparison_rejects_length_multiple_of_256() {
        let expected = "token";

        let plus_256 = format!("{expected}{}", "x".repeat(256));
        assert!(
            !secret_eq(expected, &plus_256),
            "a 256 byte longer secret must not be accepted"
        );

        let plus_512 = format!("{expected}{}", "x".repeat(512));
        assert!(!secret_eq(expected, &plus_512));

        // The reverse direction (provided shorter than expected) too.
        assert!(!secret_eq(&plus_256, expected));
        assert!(!secret_eq(&plus_512, expected));
    }

    /// The placeholder session is structurally valid.
    #[test]
    fn test_empty_session_is_complete() {
        let session = empty_session();
        assert_eq!(session.version, env!("CARGO_PKG_VERSION"));
        assert!(session.desired_peers.is_empty());
    }

    /// A rate of zero disables the limiter.
    #[test]
    fn test_token_bucket_disabled_never_limits() {
        let mut bucket = TokenBucket::new(0, 0);
        let now = Instant::now();
        for _ in 0..1_000 {
            assert!(bucket.try_acquire(now));
        }
    }

    /// The bucket allows a burst, then refuses until time has passed.
    #[test]
    fn test_token_bucket_allows_burst_then_refills() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(10, 3);

        // Three tokens are available immediately.
        for index in 0..3 {
            assert!(bucket.try_acquire(start), "burst token #{index} must pass");
        }
        assert!(!bucket.try_acquire(start), "the burst must be exhausted");

        // 200 ms at 10/s refills two tokens. The margin absorbs the tiny delay
        // between `new` (which stamps `last`) and `start`.
        assert!(bucket.try_acquire(start + Duration::from_millis(200)));

        // A long idle period refills only up to the burst, never beyond it.
        let later = start + Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(bucket.try_acquire(later));
        }
        assert!(!bucket.try_acquire(later), "tokens must be capped at burst");
    }
}
