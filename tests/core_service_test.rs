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
use meta_text::ipc::protocol::{ErrorCode, Reply, ResponseResult, SendOutcomeKind};
use meta_text::ipc::{
    CoreClient, CoreHandle, CoreService, CoreServiceOptions, LocalClient, Request,
};

/// Build and start a core service in an isolated workspace.
async fn started_service() -> (CoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
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
    (service.spawn(), dir)
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
