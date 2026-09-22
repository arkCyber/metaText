/*!
 * message_exchange_test.rs
 *
 * Communication tests: message send/receive through the core actor.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-12
 * Version: 0.4.0
 * License: MIT
 *
 * These tests drive **two real `CoreService` actors** over the TCP transport and
 * assert on what one side receives when the other sends. They cover the cases a
 * messaging client actually has to get right:
 *
 * - a message crosses and is reported with the right body and sender,
 * - messages arrive in the order they were sent,
 * - multi-byte text and a near-limit body survive the round trip byte for byte,
 * - a message addressed to a peer that is not connected yet is *buffered* and
 *   delivered once the peer appears (the outbox flush path),
 * - both directions work on one connection.
 *
 * Everything here uses the public interface only (`meta_text::ipc`), which is
 * the same contract the CLI and TUI are held to.
 */

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::protocol::{
    ContentType, CoreEvent, ErrorCode, MessageKind, MetricsView, Reply, SendOutcomeKind,
};
use meta_text::ipc::{CoreHandle, CoreService, Request};

/// How long a test waits for a network condition before failing.
///
/// Generous on purpose: the condition is a loopback TCP handshake, but these
/// tests run in parallel with every other binary of the workspace, and a
/// machine under full load can take seconds to schedule a task. A too-tight
/// deadline turns CPU contention into a spurious "timed out waiting for both
/// peers to know each other by name".
const PATIENCE: Duration = Duration::from_secs(30);

/// One running core service with its own data directory.
struct Peer {
    /// Client handle to the actor.
    core: CoreHandle,

    /// Address other peers dial (`127.0.0.1:<port>`).
    address: SocketAddr,

    /// Keeps the temporary workspace alive for the test's lifetime.
    _dir: tempfile::TempDir,

    /// Nickname this peer announces.
    name: String,
}

/// Start a core service that announces `nickname` on an ephemeral loopback port.
async fn start_peer(nickname: &str, passphrase: &str) -> Peer {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut config = AppConfig::default();
    config.network.port = 0;
    config.app.auto_save_interval = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        nickname: Some(nickname.to_string()),
        passphrase: Some(passphrase.to_string()),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args).await.expect("core service");
    service.start().await.expect("start");
    let core = service.spawn();

    // The listener binds `0.0.0.0`, so rewrite it to a dialable loopback address.
    let Reply::Session { session } = core.request(Request::SessionInfo).await.expect("session")
    else {
        panic!("expected a session reply");
    };
    let bound = session
        .local_address
        .expect("the transport must be listening");
    let port: u16 = bound
        .rsplit_once(':')
        .expect("host:port")
        .1
        .parse()
        .expect("port");
    let address = SocketAddr::from(([127, 0, 0, 1], port));

    Peer {
        core,
        address,
        _dir: dir,
        name: nickname.to_string(),
    }
}

/// Connect `from` to `to` and wait until both sides can address each other.
///
/// "Address" is stronger than "connected" on purpose; see [`knows_peer`].
async fn connect(from: &Peer, to: &Peer) {
    from.core
        .request(Request::Connect {
            address: to.address.to_string(),
        })
        .await
        .expect("connect");

    wait_until("both peers to know each other by name", || async {
        knows_peer(&from.core, &to.name).await && knows_peer(&to.core, &from.name).await
    })
    .await;
}

/// Whether `core` has a connected peer announcing `name`.
///
/// The peer's *nickname* has to be known, not just the socket: TCP registers a
/// connection (and counts it in `connected_peers`) before the other side's
/// `hello` frame carrying the nickname is read, and the transport routes a
/// message by nickname. A send in that window is buffered and flushed as soon as
/// the hello lands — correct, documented offline behaviour, but it would report
/// `queued` where a test that just waited for `connected_peers > 0` expects
/// `sent`. Waiting for an announced nickname is therefore the real "ready to
/// message" condition.
async fn knows_peer(core: &CoreHandle, name: &str) -> bool {
    match core.request(Request::SessionInfo).await {
        Ok(Reply::Session { session }) => session
            .peer_nicknames
            .iter()
            .any(|known| known.eq_ignore_ascii_case(name)),
        _ => false,
    }
}

/// Poll `condition` until it holds, failing the test when [`PATIENCE`] runs out.
async fn wait_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + PATIENCE;
    loop {
        if condition().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Register `target` as a friend and make it the active conversation.
async fn select_peer(core: &CoreHandle, target: &str) {
    core.request(Request::AddContact {
        identifier: target.to_string(),
        note: None,
    })
    .await
    .expect("add contact");

    let Reply::Conversation { active, .. } = core
        .request(Request::SelectConversation {
            target: target.to_string(),
        })
        .await
        .expect("select conversation")
    else {
        panic!("expected a conversation reply");
    };
    assert_eq!(active.as_deref(), Some(target));
}

/// Receive the next `MessageReceived` event, skipping anything else.
async fn next_message(
    events: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
) -> (String, String, u64, MessageKind) {
    let (peer, body, wire_bytes, kind, _) = next_message_with_type(events).await;
    (peer, body, wire_bytes, kind)
}

/// Receive the next `MessageReceived` event with its content type.
async fn next_message_with_type(
    events: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
) -> (String, String, u64, MessageKind, ContentType) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match events.recv().await {
            Ok(CoreEvent::MessageReceived {
                peer,
                body,
                wire_bytes,
                kind,
                content_type,
                ..
            }) => return (peer, body, wire_bytes, kind, content_type),
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                panic!("the test lagged {skipped} event(s)")
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                panic!("the event stream closed before a message arrived")
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a message event"
        );
    }
}

/// Send `text` to the active conversation and return the delivery report.
async fn send(core: &CoreHandle, text: &str) -> meta_text::ipc::protocol::SendReport {
    match core
        .request(Request::SendMessage {
            target: None,
            text: text.to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        })
        .await
    {
        Ok(Reply::Sent { report, .. }) => report,
        other => panic!("unexpected reply to a send: {other:?}"),
    }
}

/// Send a third-person action to the active conversation.
async fn send_action(core: &CoreHandle, text: &str) -> meta_text::ipc::protocol::SendReport {
    send_typed(core, text, MessageKind::Action, ContentType::Text).await
}

/// Send a binary payload (lowercase hexadecimal) to the active conversation.
async fn send_binary(core: &CoreHandle, hex_body: &str) -> meta_text::ipc::protocol::SendReport {
    send_typed(core, hex_body, MessageKind::Text, ContentType::Binary).await
}

/// Send with an explicit kind and content type.
async fn send_typed(
    core: &CoreHandle,
    text: &str,
    kind: MessageKind,
    content_type: ContentType,
) -> meta_text::ipc::protocol::SendReport {
    match core
        .request(Request::SendMessage {
            target: None,
            text: text.to_string(),
            kind,
            content_type,
        })
        .await
    {
        Ok(Reply::Sent { report, .. }) => report,
        other => panic!("unexpected reply to a send: {other:?}"),
    }
}

/// Shut a peer down, surfacing a failed shutdown.
async fn stop(peer: &Peer) {
    peer.core.shutdown().await.expect("shutdown");
}

/// Ask a core for its operational snapshot.
async fn metrics(core: &CoreHandle) -> MetricsView {
    match core.request(Request::Metrics).await {
        Ok(Reply::Metrics { metrics }) => metrics,
        other => panic!("unexpected metrics reply: {other:?}"),
    }
}

/// A message sent on one core is received, decoded and attributed on the other.
#[tokio::test]
async fn test_a_message_crosses_between_two_cores() {
    let passphrase = "message-exchange-basic";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    // Subscribe before the message is sent so no event can be missed.
    let mut bob_events = bob.core.subscribe();

    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    let report = send(&alice.core, "hello from Alice").await;
    assert_eq!(report.outcome, SendOutcomeKind::Sent, "{report:?}");
    assert!(report.encrypted, "the TCP transport must encrypt");

    let (peer, body, wire_bytes, kind) = next_message(&mut bob_events).await;
    assert_eq!(peer, alice.name, "the sender must be attributed");
    assert_eq!(body, "hello from Alice");
    assert!(wire_bytes > 0, "the wire size must be reported");
    assert_eq!(kind, MessageKind::Text, "an ordinary message stays text");

    stop(&alice).await;
    stop(&bob).await;
}

/// Several messages arrive in the order they were sent, with no loss.
#[tokio::test]
async fn test_messages_arrive_in_order() {
    let passphrase = "message-exchange-order";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    let sent: Vec<String> = (1..=8).map(|index| format!("message #{index}")).collect();
    for text in &sent {
        let report = send(&alice.core, text).await;
        assert_eq!(report.outcome, SendOutcomeKind::Sent, "{text}: {report:?}");
    }

    for expected in &sent {
        let (peer, body, _, _) = next_message(&mut bob_events).await;
        assert_eq!(peer, alice.name);
        assert_eq!(&body, expected, "messages must keep their order");
    }

    stop(&alice).await;
    stop(&bob).await;
}

/// Multi-byte text and a near-limit body survive the round trip unchanged.
#[tokio::test]
async fn test_unicode_and_long_body_survive_the_round_trip() {
    let passphrase = "message-exchange-content";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    let limit = match alice.core.request(Request::SessionInfo).await {
        Ok(Reply::Session { session }) => session.max_message_length,
        other => panic!("unexpected session reply: {other:?}"),
    };

    let bodies = vec![
        "你好，世界 🌏".to_string(),
        "emoji: 🚀🦀🎉 mixed with ascii".to_string(),
        "line one\nline two\ttabbed".to_string(),
        // Just under the configured limit, so it must be accepted whole.
        "x".repeat(limit - 1),
    ];

    for text in &bodies {
        let report = send(&alice.core, text).await;
        assert_eq!(
            report.outcome,
            SendOutcomeKind::Sent,
            "a {} char body must be sent",
            text.chars().count()
        );
    }

    for expected in &bodies {
        let (_, body, _, _) = next_message(&mut bob_events).await;
        assert_eq!(
            &body,
            expected,
            "the body must arrive byte for byte ({} chars)",
            expected.chars().count()
        );
    }

    stop(&alice).await;
    stop(&bob).await;
}

/// Both directions work over one connection, each side attributing the other.
#[tokio::test]
async fn test_both_directions_on_one_connection() {
    let passphrase = "message-exchange-both-ways";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut alice_events = alice.core.subscribe();
    let mut bob_events = bob.core.subscribe();

    connect(&alice, &bob).await;
    // Each side has to know the other before it can address a message.
    select_peer(&alice.core, &bob.name).await;
    select_peer(&bob.core, &alice.name).await;

    send(&alice.core, "Alice -> Bob").await;
    let (peer, body, _, _) = next_message(&mut bob_events).await;
    assert_eq!(peer, alice.name);
    assert_eq!(body, "Alice -> Bob");

    send(&bob.core, "Bob -> Alice").await;
    let (peer, body, _, _) = next_message(&mut alice_events).await;
    assert_eq!(peer, bob.name);
    assert_eq!(body, "Bob -> Alice");

    stop(&alice).await;
    stop(&bob).await;
}

/// A message addressed to a peer that is not connected yet is buffered, then
/// delivered (in order) as soon as that peer appears.
///
/// This is the "send while the other side is away" case, which is what makes a
/// P2P messenger usable; it exercises the transport outbox and its flush path.
#[tokio::test]
async fn test_message_to_an_unconnected_peer_is_buffered_then_delivered() {
    let passphrase = "message-exchange-offline";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();

    // Address Bob before he is reachable at all.
    select_peer(&alice.core, &bob.name).await;

    let mut queued = Vec::new();
    for text in ["first while away", "second while away"] {
        let report = send(&alice.core, text).await;
        assert_eq!(report.outcome, SendOutcomeKind::Queued, "{report:?}");
        queued.push(text.to_string());
    }

    // `/info` shows the waiting payloads so the user is not left guessing.
    let Reply::Session { session } = alice
        .core
        .request(Request::SessionInfo)
        .await
        .expect("session")
    else {
        panic!("expected a session reply");
    };
    assert_eq!(session.queued_messages, 2, "{session:?}");

    // Bob shows up: the outbox is flushed and both messages arrive in order.
    connect(&alice, &bob).await;
    for expected in &queued {
        let (peer, body, _, _) = next_message(&mut bob_events).await;
        assert_eq!(peer, alice.name);
        assert_eq!(&body, expected);
    }

    // ...and the queue is empty again.
    wait_until("the outbox to drain", || async {
        match alice.core.request(Request::SessionInfo).await {
            Ok(Reply::Session { session }) => session.queued_messages == 0,
            _ => false,
        }
    })
    .await;

    stop(&alice).await;
    stop(&bob).await;
}

/// A second connection attempt to the same peer is idempotent, not fatal.
#[tokio::test]
async fn test_reconnecting_to_a_connected_peer_keeps_the_session_usable() {
    let passphrase = "message-exchange-reconnect";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    // Dial again, then keep talking: the session must not be poisoned.
    alice
        .core
        .request(Request::Connect {
            address: bob.address.to_string(),
        })
        .await
        .expect("a repeated connect must not fail the service");

    let report = send(&alice.core, "still here").await;
    assert!(
        matches!(
            report.outcome,
            SendOutcomeKind::Sent | SendOutcomeKind::Queued
        ),
        "{report:?}"
    );

    let (_, body, _, _) = next_message(&mut bob_events).await;
    assert_eq!(body, "still here");

    stop(&alice).await;
    stop(&bob).await;
}

/// A third-person action (`/me`) keeps its kind end to end.
///
/// The distinction is carried by the transport (a TCP frame kind, Tox's own
/// `TOX_MESSAGE_TYPE`), so a regression would silently turn every action into an
/// ordinary message.
#[tokio::test]
async fn test_action_messages_keep_their_kind() {
    let passphrase = "message-exchange-action";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    // An action and an ordinary message with the same body, to prove the kind is
    // what distinguishes them and not the text.
    let report = send_action(&alice.core, "waves").await;
    assert_eq!(report.outcome, SendOutcomeKind::Sent, "{report:?}");
    send(&alice.core, "waves").await;

    let (_, action_body, _, action_kind) = next_message(&mut bob_events).await;
    assert_eq!(action_body, "waves");
    assert_eq!(
        action_kind,
        MessageKind::Action,
        "an action must stay an action across the transport"
    );

    let (_, text_body, _, text_kind) = next_message(&mut bob_events).await;
    assert_eq!(text_body, "waves");
    assert_eq!(
        text_kind,
        MessageKind::Text,
        "an ordinary message must not become an action"
    );

    stop(&alice).await;
    stop(&bob).await;
}

/// A burst larger than any single queue still arrives complete and in order.
///
/// This exercises the whole path under load: the actor's bounded command queue,
/// the per-connection write queue, the frame cap and the receive side's decryption,
/// with nothing lost and nothing reordered.
#[tokio::test]
async fn test_a_burst_of_messages_arrives_complete() {
    const BURST: usize = 200;

    let passphrase = "message-exchange-burst";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    for index in 0..BURST {
        let report = send(&alice.core, &format!("burst #{index}")).await;
        assert_eq!(
            report.outcome,
            SendOutcomeKind::Sent,
            "#{index}: {report:?}"
        );
    }

    for index in 0..BURST {
        let (peer, body, _, kind) = next_message(&mut bob_events).await;
        assert_eq!(peer, alice.name);
        assert_eq!(
            body,
            format!("burst #{index}"),
            "message #{index} out of order"
        );
        assert_eq!(kind, MessageKind::Text);
    }

    stop(&alice).await;
    stop(&bob).await;
}

/// Messages survive a restart of the receiving side: the history is durable.
///
/// Needs a real database driver, so it only runs when one is compiled in.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_received_messages_are_in_the_history_after_a_restart() {
    let passphrase = "message-exchange-history";
    let alice = start_peer("Alice", passphrase).await;

    // Bob is started, receives, then stops and comes back on the same data dir.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.app.auto_save_interval = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();
    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        nickname: Some("Bob".to_string()),
        passphrase: Some(passphrase.to_string()),
        ..CliArgs::default()
    };

    let mut bob_service = CoreService::new(config.clone(), args.clone())
        .await
        .expect("core service");
    bob_service.start().await.expect("start");
    let bob = bob_service.spawn();
    let port = match bob.request(Request::SessionInfo).await {
        Ok(Reply::Session { session }) => session.local_address.expect("listening"),
        other => panic!("unexpected session reply: {other:?}"),
    };
    let bob_address = SocketAddr::from((
        [127, 0, 0, 1],
        port.rsplit_once(':')
            .expect("host:port")
            .1
            .parse()
            .expect("port"),
    ));

    let mut bob_events = bob.subscribe();
    alice
        .core
        .request(Request::Connect {
            address: bob_address.to_string(),
        })
        .await
        .expect("connect");
    wait_until("the peers to know each other by name", || async {
        knows_peer(&alice.core, "Bob").await && knows_peer(&bob, "Alice").await
    })
    .await;

    select_peer(&alice.core, "Bob").await;
    send(&alice.core, "persist me").await;
    let (_, body, _, _) = next_message(&mut bob_events).await;
    assert_eq!(body, "persist me");

    // Give the writer a moment to commit before the process goes away.
    wait_until("the message to reach the database", || async {
        match bob.request(Request::History { limit: Some(10) }).await {
            Ok(Reply::History { messages, .. }) => !messages.is_empty(),
            _ => false,
        }
    })
    .await;
    bob.shutdown().await.expect("shutdown");
    stop(&alice).await;

    // Restart on the same data directory: the message must still be there.
    let mut restarted = CoreService::new(config, args).await.expect("restart");
    restarted.start().await.expect("start");
    let bob = restarted.spawn();
    let Reply::History {
        persistent,
        messages,
    } = bob
        .request(Request::History { limit: Some(10) })
        .await
        .expect("history")
    else {
        panic!("expected a history reply");
    };
    assert!(persistent, "sqlite must be persistent here");
    assert!(
        messages
            .iter()
            .any(|message| message.body == "persist me" && message.direction == "in"),
        "the received message must survive a restart: {messages:?}"
    );
    stop(&Peer {
        core: bob,
        address: bob_address,
        _dir: dir,
        name: "Bob".to_string(),
    })
    .await;
}

/// `/metrics` reports what the session actually did, and its counters are
/// internally consistent (queues within capacity, no phantom sheds).
#[tokio::test]
async fn test_metrics_reflect_the_exchange() {
    let passphrase = "message-exchange-metrics";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    // A healthy session has nothing queued or shed.
    let idle = metrics(&alice.core).await;
    assert_eq!(idle.transport, "tcp");
    assert_eq!(idle.peers_connected, 1, "{idle:?}");
    assert_eq!(idle.payloads_queued, 0, "{idle:?}");
    assert_eq!(idle.payloads_expired, 0, "{idle:?}");
    assert_eq!(idle.payloads_dropped, 0, "{idle:?}");
    assert_eq!(idle.messages_sent, 0, "{idle:?}");
    assert!(
        idle.request_queue_depth <= idle.request_queue_capacity,
        "queue depth must never exceed capacity: {idle:?}"
    );
    assert!(idle.request_queue_capacity > 0);

    for index in 0..3 {
        send(&alice.core, &format!("metric #{index}")).await;
    }
    for _ in 0..3 {
        let (_, _, _, _) = next_message(&mut bob_events).await;
    }

    let after = metrics(&alice.core).await;
    assert_eq!(after.messages_sent, 3, "{after:?}");
    assert!(after.bytes_sent > 0);
    assert_eq!(after.payloads_queued, 0);

    let received = metrics(&bob.core).await;
    assert_eq!(received.messages_received, 3, "{received:?}");
    assert!(received.bytes_received > 0);
    assert_eq!(received.peers_connected, 1);

    // Only Alice added a contact, so the friend counts differ; everything that
    // describes the shared session must agree.
    assert_eq!(after.friend_count, 1, "{after:?}");
    assert_eq!(received.friend_count, 0, "{received:?}");
    assert_eq!(after.transport, received.transport);
    assert_eq!(after.persistent, received.persistent);

    stop(&alice).await;
    stop(&bob).await;
}

/// `/metrics` counts a buffered payload while it waits, and stops counting it
/// once it has been delivered.
#[tokio::test]
async fn test_metrics_track_a_buffered_payload() {
    let passphrase = "message-exchange-metrics-queue";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    select_peer(&alice.core, &bob.name).await;

    send(&alice.core, "buffered").await;
    let queued = metrics(&alice.core).await;
    assert_eq!(queued.payloads_queued, 1, "{queued:?}");
    assert_eq!(queued.payloads_expired, 0, "{queued:?}");
    assert_eq!(queued.peers_connected, 0, "{queued:?}");

    connect(&alice, &bob).await;
    let (_, body, _, _) = next_message(&mut bob_events).await;
    assert_eq!(body, "buffered");

    wait_until("the outbox to drain in the metrics", || async {
        metrics(&alice.core).await.payloads_queued == 0
    })
    .await;

    stop(&alice).await;
    stop(&bob).await;
}

/// A binary payload survives the round trip, arrives as binary, and is not
/// mangled into text or reported as undecodable.
#[tokio::test]
async fn test_binary_payload_round_trips() {
    let passphrase = "message-exchange-binary";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    let mut bob_events = bob.core.subscribe();
    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    // Bytes that are deliberately *not* valid UTF-8, so a receiver that tried to
    // read them as text would fail instead of round-tripping.
    let hex_body = "00ff10fe80";
    let report = send_binary(&alice.core, hex_body).await;
    assert_eq!(report.outcome, SendOutcomeKind::Sent, "{report:?}");

    let (peer, body, _, kind, content_type) = next_message_with_type(&mut bob_events).await;
    assert_eq!(peer, alice.name);
    assert_eq!(
        content_type,
        ContentType::Binary,
        "a binary payload must arrive as binary"
    );
    assert_eq!(kind, MessageKind::Text, "binary is never an action");
    assert_eq!(body, hex_body, "the bytes must survive byte for byte");

    // A text message with the same body length still arrives as text, so the
    // classification did not simply switch to "always binary".
    send(&alice.core, "plain").await;
    let (_, body, _, _, content_type) = next_message_with_type(&mut bob_events).await;
    assert_eq!(body, "plain");
    assert_eq!(content_type, ContentType::Text);

    stop(&alice).await;
    stop(&bob).await;
}

/// A binary body is validated before anything is sent: a malformed or oversized
/// body is refused and leaves no trace in the session.
#[tokio::test]
async fn test_invalid_binary_body_is_refused_without_side_effects() {
    let passphrase = "message-exchange-binary-reject";
    let alice = start_peer("Alice", passphrase).await;
    let bob = start_peer("Bob", passphrase).await;

    connect(&alice, &bob).await;
    select_peer(&alice.core, &bob.name).await;

    let limit = match alice.core.request(Request::SessionInfo).await {
        Ok(Reply::Session { session }) => session.max_message_length,
        other => panic!("unexpected session reply: {other:?}"),
    };
    let oversized = "00".repeat(limit + 1);

    for bad in ["0", "zz", "0g", &oversized] {
        let error = alice
            .core
            .request(Request::SendMessage {
                target: None,
                text: bad.to_string(),
                kind: MessageKind::Text,
                content_type: ContentType::Binary,
            })
            .await
            .expect_err("a malformed binary body must be refused");
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{bad}: {error:?}");
        assert!(
            error.message.contains("binary body"),
            "the error must name the field: {error:?}"
        );
    }

    // A binary action is a contradiction, not a message.
    let error = alice
        .core
        .request(Request::SendMessage {
            target: None,
            text: "00ff".to_string(),
            kind: MessageKind::Action,
            content_type: ContentType::Binary,
        })
        .await
        .expect_err("a binary action must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("action"), "{error:?}");

    // None of the refused requests changed any state.
    let Reply::Statistics { statistics } = alice
        .core
        .request(Request::Statistics)
        .await
        .expect("statistics")
    else {
        panic!("expected a statistics reply");
    };
    assert_eq!(statistics.messages_sent, 0, "{statistics:?}");
    assert_eq!(statistics.bytes_sent, 0, "{statistics:?}");
    assert_eq!(metrics(&alice.core).await.payloads_queued, 0);

    stop(&alice).await;
    stop(&bob).await;
}
