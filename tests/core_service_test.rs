/*!
 * core_service_test.rs
 *
 * Integration tests for the headless core service and its client handle.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * These tests only use the public interface of the `meta_text` crate and never
 * touch `crypto`, `database` or `network` directly: that is exactly the
 * contract the user interfaces are held to.
 */

use std::time::Duration;

use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::protocol::{
    ContentType, ErrorCode, MessageKind, Reply, ResponseResult, SendOutcomeKind,
};
use meta_text::ipc::{
    CoreClient, CoreHandle, CoreService, CoreServiceOptions, LocalClient, Request,
};

/// Build and start a core service in an isolated workspace.
async fn started_service() -> (CoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = start_in(&dir).await;
    (core, dir)
}

/// Build and start a core service over an existing data directory.
///
/// Split out of [`started_service`] so a test can restart a service over the same
/// directory — which is what makes the identity a *restart* test rather than a
/// construction one.
async fn start_in(dir: &tempfile::TempDir) -> CoreHandle {
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.app.auto_save_interval = 1;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args).await.expect("core service");
    service.start().await.expect("start");
    service.spawn()
}

/// Ask the core for a reply and unwrap it.
async fn ask(core: &CoreHandle, request: Request) -> Reply {
    core.request(request).await.expect("reply")
}

/// Ask the core for a failure and unwrap it.
async fn ask_err(core: &CoreHandle, request: Request) -> meta_text::ipc::ErrorInfo {
    core.request(request).await.expect_err("expected a failure")
}

/// A ping is echoed back unchanged.
#[tokio::test]
async fn test_ping_roundtrip() {
    let (core, _dir) = started_service().await;
    let reply = ask(
        &core,
        Request::Ping {
            echo: Some("pong".to_string()),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Pong {
            echo: Some("pong".to_string())
        }
    );
    core.shutdown().await.expect("shutdown");
}

/// The session report describes a freshly started service.
#[tokio::test]
async fn test_session_info_is_populated() {
    let (core, _dir) = started_service().await;
    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };

    assert!(session.network_running);
    assert!(!session.identity.is_empty());
    assert_eq!(session.friend_count, 0);
    assert!(session.encryption_enabled);
    assert_eq!(session.version, env!("CARGO_PKG_VERSION"));
    core.shutdown().await.expect("shutdown");
}

/// The announced identity is a persisted key, so a peer that pinned it sees the
/// same value after a restart.
///
/// This is the difference the identity work makes: a per-session random DID cannot
/// be pinned by anybody, so a change could never be seen and the value could never
/// be compared.
#[tokio::test]
async fn test_the_identity_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    let first = start_in(&dir).await;
    let Reply::Session { session } = ask(&first, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    first.shutdown().await.expect("shutdown");

    let second = start_in(&dir).await;
    let Reply::Session { session: after } = ask(&second, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    second.shutdown().await.expect("shutdown");

    assert_eq!(
        session.identity, after.identity,
        "the same data directory must announce the same identity"
    );
    assert_eq!(
        session.identity_fingerprint, after.identity_fingerprint,
        "the fingerprint a user compares must be stable too"
    );
    assert_eq!(session.identity.len(), 64, "the shape a peer expects");
    assert_eq!(
        session.identity_fingerprint.matches('-').count(),
        7,
        "eight groups of four hexadecimal characters"
    );
    assert!(
        !session.identity_fingerprint.is_empty()
            && !session.identity.contains(&session.identity_fingerprint),
        "the fingerprint is a short form of the identity, not the value itself"
    );
}

/// Friends can be added once; a duplicate is reported, not an error.
#[tokio::test]
async fn test_contact_lifecycle() {
    let (core, _dir) = started_service().await;

    let reply = ask(
        &core,
        Request::AddContact {
            identifier: "DID123".to_string(),
            note: Some("my friend".to_string()),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::ContactAdded {
            index: 1,
            identifier: "DID123".to_string()
        }
    );

    // Duplicates are detected case-insensitively.
    let reply = ask(
        &core,
        Request::AddContact {
            identifier: "did123".to_string(),
            note: None,
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::ContactExists {
            identifier: "did123".to_string()
        }
    );

    let Reply::Contacts { contacts } = ask(&core, Request::ListContacts).await else {
        panic!("expected contacts");
    };
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].note.as_deref(), Some("my friend"));

    let reply = ask(
        &core,
        Request::RemoveContact {
            target: "1".to_string(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::ContactRemoved {
            index: 1,
            name: "DID123".to_string()
        }
    );

    // A missing friend is a typed, non-fatal failure.
    let error = ask_err(
        &core,
        Request::RemoveContact {
            target: "nobody".to_string(),
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(!error.code.is_critical());
    core.shutdown().await.expect("shutdown");
}

/// Nickname and status can be set and queried.
#[tokio::test]
async fn test_nickname_and_status() {
    let (core, _dir) = started_service().await;

    let reply = ask(
        &core,
        Request::SetNickname {
            nickname: "Alice".to_string(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Updated {
            subject: "nickname".to_string(),
            detail: "Alice".to_string()
        }
    );

    // An empty request only reports the current value.
    let reply = ask(
        &core,
        Request::SetNickname {
            nickname: String::new(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Updated {
            subject: "nickname".to_string(),
            detail: "Alice".to_string()
        }
    );

    let reply = ask(
        &core,
        Request::SetStatus {
            text: "busy".to_string(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Updated {
            subject: "status".to_string(),
            detail: "busy".to_string()
        }
    );
    core.shutdown().await.expect("shutdown");
}

/// An offline destination is buffered rather than lost.
#[tokio::test]
async fn test_message_to_offline_peer_is_queued() {
    let (core, _dir) = started_service().await;

    let reply = ask(
        &core,
        Request::SendMessage {
            target: Some("Alice".to_string()),
            text: "hello there".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await;

    let Reply::Sent { target, report } = reply else {
        panic!("expected a send report");
    };
    assert_eq!(target, "Alice");
    assert_eq!(report.outcome, SendOutcomeKind::Queued);
    assert_eq!(report.queued_for, 0);
    assert_eq!(report.queue_position, Some(1));
    assert!(report.message_id.is_some());
    assert!(report.encrypted);

    let Reply::Statistics { statistics } = ask(&core, Request::Statistics).await else {
        panic!("expected statistics");
    };
    assert_eq!(statistics.messages_sent, 1);
    assert!(statistics.bytes_sent > 0);
    core.shutdown().await.expect("shutdown");
}

/// The configured limit is enforced before any crypto work happens.
#[tokio::test]
async fn test_message_length_limit_is_enforced() {
    let (core, _dir) = started_service().await;
    let long = "x".repeat(5000);

    let error = ask_err(
        &core,
        Request::SendMessage {
            target: Some("Alice".to_string()),
            text: long,
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    let Reply::Statistics { statistics } = ask(&core, Request::Statistics).await else {
        panic!("expected statistics");
    };
    assert_eq!(statistics.messages_sent, 0, "nothing may be counted");
    core.shutdown().await.expect("shutdown");
}

/// A message without a target and without an active chat is rejected.
#[tokio::test]
async fn test_message_without_conversation_is_rejected() {
    let (core, _dir) = started_service().await;
    let error = ask_err(
        &core,
        Request::SendMessage {
            target: None,
            text: "hello".to_string(),
            kind: MessageKind::Text,
            content_type: ContentType::Text,
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::NotFound);
    core.shutdown().await.expect("shutdown");
}

/// Selecting a conversation validates the target and stores the choice.
#[tokio::test]
async fn test_conversation_selection() {
    let (core, _dir) = started_service().await;
    ask(
        &core,
        Request::AddContact {
            identifier: "DIDABC".to_string(),
            note: None,
        },
    )
    .await;

    let reply = ask(
        &core,
        Request::SelectConversation {
            target: "didabc".to_string(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Conversation {
            active: Some("DIDABC".to_string()),
            index: Some(1)
        }
    );

    // Friend numbering starts at one.
    let error = ask_err(
        &core,
        Request::SelectConversation {
            target: "0".to_string(),
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    // An unknown name is reported as not found.
    let error = ask_err(
        &core,
        Request::SelectConversation {
            target: "nobody".to_string(),
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::NotFound);

    // Querying without a target reports the active chat.
    let reply = ask(
        &core,
        Request::SelectConversation {
            target: String::new(),
        },
    )
    .await;
    assert_eq!(
        reply,
        Reply::Conversation {
            active: Some("DIDABC".to_string()),
            index: Some(1)
        }
    );
    core.shutdown().await.expect("shutdown");
}

/// The session snapshot survives a save/restart cycle.
#[tokio::test]
async fn test_session_persistence_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        ..CliArgs::default()
    };

    {
        let mut service = CoreService::new(config.clone(), args.clone())
            .await
            .expect("service");
        service.start().await.expect("start");
        let core = service.spawn();

        ask(
            &core,
            Request::SetNickname {
                nickname: "Stored".to_string(),
            },
        )
        .await;
        ask(
            &core,
            Request::AddContact {
                identifier: "DIDKEEP".to_string(),
                note: None,
            },
        )
        .await;
        assert!(matches!(
            ask(&core, Request::SaveSession).await,
            Reply::Saved { .. }
        ));
        core.shutdown().await.expect("shutdown");
    }

    // A second service restores nickname and contacts from disk.
    let mut service = CoreService::new(config, args).await.expect("service");
    service.start().await.expect("start");
    let core = service.spawn();

    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.nickname, "Stored");
    assert_eq!(session.friend_count, 1);

    let Reply::Contacts { contacts } = ask(&core, Request::ListContacts).await else {
        panic!("expected contacts");
    };
    assert_eq!(contacts[0].name, "DIDKEEP");
    core.shutdown().await.expect("shutdown");
}

/// A graceful shutdown persists the session even without an explicit `/save`.
///
/// The shutdown reply is only sent once terminal cleanup has finished, so a
/// front-end (and `main`) that returns on the reply can no longer drop the
/// runtime in the middle of the snapshot write.
#[tokio::test]
async fn test_shutdown_persists_session_without_explicit_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        ..CliArgs::default()
    };

    let session_file = dir.path().join("metatext-session.json");

    let mut service = CoreService::new(config, args).await.expect("service");
    service.start().await.expect("start");
    let core = service.spawn();

    ask(
        &core,
        Request::SetNickname {
            nickname: "Graceful".to_string(),
        },
    )
    .await;

    // Deliberately no `Request::SaveSession`: the shutdown path alone must
    // leave a readable snapshot behind.
    core.shutdown().await.expect("shutdown");

    assert!(
        session_file.exists(),
        "a graceful shutdown must persist the session snapshot"
    );
    let raw = std::fs::read_to_string(&session_file).expect("read session file");
    assert!(
        raw.contains("Graceful"),
        "the snapshot must contain the nickname that was set"
    );
}

/// The session snapshot is replaced atomically and leaves no staging file.
#[tokio::test]
async fn test_session_save_is_atomic_and_leaves_no_temp_file() {
    let (core, dir) = started_service().await;

    ask(
        &core,
        Request::SetNickname {
            nickname: "Atomic".to_string(),
        },
    )
    .await;
    assert!(matches!(
        ask(&core, Request::SaveSession).await,
        Reply::Saved { .. }
    ));

    let session = dir.path().join("metatext-session.json");
    let temporary = dir.path().join("metatext-session.tmp");
    assert!(session.exists(), "the snapshot must be written");
    assert!(
        !temporary.exists(),
        "the staging file must be renamed away, not left behind"
    );

    // The shutdown save follows the same path.
    core.shutdown().await.expect("shutdown");
    assert!(session.exists());
    assert!(!temporary.exists());
}

/// `--nick` wins over the persisted session.
#[tokio::test]
async fn test_command_line_nickname_wins_over_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AppConfig::default();
    config.network.port = 0;
    config.database.connection_string = dir.path().join("core.db").to_string_lossy().into_owned();

    let base = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        port: Some(0),
        ..CliArgs::default()
    };

    {
        let mut service = CoreService::new(config.clone(), base.clone())
            .await
            .expect("service");
        service.start().await.expect("start");
        let core = service.spawn();
        ask(
            &core,
            Request::SetNickname {
                nickname: "Stored".to_string(),
            },
        )
        .await;
        let _ = ask(&core, Request::SaveSession).await;
        core.shutdown().await.expect("shutdown");
    }

    let args = CliArgs {
        nickname: Some("FromCli".to_string()),
        ..base
    };
    let mut service = CoreService::new(config, args).await.expect("service");
    service.start().await.expect("start");
    let core = service.spawn();

    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.nickname, "FromCli");
    core.shutdown().await.expect("shutdown");
}

/// Shutdown is acknowledged and the actor stops accepting work.
#[tokio::test]
async fn test_shutdown_stops_the_actor() {
    let (core, _dir) = started_service().await;
    let reply = core.shutdown().await.expect("acknowledged");
    assert_eq!(reply, Reply::ShuttingDown { acknowledged: true });

    // The actor task ends asynchronously; wait for the channel to close.
    for _ in 0..50 {
        if !core.is_alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the core service did not stop");
}

/// The command queue is bounded: overload is reported, not buffered forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_command_queue_applies_backpressure() {
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

    // A single-slot queue plus many concurrent callers must never deadlock or
    // grow without bound: every caller is served in turn.
    let core = service
        .with_options(CoreServiceOptions {
            command_capacity: 1,
            event_capacity: 4,
        })
        .spawn();

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let core = core.clone();
        tasks.push(tokio::spawn(async move {
            core.request(Request::Ping { echo: None }).await
        }));
    }

    let mut answered = 0;
    for task in tasks {
        if task.await.expect("join").is_ok() {
            answered += 1;
        }
    }
    assert_eq!(answered, 32, "every ping must eventually be answered");
    core.shutdown().await.expect("shutdown");
}

/// A rejected request never poisons the service.
#[tokio::test]
async fn test_failures_do_not_poison_the_service() {
    let (core, _dir) = started_service().await;
    let error = ask_err(
        &core,
        Request::RemoveContact {
            target: "ghost".to_string(),
        },
    )
    .await;
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_ne!(error.severity, meta_text::ipc::protocol::Severity::Critical);

    assert!(matches!(
        ask(&core, Request::Ping { echo: None }).await,
        Reply::Pong { .. }
    ));
    core.shutdown().await.expect("shutdown");
}

/// The in-process client and the raw handle agree on every request.
#[tokio::test]
async fn test_local_client_matches_the_handle() {
    let (core, _dir) = started_service().await;
    let client = LocalClient::new(core.clone());

    let via_client = client
        .request(Request::Ping {
            echo: Some("same".to_string()),
        })
        .await
        .expect("client reply");
    let via_handle = ask(
        &core,
        Request::Ping {
            echo: Some("same".to_string()),
        },
    )
    .await;
    assert_eq!(via_client, via_handle);

    // Event support is advertised by the in-process transport.
    assert!(client.subscribe().is_some());
    core.shutdown().await.expect("shutdown");
}

/// `ResponseResult` conversion helpers used by the transports stay correct.
#[test]
fn test_response_result_helpers() {
    let ok = ResponseResult::ok(Reply::Pong { echo: None });
    assert!(ok.is_ok());
    assert_eq!(ok.into_result().expect("ok"), Reply::Pong { echo: None });

    let failed = ResponseResult::err(meta_text::ipc::ErrorInfo::new(ErrorCode::NotFound, "gone"));
    assert!(!failed.is_ok());
    assert_eq!(
        failed.into_result().expect_err("err").code,
        ErrorCode::NotFound
    );
}

/// `/metrics` reports actor lag: how long a request waited in the queue and how
/// long the actor spent on it.
///
/// The queue wait is the signal a monitor needs — it is the only counter that
/// grows when the actor is busy or a front-end floods it — so it is asserted
/// with real queued work rather than with a single request, whose wait would be
/// unmeasurably close to zero.
#[tokio::test]
async fn test_metrics_report_actor_lag() {
    let (core, _dir) = started_service().await;

    // A single request on an idle actor is served and counted.
    ask(&core, Request::Ping { echo: None }).await;
    let Reply::Metrics { metrics } = ask(&core, Request::Metrics).await else {
        panic!("expected a metrics reply");
    };
    // The snapshot describes finished work, so the reporting request itself is not
    // part of its own count: only the idle ping has completed.
    assert_eq!(metrics.requests_served, 1, "{metrics:?}");
    assert_eq!(metrics.request_queue_capacity, 64, "{metrics:?}");

    // A burst that has to queue: with several requests in flight at once, at
    // least one of them cannot be served immediately.
    const BURST: usize = 16;
    let mut pending = Vec::with_capacity(BURST);
    for index in 0..BURST {
        let core = core.clone();
        pending.push(tokio::spawn(async move {
            core.request(Request::Ping {
                echo: Some(index.to_string()),
            })
            .await
        }));
    }
    for task in pending {
        task.await.expect("task").expect("ping");
    }

    // A request that must reach the database has a service time measurable on any
    // machine. Timing a `Ping` instead would make the assertion depend on the
    // clock's resolution: an idle actor serves one in under a microsecond.
    ask(&core, Request::History { limit: Some(1) }).await;

    let Reply::Metrics { metrics } = ask(&core, Request::Metrics).await else {
        panic!("expected a metrics reply");
    };
    assert_eq!(
        metrics.requests_served,
        2 + BURST as u64 + 1,
        "every completed request is counted once, and the reporting request is \
         not part of its own snapshot: {metrics:?}"
    );
    assert!(
        metrics.request_wait_max_us > 0,
        "a queued request must show a non-zero wait: {metrics:?}"
    );
    assert!(
        metrics.request_service_max_us > 0,
        "the actor must time its own work: {metrics:?}"
    );
    assert!(
        metrics.request_wait_max_us >= metrics.request_wait_last_us,
        "the last wait cannot exceed the maximum: {metrics:?}"
    );
    assert!(
        metrics.request_service_max_us >= metrics.request_service_last_us,
        "the last service time cannot exceed the maximum: {metrics:?}"
    );
    // The actor is idle again once the burst has drained.
    assert_eq!(metrics.request_queue_depth, 0, "{metrics:?}");

    core.shutdown().await.expect("shutdown");
}

/// The TCP transport has no groups, and says so instead of pretending.
///
/// "There are no groups" and "this transport cannot have groups" are different
/// facts, so the capability is reported (and every group mutation is refused)
/// rather than answered with an empty list that looks like an empty session.
#[tokio::test]
async fn test_groups_are_reported_as_unsupported_on_the_tcp_transport() {
    let (core, _dir) = started_service().await;

    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.transport, "tcp");
    assert!(!session.supports_groups);
    assert_eq!(session.groups, 0);
    assert_eq!(session.pending_group_invites, 0);

    // Asking for the list succeeds and carries the capability flag, so a REPL can
    // print one clear sentence instead of an error.
    let Reply::Groups { supported, groups } = ask(&core, Request::Groups).await else {
        panic!("expected a groups reply");
    };
    assert!(!supported);
    assert!(groups.is_empty(), "{groups:?}");

    // The pending invitations answer the same way: an empty list carrying the
    // capability flag, never an error, so a late-attached front-end can render one
    // sentence instead of a failure. (The tokens themselves only exist on Tox.)
    let Reply::GroupInvites { supported, invites } = ask(&core, Request::GroupInvites).await else {
        panic!("expected a group invites reply");
    };
    assert!(!supported);
    assert!(invites.is_empty(), "{invites:?}");

    // Every group mutation fails with a typed error that names the transport.
    for (request, operation) in [
        (
            Request::CreateGroup {
                title: Some("Team".to_string()),
            },
            "create a group",
        ),
        (
            Request::JoinGroup {
                token: "00ff".to_string(),
            },
            "join a group",
        ),
        (
            Request::RenameGroup {
                group_id: Some("ab".repeat(32)),
                name: "Renamed".to_string(),
            },
            "rename a group",
        ),
        (
            Request::InviteToGroup {
                group_id: "ab".repeat(32),
                peer: "Nobody".to_string(),
            },
            "invite to a group",
        ),
        (
            Request::SendGroupMessage {
                group_id: None,
                text: "hello".to_string(),
                kind: MessageKind::Text,
                content_type: ContentType::Text,
            },
            "send to a group",
        ),
        (Request::LeaveGroup { group_id: None }, "leave a group"),
        (
            Request::DeclineGroupInvite {
                token: "00ff".to_string(),
            },
            "discard a group invitation",
        ),
    ] {
        let error = core
            .request(request)
            .await
            .expect_err("a group request must be refused on tcp");
        assert_eq!(
            error.code,
            ErrorCode::InvalidRequest,
            "{operation}: {error:?}"
        );
        assert!(
            error.message.contains("no groups") && error.message.contains("--transport tox"),
            "the refusal must say why and what to do: {error:?}"
        );
    }

    // Validation still runs first where it can: a group request the core can
    // reject on its own does so without touching the transport at all.
    let error = core
        .request(Request::JoinGroup {
            token: "  ".to_string(),
        })
        .await
        .expect_err("an empty token must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("token"), "{error:?}");

    // None of the refusals changed any state or counted any traffic.
    let Reply::Statistics { statistics } = ask(&core, Request::Statistics).await else {
        panic!("expected a statistics reply");
    };
    assert_eq!(statistics.messages_sent, 0, "{statistics:?}");
    assert_eq!(statistics.bytes_sent, 0, "{statistics:?}");
    let Reply::Metrics { metrics } = ask(&core, Request::Metrics).await else {
        panic!("expected a metrics reply");
    };
    assert_eq!(metrics.groups, 0);
    assert!(!metrics.groups_supported);

    core.shutdown().await.expect("shutdown");
}

/// The friend-request vocabulary is Tox-only, and TCP says so rather than failing
/// in a way that looks like a missing request.
///
/// Both answering verbs are covered: accepting and rejecting are the same kind of
/// lookup, and a front-end that offers one must not have the other silently
/// succeed on a transport that has no requests at all.
#[tokio::test]
async fn test_friend_request_answers_are_refused_on_the_tcp_transport() {
    let (core, _dir) = started_service().await;

    let Reply::Session { session } = ask(&core, Request::SessionInfo).await else {
        panic!("expected a session reply");
    };
    assert_eq!(session.transport, "tcp");
    assert!(!session.supports_friend_requests);
    assert_eq!(session.pending_requests, 0);

    // Listing is the one that succeeds everywhere: "there are none" is an answer.
    let Reply::PeerRequests { requests } = ask(&core, Request::PeerRequests).await else {
        panic!("expected a peer-requests reply");
    };
    assert!(requests.is_empty(), "{requests:?}");

    let key = "A".repeat(meta_text::cli::PUBLIC_KEY_HEX_LEN);
    for (request, operation) in [
        (
            Request::AcceptPeerRequest {
                public_key: key.clone(),
            },
            "accept",
        ),
        (
            Request::RejectPeerRequest {
                public_key: key.clone(),
            },
            "reject",
        ),
    ] {
        let error = core
            .request(request)
            .await
            .expect_err("a friend-request answer must be refused on tcp");
        assert_eq!(error.code, ErrorCode::Network, "{operation}: {error:?}");
        assert!(
            error
                .message
                .contains("the tcp transport has no friend requests"),
            "the refusal must name the transport: {operation}: {error:?}"
        );
    }

    core.shutdown().await.expect("shutdown");
}
