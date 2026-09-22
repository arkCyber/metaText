/*!
 * ipc_socket_test.rs
 *
 * End-to-end test of the out-of-process core protocol over TCP.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * The test proves the isolation contract: a front-end that only has a socket
 * can drive the backend, cannot stop it, and is refused when its token is
 * wrong or its protocol version is stale.
 */

use std::time::Duration;

use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::client::RemoteClient;
use meta_text::ipc::protocol::{
    is_supported_protocol, ClientMessage, ContentType, CoreEvent, ErrorCode, MessageKind, Reply,
    ServerMessage, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use meta_text::ipc::server::{CoreServer, ServerOptions};
use meta_text::ipc::{CoreClient, CoreService, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Token used by the tests.
const TOKEN: &str = "test-token";

/// Start a core service and expose it on an ephemeral loopback port.
async fn running_endpoint() -> (
    tempfile::TempDir,
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    running_endpoint_with(ServerOptions {
        token: Some(TOKEN.to_string()),
        name: "test".to_string(),
        ..ServerOptions::default()
    })
    .await
}

/// Start a core service with explicit endpoint options.
async fn running_endpoint_with(
    options: ServerOptions,
) -> (
    tempfile::TempDir,
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args).await.expect("service");
    service.start().await.expect("start");
    let core = service.spawn();

    let endpoint = CoreServer::bind("127.0.0.1:0").await.expect("bind");
    let address = endpoint.local_addr().expect("local addr");
    let task = tokio::spawn(async move { endpoint.run(core, options).await });

    (dir, address, task)
}

/// A socket client can drive the backend through the protocol.
#[tokio::test]
async fn test_remote_client_roundtrip() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "test")
        .await
        .expect("handshake");

    let reply = client
        .request(Request::Ping {
            echo: Some("over the wire".to_string()),
        })
        .await
        .expect("ping reply");
    assert_eq!(
        reply,
        Reply::Pong {
            echo: Some("over the wire".to_string())
        }
    );

    // A mutation made by the socket client is visible through a second read.
    client
        .request(Request::AddContact {
            identifier: "REMOTE".to_string(),
            note: None,
        })
        .await
        .expect("add");

    let contact_count = match client.request(Request::ListContacts).await.expect("list") {
        Reply::Contacts { contacts } => contacts.len(),
        other => panic!("unexpected reply: {other:?}"),
    };
    assert_eq!(contact_count, 1);

    client.goodbye().await;
    server.abort();
}

/// The protocol version is part of the contract and is negotiated at handshake.
///
/// The literal is intentional: it must be *bumped* by hand when the vocabulary
/// changes, and a test that simply echoed the constant would not notice a missing
/// bump. The history of the value lives in `ipc::protocol`.
#[test]
fn test_protocol_version_is_announced() {
    assert_eq!(
        PROTOCOL_VERSION, 9,
        "bump the literal when the protocol changes"
    );
    // The window must stay non-empty and never include an unsupported version.
    assert!(is_supported_protocol(PROTOCOL_VERSION));
    assert!(is_supported_protocol(MIN_SUPPORTED_PROTOCOL_VERSION));
    // Two constants, so the compiler can check the window itself; a run-time
    // `assert!` that can never be false would not be testing anything.
    const _: () = assert!(MIN_SUPPORTED_PROTOCOL_VERSION <= PROTOCOL_VERSION);
}

/// The envelope literals a client written in another language has to match.
///
/// The README advertises that `RemoteClient` *and any other language* can drive the
/// backend, but every other test in this file goes through the Rust types: a `serde`
/// rename would keep them all green while breaking every hand written client. The
/// failure tag is the trap — it is `err`, not `error` — and nothing else pins it. This
/// test speaks the protocol with nothing but a `TcpStream` and the four byte length
/// prefix, and reads the frames back as bytes. None of the requests below mutates
/// state, so no event can interleave with a response.
#[tokio::test]
async fn test_the_wire_literals_are_what_a_foreign_client_reads() {
    let (_dir, address, server) = running_endpoint().await;
    let mut socket = TcpStream::connect(address).await.expect("connect");

    // Hello: hand written, the way a client in another language would send it. The
    // version literal itself is pinned by `test_protocol_version_is_announced`.
    send_raw_frame(
        &mut socket,
        &format!(
            r#"{{"type":"hello","protocol_version":{PROTOCOL_VERSION},"token":"{TOKEN}","client":"raw/0.1"}}"#
        ),
    )
    .await;

    let welcome = read_raw_frame(&mut socket).await;
    assert!(welcome.contains(r#""type":"welcome""#), "{welcome}");
    assert!(
        welcome.contains(&format!(r#""protocol_version":{PROTOCOL_VERSION}"#)),
        "{welcome}"
    );

    // A served request: the success envelope.
    send_raw_frame(
        &mut socket,
        r#"{"type":"request","id":1,"request":{"type":"ping","echo":"raw"}}"#,
    )
    .await;
    let reply = read_raw_frame(&mut socket).await;
    assert!(reply.contains(r#""status":"ok""#), "{reply}");
    assert!(reply.contains(r#""type":"pong""#), "{reply}");
    assert!(reply.contains(r#""echo":"raw""#), "{reply}");

    // A refused request: the failure envelope, whose tag is `err` and whose payload
    // sits under `error`.
    send_raw_frame(
        &mut socket,
        r#"{"type":"request","id":2,"request":{"type":"shutdown"}}"#,
    )
    .await;
    let refused = read_raw_frame(&mut socket).await;
    assert!(refused.contains(r#""status":"err""#), "{refused}");
    assert!(refused.contains(r#""code":"unauthorized""#), "{refused}");

    // A refusal is a value, not the end of the session: the same socket serves the
    // next request.
    send_raw_frame(
        &mut socket,
        r#"{"type":"request","id":3,"request":{"type":"ping","echo":"again"}}"#,
    )
    .await;
    let again = read_raw_frame(&mut socket).await;
    assert!(again.contains(r#""echo":"again""#), "{again}");

    send_raw_frame(&mut socket, r#"{"type":"goodbye"}"#).await;
    server.abort();
}

/// Send one frame: a four byte big endian length followed by the JSON payload.
async fn send_raw_frame(socket: &mut TcpStream, payload: &str) {
    let bytes = payload.as_bytes();
    let length = u32::try_from(bytes.len()).expect("a small frame");
    socket
        .write_all(&length.to_be_bytes())
        .await
        .expect("length");
    socket.write_all(bytes).await.expect("payload");
    socket.flush().await.expect("flush");
}

/// Read one frame and return its payload as text.
async fn read_raw_frame(socket: &mut TcpStream) -> String {
    let mut header = [0_u8; 4];
    socket.read_exact(&mut header).await.expect("length");
    let length = u32::from_be_bytes(header) as usize;
    let mut body = vec![0_u8; length];
    socket.read_exact(&mut body).await.expect("payload");
    String::from_utf8(body).expect("the protocol is text")
}

/// A client whose core has gone fails its next request **promptly**, not after its
/// deadline.
///
/// The pump notices the end of the stream, marks the connection closed and drops the
/// replies that were in flight, so a front-end that keeps polling is told the
/// connection is gone instead of waiting its whole deadline per request. A core that
/// exits without a goodbye closes the socket the same way, so this is the path a
/// front-end sees in both cases.
#[tokio::test]
async fn test_a_client_with_a_dead_core_fails_promptly() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "test")
        .await
        .expect("handshake");

    // The core closes the session, the way it does when it shuts down.
    client.goodbye().await;

    // A short deadline makes the difference observable: an attempt nobody answers is
    // reported as a timeout, and once the pump has seen the stream end the same attempt
    // is reported as a network error — the state a front-end polls on.
    let client = client.with_timeout(Duration::from_millis(250));
    let error = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.request(Request::Ping { echo: None }).await {
                Err(error) if error.code == ErrorCode::Network => break error,
                Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("the client must report the closed session rather than wait for its deadline");

    assert_eq!(error.code, ErrorCode::Network);
    assert!(
        error.message.contains("closed"),
        "the message must name the state: {}",
        error.message
    );
    server.abort();
}

/// A wrong token is refused and reported as `unauthorized`.
#[tokio::test]
async fn test_wrong_token_is_refused() {
    let (_dir, address, server) = running_endpoint().await;
    let error = RemoteClient::connect(
        &address.to_string(),
        Some("not-the-token".to_string()),
        "intruder",
    )
    .await
    .expect_err("must be refused");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    server.abort();
}

/// A missing token is refused when the server requires one.
#[tokio::test]
async fn test_missing_token_is_refused() {
    let (_dir, address, server) = running_endpoint().await;
    let error = RemoteClient::connect(&address.to_string(), None, "intruder")
        .await
        .expect_err("must be refused");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    server.abort();
}

/// A socket client cannot stop the backend.
#[tokio::test]
async fn test_remote_shutdown_is_refused() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "test")
        .await
        .expect("handshake");

    let error = client
        .request(Request::Shutdown)
        .await
        .expect_err("shutdown must be refused");
    assert_eq!(error.code, ErrorCode::Unauthorized);

    // The backend is still serving.
    assert!(client.request(Request::Ping { echo: None }).await.is_ok());

    server.abort();
}

/// Requests from one connection are executed in order.
///
/// A front-end that sends `add_contact` and immediately `list_contacts` must
/// observe the addition: replies and effects are FIFO per connection.
#[tokio::test]
async fn test_requests_from_one_client_are_ordered() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "ordered")
        .await
        .expect("handshake");

    let add = client.request(Request::AddContact {
        identifier: "FIRST".to_string(),
        note: None,
    });
    let list = client.request(Request::ListContacts);
    let (add, list) = tokio::join!(add, list);

    assert_eq!(
        add.expect("add"),
        Reply::ContactAdded {
            index: 1,
            identifier: "FIRST".to_string()
        }
    );
    match list.expect("list") {
        Reply::Contacts { contacts } => {
            assert_eq!(
                contacts.len(),
                1,
                "the add must be visible to the following list"
            );
            assert_eq!(contacts[0].name, "FIRST");
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    server.abort();
}
#[tokio::test]
async fn test_events_are_streamed() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "test")
        .await
        .expect("handshake");

    let mut events = client.subscribe().expect("event stream");
    client
        .request(Request::SetNickname {
            nickname: "Broadcaster".to_string(),
        })
        .await
        .expect("set nickname");

    let received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(CoreEvent::NicknameChanged { nickname }) => break Some(nickname),
                Ok(_) => continue,
                Err(_) => break None,
            }
        }
    })
    .await
    .expect("event arrived in time");

    assert_eq!(received.as_deref(), Some("Broadcaster"));

    server.abort();
}

/// Many requests in flight at once are all answered, matched by id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_many_concurrent_requests_are_all_answered() {
    let (_dir, address, server) = running_endpoint().await;
    // The point of this test is id matching under concurrency, not the deadline.
    // A generous one keeps the assertion about replies rather than about
    // scheduler luck on a loaded machine, while still failing fast if a reply is
    // genuinely lost.
    let client = std::sync::Arc::new(
        RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "load")
            .await
            .expect("handshake")
            .with_timeout(Duration::from_secs(60)),
    );

    let mut tasks = Vec::new();
    for index in 0..64_u32 {
        let client = std::sync::Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            client
                .request(Request::Ping {
                    echo: Some(index.to_string()),
                })
                .await
                .expect("reply")
        }));
    }

    let mut echoes: Vec<u32> = Vec::new();
    for task in tasks {
        match task.await.expect("join") {
            Reply::Pong { echo } => echoes.push(echo.expect("echo").parse().expect("numeric echo")),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    echoes.sort_unstable();
    let expected: Vec<u32> = (0..64_u32).collect();
    assert_eq!(
        echoes, expected,
        "every request must be answered exactly once"
    );

    server.abort();
}

/// Two attached front-ends share one backend without interfering.
#[tokio::test]
async fn test_multiple_clients_share_one_backend() {
    let (_dir, address, server) = running_endpoint().await;
    let alice = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "alice")
        .await
        .expect("alice");
    let bob = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "bob")
        .await
        .expect("bob");

    // Each client writes, and each request is correlated independently of the
    // other connection.
    alice
        .request(Request::AddContact {
            identifier: "AAA".to_string(),
            note: None,
        })
        .await
        .expect("alice add");
    bob.request(Request::AddContact {
        identifier: "BBB".to_string(),
        note: None,
    })
    .await
    .expect("bob add");

    // Both observe the same shared state.
    for client in [&alice, &bob] {
        match client.request(Request::ListContacts).await.expect("list") {
            Reply::Contacts { contacts } => {
                let names: Vec<&str> = contacts.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["AAA", "BBB"]);
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    // A nickname set by one client is visible to the other.
    alice
        .request(Request::SetNickname {
            nickname: "Alice".to_string(),
        })
        .await
        .expect("set nickname");
    match bob.request(Request::SessionInfo).await.expect("session") {
        Reply::Session { session } => assert_eq!(session.nickname, "Alice"),
        other => panic!("unexpected reply: {other:?}"),
    }

    server.abort();
}

/// Boundary validation is enforced on the server side, not only in the CLI.
#[tokio::test]
async fn test_boundary_validation_over_the_wire() {
    use meta_text::ipc::protocol::{MAX_IDENTIFIER_LEN, MAX_NICKNAME_LEN};

    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "validation")
        .await
        .expect("handshake");

    // Requests that must be rejected, each with the typed code a front-end
    // switches on.
    let rejected = vec![
        Request::SetNickname {
            nickname: "a".repeat(MAX_NICKNAME_LEN + 1),
        },
        Request::SetNickname {
            nickname: "bad\u{0}name".to_string(),
        },
        Request::AddContact {
            identifier: "with space".to_string(),
            note: None,
        },
        Request::AddContact {
            identifier: "x".repeat(MAX_IDENTIFIER_LEN + 1),
            note: None,
        },
        Request::AddContact {
            identifier: "DID".to_string(),
            note: Some("note\u{0}".to_string()),
        },
        Request::SendMessage {
            target: Some("Alice".to_string()),
            text: "escape\u{1b}[2J".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    ];

    for request in &rejected {
        let error = client
            .request(request.clone())
            .await
            .expect_err("was accepted but must be rejected");
        assert_eq!(
            error.code,
            ErrorCode::InvalidRequest,
            "unexpected code for {request:?}: {error:?}"
        );
    }

    // A rejected request must leave no trace: nothing was added, and the
    // running counter did not move.
    match client.request(Request::ListContacts).await.expect("list") {
        Reply::Contacts { contacts } => assert!(contacts.is_empty()),
        other => panic!("unexpected reply: {other:?}"),
    }
    match client.request(Request::Statistics).await.expect("stats") {
        Reply::Statistics { statistics } => assert_eq!(statistics.messages_sent, 0),
        other => panic!("unexpected reply: {other:?}"),
    }

    server.abort();
}

/// A raw client that speaks the framing directly, used to test the paths the
/// typed client cannot reach (stale versions, oversized frames, silence).
mod raw {
    use meta_text::ipc::framing::{read_message, write_message};
    use meta_text::ipc::protocol::{ClientMessage, ServerMessage};
    use tokio::net::TcpStream;

    /// Open a TCP connection to the test endpoint.
    pub async fn connect(address: &std::net::SocketAddr) -> TcpStream {
        TcpStream::connect(address).await.expect("connect")
    }

    /// Send one client message.
    pub async fn send(stream: &mut TcpStream, message: &ClientMessage) {
        write_message(stream, message).await.expect("write");
    }

    /// Read one server message, or `None` if the server closed the connection.
    pub async fn recv(stream: &mut TcpStream) -> Option<ServerMessage> {
        read_message(stream).await.expect("read")
    }
}

/// A client announcing a stale protocol version is rejected, not mis-served.
#[tokio::test]
async fn test_stale_protocol_version_is_refused() {
    let (_dir, address, server) = running_endpoint().await;

    // Anything below the floor and anything above the current version.
    for stale in [
        MIN_SUPPORTED_PROTOCOL_VERSION - 1,
        PROTOCOL_VERSION + 1,
        0,
        u32::MAX,
    ] {
        let mut stream = raw::connect(&address).await;
        raw::send(
            &mut stream,
            &ClientMessage::Hello {
                protocol_version: stale,
                token: Some(TOKEN.to_string()),
                client: "stale".to_string(),
            },
        )
        .await;

        match raw::recv(&mut stream).await {
            Some(ServerMessage::Rejected { error }) => {
                assert_eq!(error.code, ErrorCode::UnsupportedProtocol, "{error:?}");
                assert!(
                    error.message.contains("is not supported"),
                    "the rejection must explain the window: {}",
                    error.message
                );
            }
            other => panic!("expected a rejection for version {stale}, got {other:?}"),
        }
    }

    server.abort();
}

/// Every version inside the compatibility window is served, so a front-end can
/// be upgraded independently of the core.
#[tokio::test]
async fn test_the_compatibility_window_is_served() {
    let (_dir, address, server) = running_endpoint().await;

    for version in MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION {
        let mut stream = raw::connect(&address).await;
        raw::send(
            &mut stream,
            &ClientMessage::Hello {
                protocol_version: version,
                token: Some(TOKEN.to_string()),
                client: format!("v{version}"),
            },
        )
        .await;

        match raw::recv(&mut stream).await {
            Some(ServerMessage::Welcome {
                protocol_version,
                session,
            }) => {
                assert_eq!(
                    protocol_version, PROTOCOL_VERSION,
                    "the server must answer with its own version"
                );
                assert!(!session.nickname.is_empty());
            }
            other => panic!("version {version} must be served, got {other:?}"),
        }

        // The session stays usable after the handshake.
        raw::send(
            &mut stream,
            &ClientMessage::Request {
                id: 1,
                request: Request::Ping {
                    echo: Some(format!("v{version}")),
                },
            },
        )
        .await;
        match raw::recv(&mut stream).await {
            Some(ServerMessage::Response { id, result }) => {
                assert_eq!(id, 1);
                assert!(result.is_ok(), "ping must answer on version {version}");
            }
            other => panic!("expected a response on version {version}, got {other:?}"),
        }
    }

    server.abort();
}

/// An oversized frame is refused and the connection is dropped, while the
/// endpoint keeps serving other clients.
#[tokio::test]
async fn test_oversized_frame_does_not_break_the_endpoint() {
    use meta_text::ipc::protocol::MAX_FRAME_LEN;
    use tokio::io::AsyncWriteExt;

    let (_dir, address, server) = running_endpoint().await;

    let mut stream = raw::connect(&address).await;
    stream
        .write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
        .await
        .expect("write oversized header");
    // The server must close the connection rather than reading the body.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = [0_u8; 1];
        matches!(
            tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await,
            Ok(0)
        )
    })
    .await
    .expect("server must react promptly");
    assert!(closed, "the server must close the offending connection");

    // The endpoint is still healthy for well behaved clients.
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "after")
        .await
        .expect("handshake still works");
    assert!(client.request(Request::Ping { echo: None }).await.is_ok());

    server.abort();
}

/// A client that connects and stays silent is dropped by the handshake
/// timeout, so it cannot occupy a connection slot indefinitely.
#[tokio::test]
async fn test_silent_client_is_dropped_by_the_handshake_timeout() {
    // Shorten the server side of the timeout so the assertion is driven by the
    // endpoint rather than by the test's own wall-clock budget: a loaded
    // machine can delay a 5 second timer by seconds, which made this test flaky.
    let (_dir, address, server) = running_endpoint_with(ServerOptions {
        token: Some(TOKEN.to_string()),
        name: "test".to_string(),
        handshake_timeout: Duration::from_millis(250),
        ..ServerOptions::default()
    })
    .await;

    let mut stream = raw::connect(&address).await;
    // Never send a greeting; the server must close the connection.
    let closed = tokio::time::timeout(Duration::from_secs(30), async {
        let mut buffer = [0_u8; 1];
        matches!(
            tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await,
            Ok(0)
        )
    })
    .await
    .expect("the server must enforce the handshake timeout");
    assert!(closed);

    // And the endpoint still serves real clients.
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "after")
        .await
        .expect("handshake still works");
    assert!(client.request(Request::Ping { echo: None }).await.is_ok());

    server.abort();
}

/// A monitoring client can read the operational snapshot over the socket.
#[tokio::test]
async fn test_metrics_are_available_over_the_wire() {
    let (_dir, address, server) = running_endpoint().await;
    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "monitor")
        .await
        .expect("handshake");

    // Generate a little traffic so the counters are not all zero.
    client
        .request(Request::AddContact {
            identifier: "Alice".to_string(),
            note: None,
        })
        .await
        .expect("add contact");
    client
        .request(Request::SendMessage {
            target: Some("Alice".to_string()),
            text: "counted".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        })
        .await
        .expect("send");

    let Reply::Metrics { metrics } = client.request(Request::Metrics).await.expect("metrics")
    else {
        panic!("expected a metrics reply");
    };

    assert_eq!(metrics.transport, "tcp");
    assert_eq!(metrics.messages_sent, 1, "{metrics:?}");
    assert_eq!(metrics.friend_count, 1, "{metrics:?}");
    assert_eq!(
        metrics.payloads_queued, 1,
        "the peer is offline: {metrics:?}"
    );
    assert_eq!(metrics.peers_connected, 0, "{metrics:?}");
    assert!(
        metrics.request_queue_depth <= metrics.request_queue_capacity,
        "{metrics:?}"
    );
    // The bounded request/invitation lists are only meaningful on Tox, and a
    // transport without them reports zero rather than omitting the counters.
    assert_eq!(metrics.friend_requests_dropped, 0, "{metrics:?}");
    assert_eq!(metrics.group_invites_dropped, 0, "{metrics:?}");
    // The monitor itself is a subscriber.
    assert!(metrics.event_subscribers >= 1, "{metrics:?}");

    client.goodbye().await;
    server.abort();
}

/// A client that floods the endpoint is shed with `backpressure` instead of
/// being allowed to keep the actor busy, and other clients are unaffected.
#[tokio::test]
async fn test_request_rate_limit_sheds_a_flood() {
    let (_dir, address, server) = running_endpoint_with(ServerOptions {
        token: Some(TOKEN.to_string()),
        name: "test".to_string(),
        // One request, then a one second refill: a burst of pings cannot pass.
        requests_per_second: 1,
        request_burst: 1,
        ..ServerOptions::default()
    })
    .await;

    let client = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "flood")
        .await
        .expect("handshake");

    let mut shed = 0;
    for _ in 0..5 {
        if let Err(error) = client.request(Request::Ping { echo: None }).await {
            assert_eq!(error.code, ErrorCode::Backpressure, "{}", error.message);
            shed += 1;
        }
    }
    assert!(
        shed > 0,
        "the limiter must shed at least one request from a burst"
    );

    // The endpoint still accepts a second client...
    let other = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "other")
        .await
        .expect("handshake still works");
    // ...and it is shed too, because the budget follows the client *address*: a
    // second connection from the same host is the same client as far as the limit is
    // concerned. (A different address has its own budget, which
    // `ipc::server::tests::test_a_budget_follows_the_address_not_the_connection`
    // pins without needing two hosts.)
    let error = other
        .request(Request::Ping { echo: None })
        .await
        .expect_err("the same address shares the spent budget");
    assert_eq!(error.code, ErrorCode::Backpressure, "{error:?}");

    server.abort();
}

/// Reconnecting does not hand out a fresh allowance.
///
/// The limiter used to live on the connection, so a client that was shed could open
/// a new socket and start over. The budget is keyed by the peer's address now, and
/// this is the test that fails if that ever regresses.
#[tokio::test]
async fn test_a_reconnect_does_not_refill_the_budget() {
    let (_dir, address, server) = running_endpoint_with(ServerOptions {
        token: Some(TOKEN.to_string()),
        name: "test".to_string(),
        // One request per second: the first client spends the only token.
        requests_per_second: 1,
        request_burst: 1,
        ..ServerOptions::default()
    })
    .await;

    let first = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "first")
        .await
        .expect("handshake");
    assert!(
        first.request(Request::Ping { echo: None }).await.is_ok(),
        "the first client spends its one token"
    );
    first.goodbye().await;
    drop(first);

    // A brand new connection from the same address inherits the spent budget.
    let second = RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "second")
        .await
        .expect("handshake");
    let error = second
        .request(Request::Ping { echo: None })
        .await
        .expect_err("a reconnect must not refill the allowance");
    assert_eq!(error.code, ErrorCode::Backpressure, "{error:?}");

    // ...until the allowance itself refills, one request per second.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(
        second.request(Request::Ping { echo: None }).await.is_ok(),
        "time, unlike a reconnect, does refill the budget"
    );

    server.abort();
}
