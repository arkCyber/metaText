/*!
 * tox_core_transport_test.rs
 *
 * Integration tests for driving the Tox transport through the core actor.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-12
 * Version: 0.4.0
 * License: MIT
 *
 * These tests use only the public interface of the `meta_text` crate. They show
 * that selecting `--transport tox` makes the *same* core service (and therefore
 * the same history, contacts and TUI) run over Tox: no separate front-end is
 * involved any more.
 *
 * A machine without `libtoxcore` cannot run them, so they skip when
 * [`meta_text::tox::is_linked`] is false instead of failing the build.
 */

#![cfg(feature = "tox-protocol")]

use std::path::Path;

use meta_text::cli::{CliArgs, Transport};
use meta_text::config::AppConfig;
use meta_text::ipc::protocol::{
    ContentType, ErrorCode, MessageKind, Reply, Request, SendOutcomeKind,
};
use meta_text::ipc::{CoreHandle, CoreService};

/// Build and start a core service on the Tox transport.
async fn started_tox_service(data_dir: &Path) -> Option<CoreHandle> {
    if !meta_text::tox::is_linked() {
        eprintln!("skipping: this build has no usable libtoxcore");
        return None;
    }

    let mut config = AppConfig::default();
    config.network.port = 0;
    config.app.auto_save_interval = 0;
    config.database.connection_string = data_dir.join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        transport: Transport::Tox,
        data_dir: Some(data_dir.to_path_buf()),
        nickname: Some("ToxTester".to_string()),
        port: Some(0),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args).await.expect("core service");
    service.start().await.expect("start");
    Some(service.spawn())
}

/// Ask the core for a reply and unwrap it.
async fn ask(core: &CoreHandle, request: Request) -> Reply {
    core.request(request).await.expect("reply")
}

/// Serialise the tests that use the public DHT.
///
/// Each one boots two full toxcore instances and waits on the public network, so
/// running them at the same time makes them compete for CPU and for DHT
/// round-trips — and the waits they encode are timing sensitive, because an event
/// that is merely *late* is indistinguishable from one that never arrived (a
/// friend request that misses a 120 s budget fails the run). `cargo test` executes
/// test functions in parallel by default, so the tests take this lock themselves
/// rather than relying on `--test-threads=1` being remembered.
///
/// The lock is a `tokio` mutex because it is held across `.await` by an async test.
///
/// Serialising is necessary but not sufficient: a batch of the four still misses the
/// budget now and then, because the public DHT rate-limits a host that bootstraps
/// repeatedly. The failure moves between the tests from run to run and disappears
/// when one is run on its own, so re-run the named test by itself before treating a
/// batch failure as a regression.
#[cfg(feature = "tox-protocol")]
fn dht_guard() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The session report identifies Tox and exposes the address a peer can add.
#[tokio::test]
async fn test_tox_transport_is_reported_in_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(core) = started_tox_service(dir.path()).await else {
        return;
    };

    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };

    assert_eq!(session.transport, "tox");
    assert!(session.network_running);
    assert!(session.supports_friend_requests);

    let address = session
        .public_identity
        .expect("Tox must expose its address");
    assert_eq!(address.len(), meta_text::cli::TOX_ADDRESS_HEX_LEN);
    assert!(address.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(session.local_address.as_deref(), Some(address.as_str()));

    core.shutdown().await.expect("shutdown");
}

/// The identity a peer saved keeps working after a restart.
#[tokio::test]
async fn test_tox_identity_is_stable_across_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");

    let Some(first) = started_tox_service(dir.path()).await else {
        return;
    };
    let Reply::Session { session } = ask(&first, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    let first_address = session.public_identity.expect("address");
    assert_eq!(first_address.len(), meta_text::cli::TOX_ADDRESS_HEX_LEN);
    first.shutdown().await.expect("shutdown");

    // toxcore persists the identity in its own savedata under the data dir.
    assert!(dir.path().join("tox-savedata.bin").exists());

    let second = started_tox_service(dir.path()).await.expect("second start");
    let Reply::Session { session } = ask(&second, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(
        session.public_identity.as_deref(),
        Some(first_address.as_str()),
        "the Tox address must survive a restart"
    );
    second.shutdown().await.expect("shutdown");
}

/// A peer address is validated against the selected transport.
#[tokio::test]
async fn test_tox_connect_validates_the_address_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(core) = started_tox_service(dir.path()).await else {
        return;
    };

    // A host:port is the tcp shape and must be refused here.
    let error = core
        .request(Request::Connect {
            address: "127.0.0.1:34567".to_string(),
        })
        .await
        .expect_err("a host:port is not a Tox address");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("Tox address"), "{}", error.message);

    // A well formed but unusable address is reported by the transport, not by
    // validation: changing the last checksum character makes toxcore refuse it.
    // (An all-zero address is *not* a good probe: its checksum is trivially
    // valid, because the XOR of all zero bytes is zero.)
    let peer_dir = tempfile::tempdir().expect("peer tempdir");
    let peer_config = meta_text::tox::ToxConfig::new(peer_dir.path(), "Peer");
    let peer = meta_text::tox::ToxClient::start(peer_config).expect("peer start");

    let mut characters: Vec<char> = peer.address().chars().collect();
    let last = characters.len() - 1;
    characters[last] = if characters[last] == '0' { '1' } else { '0' };
    let broken: String = characters.into_iter().collect();

    let error = core
        .request(Request::Connect { address: broken })
        .await
        .expect_err("a corrupt address must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{}", error.message);

    core.shutdown().await.expect("shutdown");
    peer.shutdown().expect("peer shutdown");
}

/// Adding a friend on Tox sends a request; the friend is known immediately.
#[tokio::test]
async fn test_tox_add_contact_sends_a_friend_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(core) = started_tox_service(dir.path()).await else {
        return;
    };

    // A second instance supplies a genuinely valid Tox address.
    let peer_dir = tempfile::tempdir().expect("peer tempdir");
    let peer_config = meta_text::tox::ToxConfig::new(peer_dir.path(), "Peer");
    let peer = meta_text::tox::ToxClient::start(peer_config).expect("peer start");
    let address = peer.address().to_string();

    let reply = ask(
        &core,
        Request::AddContact {
            identifier: address.clone(),
            note: None,
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::ContactAdded {
            index: 1,
            identifier: address.clone(),
        }
    );

    // The transport now knows the pending friend, so `/peers` has something to
    // report even before the DHT connects them.
    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.pending_peers, 1, "{session:?}");
    assert!(session.desired_peers.iter().any(|entry| entry == &address));

    // The friend is registered but cannot be reached offline, so a message is
    // buffered instead of being dropped: the Tox transport keeps the same
    // "queued for later" contract as the TCP one.
    ask(
        &core,
        Request::SelectConversation {
            target: "1".to_string(),
        },
    )
    .await;
    let Reply::Sent { report, .. } = ask(
        &core,
        Request::SendMessage {
            target: None,
            text: "are you there?".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await
    else {
        panic!("expected a send report");
    };
    assert_eq!(
        report.outcome,
        SendOutcomeKind::Queued,
        "an offline Tox friend must be queued, not dropped: {report:?}"
    );
    assert_eq!(report.queue_position, Some(1));

    // `/info` reports the buffered payload so the user knows it is waiting.
    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.queued_messages, 1, "{session:?}");

    core.shutdown().await.expect("shutdown");
    peer.shutdown().expect("peer shutdown");
}

/// The friend-request vocabulary is present but empty for a fresh instance.
#[tokio::test]
async fn test_tox_peer_requests_start_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(core) = started_tox_service(dir.path()).await else {
        return;
    };

    assert_eq!(
        ask(&core, Request::PeerRequests).await,
        Reply::PeerRequests {
            requests: Vec::new()
        }
    );

    let error = core
        .request(Request::AcceptPeerRequest {
            public_key: "A".repeat(meta_text::cli::PUBLIC_KEY_HEX_LEN),
        })
        .await
        .expect_err("no such request");
    assert_eq!(error.code, ErrorCode::Network);
    assert!(error.message.contains("no pending friend request"));

    // Refusing one is the same kind of lookup, and a refusal that matches nothing
    // says so instead of silently pretending the request was discarded.
    let error = core
        .request(Request::RejectPeerRequest {
            public_key: "A".repeat(meta_text::cli::PUBLIC_KEY_HEX_LEN),
        })
        .await
        .expect_err("no such request");
    assert_eq!(error.code, ErrorCode::Network);
    assert!(
        error.message.contains("no pending friend request"),
        "{error:?}"
    );

    // An empty key is refused before the transport is asked.
    let error = core
        .request(Request::RejectPeerRequest {
            public_key: "   ".to_string(),
        })
        .await
        .expect_err("an empty key must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest, "{error:?}");

    core.shutdown().await.expect("shutdown");
}

/// A full friend request → accept → message exchange between two core services.
///
/// Ignored by default because it needs outbound UDP to reach the public Tox DHT,
/// which a sandboxed CI runner does not have. Run it explicitly:
///
/// ```bash
/// cargo test --all-features --test tox_core_transport_test -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires the public Tox DHT"]
async fn test_two_cores_exchange_a_message_over_tox() {
    use meta_text::ipc::protocol::CoreEvent;
    use std::time::{Duration, Instant};

    let _serial = dht_guard().lock().await;

    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let Some(alice) = started_tox_service(dir_a.path()).await else {
        return;
    };
    let Some(bob) = started_tox_service(dir_b.path()).await else {
        return;
    };

    // Subscribe before anything is sent so no event is missed.
    let mut alice_events = alice.subscribe();
    let mut bob_events = bob.subscribe();

    let address_of = |core: &CoreHandle| {
        let core = core.clone();
        async move {
            let Reply::Session { session } =
                core.request(Request::SessionInfo).await.expect("reply")
            else {
                panic!("expected a session reply");
            };
            session.public_identity.expect("tox address")
        }
    };

    let bob_address = address_of(&bob).await;
    ask(
        &alice,
        Request::AddContact {
            identifier: bob_address,
            note: None,
        },
    )
    .await;

    // Alice talks to Bob by the public key she stored for the contact.
    ask(
        &alice,
        Request::SelectConversation {
            target: "1".to_string(),
        },
    )
    .await;

    // Bob has not accepted yet, so this is the "peer is away" case: it must be
    // buffered (or already delivered, if LAN discovery beat us to it) and must
    // arrive once the friends connect. This is the outbox flush end to end.
    let away = "queued while you were away";
    let Reply::Sent { report, .. } = ask(
        &alice,
        Request::SendMessage {
            target: None,
            text: away.to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await
    else {
        panic!("expected a send report");
    };
    assert!(
        matches!(
            report.outcome,
            SendOutcomeKind::Queued | SendOutcomeKind::Sent
        ),
        "a message to an unconnected friend must be queued or already sent: {report:?}"
    );

    // Bob has to see the request and accept it before the friends can connect.
    let deadline = Instant::now() + Duration::from_secs(120);
    let public_key = loop {
        if let Ok(Reply::PeerRequests { requests }) = bob.request(Request::PeerRequests).await {
            if let Some(request) = requests.first() {
                break request.public_key.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the friend request"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    ask(
        &bob,
        Request::AcceptPeerRequest {
            public_key: public_key.clone(),
        },
    )
    .await;
    // Accepting consumes the request: a second accept has nothing to act on, which
    // is what keeps the bounded pending list honest.
    let Reply::PeerRequests { requests } = ask(&bob, Request::PeerRequests).await else {
        panic!("expected a requests reply");
    };
    assert!(
        requests.is_empty(),
        "an accepted request must leave the list: {requests:?}"
    );

    // Both sides must report the other as connected before a message can flow.
    let connect_deadline = deadline;
    let connected = move |core: CoreHandle| {
        let core = core.clone();
        async move {
            loop {
                if let Ok(Reply::Session { session }) = core.request(Request::SessionInfo).await {
                    if session.connected_peers > 0 {
                        return true;
                    }
                }
                if Instant::now() >= connect_deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    assert!(
        connected(alice.clone()).await,
        "Alice never saw Bob connect"
    );

    let Reply::Sent { report, .. } = ask(
        &alice,
        Request::SendMessage {
            target: None,
            text: "hello over tox".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await
    else {
        panic!("expected a send report");
    };
    assert_eq!(
        report.outcome,
        meta_text::ipc::protocol::SendOutcomeKind::Sent
    );

    // Bob must observe both bodies, the buffered one first: the outbox is
    // flushed before the message that was sent after the connection came up.
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while received.len() < 2 {
        match bob_events.try_recv() {
            Ok(CoreEvent::MessageReceived { body, .. }) => received.push(body),
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("Bob lagged {skipped} event(s)")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("Bob's event stream closed")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "Bob only received {received:?} before the timeout"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    assert_eq!(
        received,
        vec![away.to_string(), "hello over tox".to_string()],
        "both the buffered and the live message must arrive, in order"
    );

    let _ = alice_events.try_recv();
    alice.shutdown().await.expect("shutdown");
    bob.shutdown().await.expect("shutdown");
}

/// Refusing a friend request over the real DHT: the request leaves the list and no
/// friendship is formed.
///
/// The happy path of [`Request::RejectPeerRequest`] cannot be reached offline — a
/// pending request only exists once a real peer sends one — so it lives here.
/// Nothing is sent to the requester (toxcore has no such message: a request is a
/// proposal, and only an accept makes it a friendship), which is why the
/// observable effect is local: the entry is gone and no contact was created.
#[tokio::test]
#[ignore = "requires the public Tox DHT"]
async fn test_two_cores_exchange_and_refuse_a_friend_request_over_tox() {
    use std::time::{Duration, Instant};

    let _serial = dht_guard().lock().await;
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let Some(alice) = started_tox_service(dir_a.path()).await else {
        return;
    };
    let Some(bob) = started_tox_service(dir_b.path()).await else {
        return;
    };

    let bob_address = {
        let Reply::Session { session } = ask(&bob, Request::SessionInfo).await else {
            panic!("expected a session reply");
        };
        session.public_identity.expect("tox address")
    };
    ask(
        &alice,
        Request::AddContact {
            identifier: bob_address,
            note: None,
        },
    )
    .await;

    // Bob waits for the request to arrive, then refuses it.
    let deadline = Instant::now() + Duration::from_secs(120);
    let public_key = loop {
        if let Ok(Reply::PeerRequests { requests }) = bob.request(Request::PeerRequests).await {
            if let Some(request) = requests.first() {
                break request.public_key.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the friend request"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    let Reply::PeerRequestRejected {
        public_key: rejected,
    } = ask(
        &bob,
        Request::RejectPeerRequest {
            public_key: public_key.clone(),
        },
    )
    .await
    else {
        panic!("expected a rejected reply");
    };
    assert_eq!(
        rejected, public_key,
        "the rejected key must be reported back"
    );

    let Reply::PeerRequests { requests } = ask(&bob, Request::PeerRequests).await else {
        panic!("expected a requests reply");
    };
    assert!(
        requests.is_empty(),
        "the rejected request must leave the list: {requests:?}"
    );

    // Refusing twice is not a silent success: a front-end showing a request that is
    // already gone has to find out.
    let error = bob
        .request(Request::RejectPeerRequest {
            public_key: public_key.clone(),
        })
        .await
        .expect_err("a rejected request must not be rejectable twice");
    assert!(
        error.message.contains("no pending friend request"),
        "{error:?}"
    );

    // The refusal created no contact on Bob's side, so there is nothing that could
    // ever connect: rejecting is a decision, not a deferred accept.
    let Reply::Session { session } = ask(&bob, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.friend_count, 0, "{session:?}");
    assert_eq!(session.pending_requests, 0, "{session:?}");
    assert_eq!(session.connected_peers, 0, "{session:?}");

    alice.shutdown().await.expect("shutdown");
    bob.shutdown().await.expect("shutdown");
}

/// A pending friend request survives a restart of the core, and is still
/// answerable afterwards.
///
/// This is the only place the request half of the Tox store can be proved end to
/// end: an incoming request exists as a callback and *nothing else* — `toxcore`
/// keeps no record of it in the savedata — so only a second instance over the
/// real DHT can produce one (the same reason the refusal test above is ignored
/// by default).
///
/// The read happens immediately after the restart, before the DHT can even
/// reconnect: a request that were merely re-delivered by the requester's client
/// could not be there yet, so what the test observes is the store being read.
#[tokio::test]
#[ignore = "requires the public Tox DHT"]
async fn test_a_pending_friend_request_survives_a_restart_over_tox() {
    use std::time::{Duration, Instant};

    let _serial = dht_guard().lock().await;
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let Some(alice) = started_tox_service(dir_a.path()).await else {
        return;
    };
    let Some(bob) = started_tox_service(dir_b.path()).await else {
        return;
    };

    let bob_address = {
        let Reply::Session { session } = ask(&bob, Request::SessionInfo).await else {
            panic!("expected a session reply");
        };
        session.public_identity.expect("tox address")
    };
    ask(
        &alice,
        Request::AddContact {
            identifier: bob_address,
            note: None,
        },
    )
    .await;

    // Bob waits for the request: it is the only way one can exist.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (public_key, message) = loop {
        if let Ok(Reply::PeerRequests { requests }) = bob.request(Request::PeerRequests).await {
            if let Some(request) = requests.first() {
                break (request.public_key.clone(), request.message.clone());
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the friend request"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    // Restart Bob over the same data directory: the toxcore savedata survives
    // because it always did, and the pending request now has to come from the
    // store.
    bob.shutdown().await.expect("shutdown");
    let bob = started_tox_service(dir_b.path())
        .await
        .expect("Bob must restart over the same data directory");
    assert!(
        dir_b.path().join("tox-state.json").exists(),
        "the request must be persisted next to the savedata"
    );

    let Reply::PeerRequests { requests } = ask(&bob, Request::PeerRequests).await else {
        panic!("expected a requests reply");
    };
    assert_eq!(
        requests.len(),
        1,
        "the pending request must survive a restart: {requests:?}"
    );
    assert_eq!(requests[0].public_key, public_key);
    assert_eq!(
        requests[0].message, message,
        "the message the requester wrote must survive too"
    );

    // The point of keeping it: it can still be answered.
    let Reply::PeerRequestAccepted {
        public_key: accepted,
    } = ask(
        &bob,
        Request::AcceptPeerRequest {
            public_key: public_key.clone(),
        },
    )
    .await
    else {
        panic!("expected an accepted reply");
    };
    assert_eq!(accepted, public_key);

    // An answered request must not come back either, or the store would offer a
    // reply to something that is already a friendship.
    bob.shutdown().await.expect("shutdown");
    let bob = started_tox_service(dir_b.path())
        .await
        .expect("Bob must restart over the same data directory");
    let Reply::PeerRequests { requests } = ask(&bob, Request::PeerRequests).await else {
        panic!("expected a requests reply");
    };
    assert!(
        requests.is_empty(),
        "an accepted request must not be restored: {requests:?}"
    );
    let Reply::Session { session } = ask(&bob, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(
        session.friend_count, 1,
        "the accepted request is a friendship that survives a restart: {session:?}"
    );
    assert_eq!(session.pending_requests, 0, "{session:?}");

    alice.shutdown().await.expect("shutdown");
    bob.shutdown().await.expect("shutdown");
}

/// A group invitation carried over the real DHT: invite → list → join → send.
///
/// This is the half of group chat that cannot be reached without a second peer.
/// It covers [`Request::GroupInvites`] specifically: a token only exists once a
/// friend invites us, and that request is the only way a front-end that attached
/// *after* the `GroupInviteReceived` event can still see (and join) it.
///
/// Ignored by default for the same reason as the message test above; run it with
///
/// ```bash
/// cargo test --all-features --test tox_core_transport_test -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires the public Tox DHT"]
async fn test_two_cores_exchange_a_group_invitation_over_tox() {
    use meta_text::ipc::protocol::CoreEvent;
    use std::time::{Duration, Instant};

    let _serial = dht_guard().lock().await;
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let Some(alice) = started_tox_service(dir_a.path()).await else {
        return;
    };
    let Some(bob) = started_tox_service(dir_b.path()).await else {
        return;
    };
    let mut bob_events = bob.subscribe();

    let address_of = |core: &CoreHandle| {
        let core = core.clone();
        async move {
            let Reply::Session { session } =
                core.request(Request::SessionInfo).await.expect("reply")
            else {
                panic!("expected a session reply");
            };
            session.public_identity.expect("tox address")
        }
    };
    let bob_address = address_of(&bob).await;

    // Become friends first: a conference invitation travels inside the friend
    // connection, so it cannot be delivered before that exists.
    ask(
        &alice,
        Request::AddContact {
            identifier: bob_address.clone(),
            note: None,
        },
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(120);
    let public_key = loop {
        if let Ok(Reply::PeerRequests { requests }) = bob.request(Request::PeerRequests).await {
            if let Some(request) = requests.first() {
                break request.public_key.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the friend request"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    ask(&bob, Request::AcceptPeerRequest { public_key }).await;

    let connect_deadline = deadline;
    let peer_connected = move |core: CoreHandle| {
        let core = core.clone();
        async move {
            loop {
                if let Ok(Reply::Session { session }) = core.request(Request::SessionInfo).await {
                    if session.connected_peers > 0 {
                        return true;
                    }
                }
                if Instant::now() >= connect_deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    assert!(
        peer_connected(alice.clone()).await && peer_connected(bob.clone()).await,
        "the two cores never became friends"
    );

    // Alice creates a group; nobody has invited Bob yet, so his invitation list is
    // empty even though his transport can host groups.
    let Reply::GroupCreated { group } = ask(
        &alice,
        Request::CreateGroup {
            title: Some("Team".to_string()),
        },
    )
    .await
    else {
        panic!("expected a group-created reply");
    };
    let Reply::GroupInvites { supported, invites } = ask(&bob, Request::GroupInvites).await else {
        panic!("expected a group-invites reply");
    };
    assert!(supported, "Tox supports groups");
    assert!(invites.is_empty(), "nothing invited Bob yet: {invites:?}");

    // Invite Bob by his address and let him read the token from the *request*,
    // which is the path a front-end that attached later depends on.
    ask(
        &alice,
        Request::InviteToGroup {
            group_id: group.id.clone(),
            peer: bob_address.clone(),
        },
    )
    .await;
    let token = loop {
        if let Ok(Reply::GroupInvites { invites, .. }) = bob.request(Request::GroupInvites).await {
            if let Some(invite) = invites.first() {
                break invite.token.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the group invitation"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    // Declining discards the token without joining anything: this is the answer
    // the invitation side was missing, and the reason the pending list can be kept
    // honest without accepting everything in it.
    let Reply::GroupInviteDeclined { token: declined } = ask(
        &bob,
        Request::DeclineGroupInvite {
            token: token.clone(),
        },
    )
    .await
    else {
        panic!("expected a group-invite-declined reply");
    };
    assert_eq!(declined, token, "the reply names the discarded token");
    let error = bob
        .request(Request::JoinGroup {
            token: token.clone(),
        })
        .await
        .expect_err("a declined token must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{error:?}");
    assert!(
        error
            .message
            .contains("no pending group invitation matches"),
        "{error:?}"
    );
    let Reply::GroupInvites { invites, .. } = ask(&bob, Request::GroupInvites).await else {
        panic!("expected a group-invites reply");
    };
    assert!(
        invites.is_empty(),
        "a declined invitation leaves the list: {invites:?}"
    );
    // Nothing was created either: declining is not a way into the conference.
    let Reply::Groups {
        groups: bob_groups, ..
    } = ask(&bob, Request::Groups).await
    else {
        panic!("expected a groups reply");
    };
    assert!(
        bob_groups.iter().all(|joined| joined.id != group.id),
        "declining must not join the group: {bob_groups:?}"
    );

    // Declining is *not* an implicit block, exactly like refusing a friend
    // request: the inviter can invite again and the invitation is usable again.
    //
    // The token it comes back with may well be the same one: toxcore derives the
    // conference cookie from the (friend, conference) pair, so re-inviting the
    // same friend to the same conference reuses it (measured — see
    // `docs/ARCHITECTURE.md` §5.1, and the reason the bridge de-duplicates a
    // repeated invitation). What must hold is that the invitation is *back* and
    // that joining it works.
    ask(
        &alice,
        Request::InviteToGroup {
            group_id: group.id.clone(),
            peer: bob_address,
        },
    )
    .await;
    let token = loop {
        if let Ok(Reply::GroupInvites { invites, .. }) = bob.request(Request::GroupInvites).await {
            if let Some(invite) = invites.first() {
                break invite.token.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bob never received the group invitation again"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    // Joining spends the token exactly once: the same token cannot be replayed.
    let Reply::GroupJoined { group: joined } = ask(
        &bob,
        Request::JoinGroup {
            token: token.clone(),
        },
    )
    .await
    else {
        panic!("expected a group-joined reply");
    };
    assert_eq!(joined.id, group.id, "both sides must agree on the group id");
    let error = bob
        .request(Request::JoinGroup {
            token: token.clone(),
        })
        .await
        .expect_err("a spent token must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{error:?}");
    assert!(
        error.message.contains("no pending group invitation"),
        "{error:?}"
    );

    // The conference handshake has to finish before a message can cross, and
    // `Group::joined` has to become true for that to be observable at all.
    async fn wait_for_group(
        core: &CoreHandle,
        wanted: &str,
        deadline: Instant,
    ) -> Result<String, String> {
        let mut last = String::from("no group list reply");
        loop {
            if let Ok(Reply::Groups { groups, .. }) = core.request(Request::Groups).await {
                last = format!("{groups:?}");
                if groups.iter().any(|group| {
                    group.id.eq_ignore_ascii_case(wanted) && group.joined && group.members >= 2
                }) {
                    return Ok(last);
                }
            }
            if Instant::now() >= deadline {
                return Err(last);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let group_deadline = Instant::now() + Duration::from_secs(120);
    let alice_groups = wait_for_group(&alice, &group.id, group_deadline).await;
    let bob_groups = wait_for_group(&bob, &group.id, group_deadline).await;
    assert!(
        alice_groups.is_ok() && bob_groups.is_ok(),
        "the conference never completed its handshake on both sides: \
         alice={alice_groups:?} bob={bob_groups:?}"
    );

    // Alice sends to the group; Bob must see it attributed to that group.
    let reply = ask(
        &alice,
        Request::SendGroupMessage {
            group_id: Some(group.id.clone()),
            text: "hello group over tox".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await;
    assert!(
        matches!(reply, Reply::GroupSent { .. }),
        "expected a group-sent reply, got {reply:?}"
    );

    let mut received = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while received.is_none() {
        match bob_events.try_recv() {
            Ok(CoreEvent::GroupMessageReceived { group_id, body, .. }) => {
                if group_id.eq_ignore_ascii_case(&group.id) {
                    received = Some(body);
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("Bob lagged {skipped} event(s)")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("Bob's event stream closed")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "Bob never saw the group message");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    assert_eq!(received.as_deref(), Some("hello group over tox"));

    // Renaming is conference-wide state, and this is the only path that proves the
    // peer-side `conference_title` callback is wired: our own instance gets no
    // event for its own change, so a unit test could never see it.
    let Reply::GroupRenamed { group: renamed, .. } = ask(
        &alice,
        Request::RenameGroup {
            group_id: Some(group.id.clone()),
            name: "Renamed over tox".to_string(),
        },
    )
    .await
    else {
        panic!("expected a group-renamed reply");
    };
    assert_eq!(renamed, "Renamed over tox");

    let mut renamed_seen = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !renamed_seen {
        match bob_events.try_recv() {
            Ok(CoreEvent::GroupChanged {
                group_id,
                group: name,
                ..
            }) => {
                if group_id.eq_ignore_ascii_case(&group.id) && name == "Renamed over tox" {
                    renamed_seen = true;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("Bob lagged {skipped} event(s)")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("Bob's event stream closed")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "Bob never saw the group being renamed"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

    alice.shutdown().await.expect("shutdown");
    bob.shutdown().await.expect("shutdown");
}
/// The whole group lifecycle over the real Tox transport, without a DHT.
///
/// Creating, listing, sending to and leaving a conference are all local
/// operations in toxcore, so this covers the transport, the core and the protocol
/// for real — only *reaching another peer* needs the network, and that is covered
/// by the ignored two-instance tests.
#[tokio::test]
async fn test_group_lifecycle_over_the_tox_transport() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(core) = started_tox_service(dir.path()).await else {
        return;
    };

    // A transport that can host groups says so, and starts with none.
    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert!(session.supports_groups, "Tox must support groups");
    assert_eq!(session.groups, 0);
    assert_eq!(session.pending_group_invites, 0);

    let Reply::Groups { supported, groups } = ask(&core, Request::Groups).await else {
        panic!("expected a groups reply");
    };
    assert!(supported);
    assert!(groups.is_empty(), "{groups:?}");

    // Create one: the identifier is the transport's (64 hex characters) and the
    // title is what was asked for.
    let Reply::GroupCreated { group } = ask(
        &core,
        Request::CreateGroup {
            title: Some("Team".to_string()),
        },
    )
    .await
    else {
        panic!("expected a group-created reply");
    };
    assert_eq!(group.name, "Team");
    assert_eq!(group.id.len(), 64, "{group:?}");
    assert!(
        group.id.chars().all(|c| c.is_ascii_hexdigit()),
        "a group id must be hexadecimal: {group:?}"
    );
    assert_eq!(group.short_id.chars().filter(|c| *c == '…').count(), 1);

    // The transport is the source of truth for the list, so the created group is
    // visible through a fresh request too.
    let Reply::Groups { groups, .. } = ask(&core, Request::Groups).await else {
        panic!("expected a groups reply");
    };
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].id, group.id);
    assert_eq!(groups[0].name, "Team");

    // The group is part of the session state, so `/info` and `/metrics` report it
    // without asking the transport again.
    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.groups, 1);
    let Reply::Metrics { metrics } = ask(&core, Request::Metrics).await else {
        panic!("expected a metrics reply");
    };
    assert_eq!(metrics.groups, 1);

    // A token that was never invited with is refused, and refused *as* an
    // invitation problem rather than as an unknown group.
    let error = core
        .request(Request::JoinGroup {
            token: "00ff".to_string(),
        })
        .await
        .expect_err("an unknown invitation token must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{error:?}");
    assert!(
        error.message.contains("no pending group invitation"),
        "{error:?}"
    );

    // Sending to the active group resolves it without an explicit selector.
    //
    // A conference created locally has **no peers yet**, and toxcore reports that
    // as an error instead of queueing the message: conference messages are live
    // only (a peer that joins later never sees them), so an outbox could not
    // honestly promise delivery. Both outcomes are accepted here; what must never
    // happen is a wrong group or a mislabelled reason.
    match core
        .request(Request::SendGroupMessage {
            group_id: None,
            text: "hello group".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        })
        .await
    {
        Ok(Reply::GroupSent {
            group_id,
            group,
            wire_bytes,
        }) => {
            assert_eq!(group_id, groups[0].id);
            assert_eq!(group, "Team");
            assert_eq!(wire_bytes, "hello group".len() as u64);
        }
        Ok(other) => panic!("unexpected reply to a group send: {other:?}"),
        Err(error) => {
            assert_eq!(error.code, ErrorCode::Network, "{error:?}");
            assert!(
                error.message.contains("send to the group"),
                "the failure must name the operation: {error:?}"
            );
            // The reason must be the real one, not a code name borrowed from a
            // different conference function.
            assert!(
                error.message.contains("NO_CONNECTION"),
                "a peerless conference must be reported as not connected: {error:?}"
            );
        }
    }

    // The group can be renamed. The title belongs to the conference, so a later
    // read agrees with the reply even on this instance: toxcore does not raise the
    // `conference_title` callback for our own change, which is why the new name is
    // reported through the reply and cached rather than waited for as an event.
    let Reply::GroupRenamed {
        group_id,
        group: renamed,
    } = ask(
        &core,
        Request::RenameGroup {
            group_id: Some(groups[0].id.clone()),
            name: "Renamed".to_string(),
        },
    )
    .await
    else {
        panic!("expected a group-renamed reply");
    };
    assert_eq!(group_id, groups[0].id);
    assert_eq!(renamed, "Renamed");
    let Reply::Groups {
        groups: after_rename,
        ..
    } = ask(&core, Request::Groups).await
    else {
        panic!("expected a groups reply");
    };
    assert_eq!(after_rename[0].name, "Renamed", "{after_rename:?}");

    // A rename without a name is refused before the transport is asked.
    let error = core
        .request(Request::RenameGroup {
            group_id: None,
            name: "   ".to_string(),
        })
        .await
        .expect_err("an empty name must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest, "{error:?}");

    // An unknown group is refused before anything is sent.
    let error = core
        .request(Request::SendGroupMessage {
            group_id: Some("ab".repeat(32)),
            text: "nowhere".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        })
        .await
        .expect_err("an unknown group must be refused");
    assert_eq!(error.code, ErrorCode::NotFound, "{error:?}");

    // Inviting requires a friend, and there is none.
    let error = core
        .request(Request::InviteToGroup {
            group_id: group.id.clone(),
            peer: "Nobody".to_string(),
        })
        .await
        .expect_err("an unknown peer must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{error:?}");

    // ...and so does declining an invitation that was never received: a token is
    // a capability, so "no such invitation" is the honest answer. The empty token
    // is refused before the transport is asked, like every other request field.
    for token in ["", "   "] {
        let error = core
            .request(Request::DeclineGroupInvite {
                token: token.to_string(),
            })
            .await
            .expect_err("an empty token must be refused");
        assert_eq!(
            error.code,
            ErrorCode::InvalidRequest,
            "{token:?}: {error:?}"
        );
    }
    let error = core
        .request(Request::DeclineGroupInvite {
            token: "ab".repeat(32),
        })
        .await
        .expect_err("an unknown token must be refused");
    assert_eq!(error.code, ErrorCode::Network, "{error:?}");
    assert!(
        error
            .message
            .contains("no pending group invitation matches"),
        "{error:?}"
    );

    // ...and declining changed nothing: the pending list is still empty and no
    // group appeared.
    let Reply::GroupInvites { invites, .. } = ask(&core, Request::GroupInvites).await else {
        panic!("expected a group-invites reply");
    };
    assert!(invites.is_empty(), "{invites:?}");
    let Reply::Groups { groups, .. } = ask(&core, Request::Groups).await else {
        panic!("expected a groups reply");
    };
    assert!(
        groups.len() == 1,
        "only the created group is there: {groups:?}"
    );

    // Leaving removes it, and a second leave has nothing to act on.
    let Reply::GroupLeft { group_id } = ask(
        &core,
        Request::LeaveGroup {
            group_id: Some(group.id.clone()),
        },
    )
    .await
    else {
        panic!("expected a group-left reply");
    };
    assert_eq!(group_id, group.id);
    let Reply::Groups { groups, .. } = ask(&core, Request::Groups).await else {
        panic!("expected a groups reply");
    };
    assert!(groups.is_empty(), "the group must be gone: {groups:?}");
    assert_eq!(
        core.request(Request::LeaveGroup { group_id: None })
            .await
            .expect_err("nothing left to leave")
            .code,
        ErrorCode::NotFound
    );

    // Metrics report the group capability, not just the count.
    let Reply::Metrics { metrics } = ask(&core, Request::Metrics).await else {
        panic!("expected a metrics reply");
    };
    assert_eq!(metrics.groups, 0);
    assert!(metrics.groups_supported);

    core.shutdown().await.expect("shutdown");
}
