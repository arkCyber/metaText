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
    ClientMessage, CoreEvent, ErrorCode, Reply, ServerMessage, PROTOCOL_VERSION,
};
use meta_text::ipc::server::{CoreServer, ServerOptions};
use meta_text::ipc::{CoreClient, CoreService, Request};

/// Token used by the tests.
const TOKEN: &str = "test-token";

/// Start a core service and expose it on an ephemeral loopback port.
async fn running_endpoint() -> (
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
    let options = ServerOptions {
        token: Some(TOKEN.to_string()),
        name: "test".to_string(),
    };
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
#[test]
fn test_protocol_version_is_announced() {
    assert_eq!(PROTOCOL_VERSION, 2);
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
    let client = std::sync::Arc::new(
        RemoteClient::connect(&address.to_string(), Some(TOKEN.to_string()), "load")
            .await
            .expect("handshake"),
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

    for stale in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
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
            }
            other => panic!("expected a rejection for version {stale}, got {other:?}"),
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
        match tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await {
            Ok(0) => true,
            _ => false,
        }
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
    let (_dir, address, server) = running_endpoint().await;

    let mut stream = raw::connect(&address).await;
    // Never send a greeting; the server must close the connection.
    let closed = tokio::time::timeout(Duration::from_secs(15), async {
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
