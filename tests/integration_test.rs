/*!
 * integration_test.rs
 *
 * End-to-end integration tests for the metaText library
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * These tests only use the public API of the `meta_text` crate and exercise
 * the subsystems together the way the binary does.
 */

use std::sync::Arc;

use meta_text::cli::CliArgs;
use meta_text::config::{
    AppConfig, CryptoConfig, DatabaseConfig, NetworkConfig, DEFAULT_KEEPALIVE_INTERVAL,
};
use meta_text::crypto::{CryptoManager, KEY_LENGTH};
use meta_text::database::DatabaseManager;
use meta_text::ipc::{CoreClient, CoreService, Request};
use meta_text::network::NetworkManager;
use meta_text::trust::PinStore;
use meta_text::types::{AppState, ContentType, MessageKind};

/// A configuration with a loopback network port and a temporary database file
fn test_config(db_path: &std::path::Path) -> AppConfig {
    let mut config = AppConfig::default();
    // Port 0 lets the OS pick a free port so tests never collide
    config.network.port = 0;
    config.database.connection_string = db_path.to_string_lossy().to_string();
    config
}

/// The configuration round-trips through save/load without losing data
#[tokio::test]
async fn test_config_roundtrip_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let config = AppConfig::default();
    config.save(&path).await.unwrap();

    let loaded = AppConfig::load(&path).await.unwrap();
    assert_eq!(loaded.app.name, config.app.name);
    assert_eq!(loaded.network.port, config.network.port);
    assert_eq!(
        loaded.database.connection_string,
        config.database.connection_string
    );
}

/// Encryption and decryption survive a full manager lifecycle
#[tokio::test]
async fn test_crypto_roundtrip_end_to_end() {
    let manager = CryptoManager::new(&CryptoConfig::default()).await.unwrap();
    assert_eq!(manager.key().len(), KEY_LENGTH);

    let message = "metaText integration message 🚀";
    let ciphertext = manager.encrypt_string(message).unwrap();
    assert_eq!(manager.decrypt_string(&ciphertext).unwrap(), message);
}

/// The network manager can bind, start and shut down cleanly
#[tokio::test]
async fn test_network_lifecycle() {
    let config = NetworkConfig {
        port: 0,
        bootstrap_nodes: vec![],
        connection_timeout: 1,
        max_connections: 8,
        keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        enable_upnp: false,
        enable_ipv6: false,
    };

    let crypto = Arc::new(CryptoManager::new(&CryptoConfig::default()).await.unwrap());
    let (event_sender, _event_receiver) = tokio::sync::mpsc::channel(1024);
    let mut manager = NetworkManager::new(&config, crypto, event_sender, PinStore::volatile())
        .await
        .unwrap();

    assert!(!manager.is_started());
    manager.start().await.unwrap();
    assert!(manager.is_started());
    assert!(manager.local_addr().is_some());
    assert_eq!(manager.connected_peers(), 0);

    manager.shutdown().await.unwrap();
    assert!(!manager.is_started());
}

/// Two peers sharing a passphrase can exchange a message over the public API
#[tokio::test]
async fn test_peers_exchange_messages_end_to_end() {
    use meta_text::crypto::CryptoManager as Crypto;
    use meta_text::types::AppEvent;

    let passphrase = "integration passphrase";
    let config = NetworkConfig {
        port: 0,
        bootstrap_nodes: vec![],
        connection_timeout: 5,
        max_connections: 8,
        keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        enable_upnp: false,
        enable_ipv6: false,
    };

    let (mut alice, mut alice_events) = {
        let crypto =
            Arc::new(Crypto::from_passphrase(&CryptoConfig::default(), passphrase).unwrap());
        let (sender, receiver) = tokio::sync::mpsc::channel(1024);
        (
            NetworkManager::new(&config, crypto, sender, PinStore::volatile())
                .await
                .unwrap(),
            receiver,
        )
    };
    let (mut bob, _bob_events) = {
        let crypto =
            Arc::new(Crypto::from_passphrase(&CryptoConfig::default(), passphrase).unwrap());
        let (sender, receiver) = tokio::sync::mpsc::channel(1024);
        (
            NetworkManager::new(&config, crypto, sender, PinStore::volatile())
                .await
                .unwrap(),
            receiver,
        )
    };

    alice.start().await.unwrap();
    bob.start().await.unwrap();

    let alice_addr = alice.local_addr().unwrap();
    bob.connect(&alice_addr.to_string()).await.unwrap();

    assert_eq!(
        bob.broadcast(b"integration hello", MessageKind::Text, ContentType::Text)
            .unwrap()
            .peers,
        1
    );

    // Handshake events (peer connected) may arrive before the message.
    let payload = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), alice_events.recv())
            .await
            .expect("no timeout")
            .expect("event");
        if let AppEvent::MessageReceived { payload, .. } = event {
            break payload;
        }
    };
    assert_eq!(String::from_utf8_lossy(&payload), "integration hello");

    alice.shutdown().await.unwrap();
    bob.shutdown().await.unwrap();
}

/// The database manager can be started and stopped repeatedly
#[tokio::test]
async fn test_database_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let config = DatabaseConfig {
        database_type: "sqlite".to_string(),
        connection_string: dir
            .path()
            .join("lifecycle.db")
            .to_string_lossy()
            .to_string(),
        max_connections: 2,
        // Migrations on: `false` is only accepted for a database whose schema already
        // exists, and this file is created by the test itself. The refusal for a
        // schema-less database is covered by
        // `database::tests::test_disabled_migrations_require_a_prepared_schema`.
        enable_migrations: true,
    };

    let mut manager = DatabaseManager::new(&config).await.unwrap();
    assert!(!manager.is_initialized());

    manager.start().await.unwrap();
    assert!(manager.is_initialized());

    manager.shutdown().await.unwrap();
    assert!(!manager.is_initialized());
}

/// With the `sqlite` feature messages survive an application restart
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_sqlite_history_survives_restart() {
    use meta_text::database::{MessageDirection, StoredMessage};

    let dir = tempfile::tempdir().unwrap();
    let config = DatabaseConfig {
        database_type: "sqlite".to_string(),
        connection_string: dir.path().join("history.db").to_string_lossy().to_string(),
        max_connections: 2,
        enable_migrations: true,
    };

    // First session: persist a single outgoing message.
    {
        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();
        assert!(manager.is_persistent());
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Outgoing,
                "Alice",
                "hello from the first session",
                27,
                MessageKind::Text,
                ContentType::Text,
            ))
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
    }

    // Second session: the history is still available.
    {
        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();
        assert_eq!(manager.message_count().await.unwrap(), 1);

        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].body, "hello from the first session");
        assert_eq!(history[0].direction, MessageDirection::Outgoing);
        manager.shutdown().await.unwrap();
    }
}

/// The core service initializes and answers requests over the private channel
#[tokio::test]
async fn test_core_service_initialization_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir.path().join("meta-text.db"));
    // The session snapshot lives next to `--data-dir`, so the test must pin it
    // to the temporary directory too. With `CliArgs::default()` the core would
    // fall back to the per-user data directory and the test would both read the
    // developer's real session (making `contacts.is_empty()` state dependent)
    // and write its throwaway contact back into it.
    let args = CliArgs {
        data_dir: Some(dir.path().to_path_buf()),
        ..CliArgs::default()
    };

    let service = CoreService::new(config, args).await;
    assert!(service.is_ok(), "the core service must initialise");

    // A freshly built service can already answer pure data requests; starting
    // the sockets is a separate, explicit step.
    let mut service = service.unwrap();
    service.start().await.expect("start");
    let handle = service.spawn();
    let client = meta_text::ipc::LocalClient::new(handle.clone());

    // The read-only requests must answer without touching the network.
    let reply = client
        .request(Request::Ping {
            echo: Some("integration".to_string()),
        })
        .await
        .expect("ping");
    assert_eq!(
        reply,
        meta_text::ipc::Reply::Pong {
            echo: Some("integration".to_string())
        }
    );

    let contacts = client
        .request(Request::ListContacts)
        .await
        .expect("contacts");
    assert!(matches!(
        contacts,
        meta_text::ipc::Reply::Contacts { contacts } if contacts.is_empty()
    ));

    // Mutations are visible through subsequent reads.
    client
        .request(Request::AddContact {
            identifier: "DID-INTEGRATION".to_string(),
            note: None,
        })
        .await
        .expect("add");
    let contacts = client
        .request(Request::ListContacts)
        .await
        .expect("contacts");
    assert!(matches!(
        contacts,
        meta_text::ipc::Reply::Contacts { contacts }
            if contacts.len() == 1 && contacts[0].name == "DID-INTEGRATION"
    ));

    handle.shutdown().await.expect("shutdown");
}

/// Application state starts with sensible defaults
#[test]
fn test_app_state_defaults() {
    let state = AppState::new();
    assert!(state.active_conversation.is_none());
    assert_eq!(state.statistics.messages_sent, 0);
    assert_eq!(state.statistics.messages_received, 0);
}
