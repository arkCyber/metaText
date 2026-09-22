/*!
 * peer_chat.rs
 *
 * Application case: two instances talk — an encrypted message over TCP.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - two full `CoreService` actors in one process, each with its own data directory
 * - the peer discovery rule: a peer is addressable by *name* only once it has
 *   announced one in its `hello` frame, which is why a send waits for the
 *   announcement rather than for the socket
 * - `CoreEvent::MessageReceived`: the decrypted body and its ciphertext length
 *
 * ```bash
 * cargo run --example peer_chat
 * ```
 *
 * The same conversation happens between two terminals running the binary
 * (`cargo run -- --mode cli --port 0 --peer 127.0.0.1:34567 --passphrase …`); the
 * example does both halves in one process so every value that crosses is visible.
 * Nothing here needs more than loopback.
 */

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::{CoreEvent, CoreHandle, CoreService, Reply, Request};
use meta_text::types::{ContentType, MessageKind};

/// How long the example waits for a peer, a reply or an event.
const PATIENCE: Duration = Duration::from_secs(10);

/// One running instance plus the directory that holds its state.
struct Peer {
    /// Client handle of the actor.
    core: CoreHandle,

    /// Loopback address the listener is reachable at.
    address: SocketAddr,

    /// Nickname this instance announces.
    name: String,

    /// Keeps the temporary data directory alive for as long as the actor runs.
    _dir: tempfile::TempDir,
}

impl Peer {
    /// Ask for a reply, turning the wire error into an `anyhow` error.
    async fn request(&self, request: Request) -> Result<Reply> {
        self.core
            .request(request)
            .await
            .map_err(anyhow::Error::from)
    }

    /// Shut the actor down.
    async fn stop(&self) -> Result<()> {
        self.core
            .shutdown()
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }
}

/// Subscribe to one instance's event stream.
fn events_of(peer: &Peer) -> tokio::sync::broadcast::Receiver<CoreEvent> {
    // `CoreHandle::subscribe` cannot fail: the in-process transport always
    // streams. The `CoreClient` trait spells the same call `Option`, because a
    // socket transport may not support events.
    peer.core.subscribe()
}

/// Whether `peer` can address `name` — i.e. that name has been announced.
async fn knows(peer: &Peer, name: &str) -> bool {
    match peer.request(Request::SessionInfo).await {
        Ok(Reply::Session { session }) => session
            .peer_nicknames
            .iter()
            .any(|known| known.eq_ignore_ascii_case(name)),
        _ => false,
    }
}

/// Start an instance that announces `nickname` and shares `passphrase`.
///
/// The listener binds the wildcard address, so what it reports is not dialable as
/// printed; it is rewritten to loopback here, which is what the test suite does
/// for the same reason.
async fn start_peer(nickname: &str, passphrase: &str) -> Result<Peer> {
    let dir = tempfile::tempdir().context("create a data directory")?;

    let mut config = AppConfig::default();
    config.app.auto_save_interval = 0;
    config.network.port = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        nickname: Some(nickname.to_string()),
        passphrase: Some(passphrase.to_string()),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args)
        .await
        .with_context(|| format!("build the core service for {nickname}"))?;
    service.start().await.context("start the subsystems")?;
    let core = service.spawn();

    let Ok(Reply::Session { session }) = core.request(Request::SessionInfo).await else {
        bail!("{nickname} did not report a session");
    };
    let bound = session
        .local_address
        .context("the transport must be listening")?;
    let port: u16 = bound
        .rsplit_once(':')
        .context("the bound address is host:port")?
        .1
        .parse()
        .context("the bound address ends in a port")?;

    Ok(Peer {
        core,
        address: SocketAddr::from(([127, 0, 0, 1], port)),
        name: nickname.to_string(),
        _dir: dir,
    })
}

/// Start both instances, connect them, exchange a message in each direction.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. Both ends share the passphrase, which is what derives the session key;
    //    the identities are generated per data directory and announced.
    let alice = start_peer("Alice", "example-passphrase").await?;
    let bob = start_peer("Bob", "example-passphrase").await?;
    println!("Alice listening on {}", alice.address);
    println!("Bob   listening on {}", bob.address);

    // 2. Subscribe before dialling: the events below are read after the message
    //    is sent, so both receivers must already exist.
    let mut alice_events = events_of(&alice);
    let mut bob_events = events_of(&bob);

    // 3. Dial. Start order does not matter — an unreachable address becomes a
    //    *desired peer* and is retried in the background — but here both ends
    //    are already listening.
    alice
        .request(Request::Connect {
            address: bob.address.to_string(),
        })
        .await?;

    // 4. Wait for the announcement, not for "connected": on TCP the socket is
    //    counted one frame before the nickname arrives, and a send inside that
    //    window is buffered for a peer that is still nameless. A failure here prints
    //    what each end actually saw, because "timed out" alone cannot distinguish a
    //    refused dial from a missing nickname from a starved actor.
    if !wait_until("both peers to know each other by name", || async {
        knows(&alice, &bob.name).await && knows(&bob, &alice.name).await
    })
    .await
    {
        bail!(
            "the peers did not see each other within {PATIENCE:?} — Alice: {}, Bob: {}",
            peer_summary(&alice).await?,
            peer_summary(&bob).await?
        );
    }
    println!("handshake done: Alice sees Bob, Bob sees Alice");

    // 5. Send. The transport encrypts the body; the reply reports what it did
    //    with the ciphertext instead of a bare "ok".
    let Reply::Sent { target, report } = alice
        .request(Request::SendMessage {
            target: Some(bob.name.clone()),
            text: "hello over loopback".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        })
        .await?
    else {
        bail!("expected a send report");
    };
    println!(
        "→ {target}: hello over loopback ({} bytes on the wire, outcome {:?})",
        report.wire_bytes, report.outcome
    );

    // 6. The other end sees the decrypted body and the ciphertext length. An
    //    observer of the wire sees only the latter.
    let (peer, body, wire_bytes) = next_message(&mut bob_events).await?;
    println!("← {peer}: {body} ({wire_bytes} bytes on the wire)");

    // 7. The other direction, to show the key is agreed per pair rather than
    //    owned by the dialler.
    bob.request(Request::SendMessage {
        target: Some(alice.name.clone()),
        text: "hello back".to_string(),
        kind: MessageKind::Text,
        content_type: ContentType::Text,
    })
    .await?;
    let (peer, body, _) = next_message(&mut alice_events).await?;
    println!("← {peer}: {body}");

    // 8. Each end pins what the other announced, so `/peers` can show it and a
    //    change under a known nickname is reported rather than silently accepted.
    let Reply::Session { session } = alice.request(Request::SessionInfo).await? else {
        bail!("expected a session reply");
    };
    for pinned in &session.peer_identities {
        println!(
            "pinned {}: {} (changed: {})",
            pinned.nickname, pinned.fingerprint, pinned.changed
        );
    }

    // 9. Stop both actors. Order does not matter here either: a peer that has
    //    gone away is re-dialled when it comes back.
    alice.stop().await?;
    bob.stop().await?;
    Ok(())
}

/// Wait until `condition` holds.
///
/// Returns `false` when [`PATIENCE`] runs out; the caller decides what to report,
/// because it is the caller that can say what each end saw.
async fn wait_until<F, Fut>(what: &str, mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !condition().await {
        if tokio::time::Instant::now() >= deadline {
            eprintln!("timed out waiting for {what}");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    true
}

/// One line describing what an instance reports about its peers.
///
/// Used on the failure path: the difference between "the dial never landed", "the
/// name never arrived" and "the address is still being retried" is exactly what an
/// operator needs, and all three otherwise look like the same timeout.
async fn peer_summary(peer: &Peer) -> Result<String> {
    let Ok(Reply::Session { session }) = peer.request(Request::SessionInfo).await else {
        return Ok("no session reply".to_string());
    };
    Ok(format!(
        "connected={} pending={} known={:?} desired={:?}",
        session.connected_peers,
        session.pending_peers,
        session.peer_nicknames,
        session.desired_peers
    ))
}

/// Await the next `MessageReceived` event: sender, body, ciphertext length.
async fn next_message(
    events: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
) -> Result<(String, String, u64)> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .context("no message arrived before the deadline")?
            .context("the event stream closed")?;
        if let CoreEvent::MessageReceived {
            peer,
            body,
            wire_bytes,
            ..
        } = event
        {
            return Ok((peer, body, wire_bytes));
        }
    }
}
