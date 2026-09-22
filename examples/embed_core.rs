/*!
 * embed_core.rs
 *
 * Application case: embed the headless metaText core in a program of your own.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `CoreService` / `CoreHandle`: the actor and the cheap, cloneable client handle
 * - `LocalClient`: the in-process implementation of the `CoreClient` contract
 * - `Request` / `Reply` / `CoreEvent`: the same vocabulary a remote front-end speaks
 *
 * ```bash
 * cargo run --example embed_core
 * ```
 *
 * This is the library path, with no terminal and no user interface: every step
 * below is a value handed to `request()` or read from the event stream, which is
 * exactly what the REPL and the TUI do. A program that wants the layer boundary
 * enforced by the compiler can depend on `meta-text-core` directly; the imports
 * then read `meta_text_core::ipc::…` instead of `meta_text::ipc::…`.
 */

use std::time::Duration;

use anyhow::{bail, Context, Result};
use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::{CoreClient, CoreEvent, CoreService, LocalClient, Reply, Request};

/// How long the example waits for one event before giving up on it.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Build a core, serve a few requests, follow one event and stop cleanly.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. All state goes to a temporary directory. An example must not write into
    //    the working tree, which is the same rule the test suite follows.
    let data_dir = tempfile::tempdir().context("create a data directory")?;

    // 2. Configuration and arguments are the values the binary builds from
    //    `config.toml` and `argv`; here they are constructed in memory. Port `0`
    //    asks the OS for a free port, so a second instance can start without
    //    picking numbers by hand.
    let mut config = AppConfig::default();
    config.app.auto_save_interval = 0; // 0 disables the periodic snapshot
    config.network.port = 0;
    config.database.connection_string = data_dir
        .path()
        .join("core.db")
        .to_string_lossy()
        .into_owned();

    let args = CliArgs {
        data_dir: Some(data_dir.path().to_path_buf()),
        port: Some(0),
        nickname: Some("embedded".to_string()),
        ..CliArgs::default()
    };

    // 3. Construction is split from activation so a caller can fail before a
    //    socket is bound or a file is opened: `new` builds the subsystems,
    //    `start` brings them up, `spawn` moves the actor onto the runtime and
    //    hands back the handle.
    let mut service = CoreService::new(config, args)
        .await
        .context("build the core service")?;
    service.start().await.context("start the subsystems")?;
    let core = service.spawn();

    // 4. A front-end only ever sees the client contract. `subscribe` is taken
    //    before the first request so no event published from here on is missed;
    //    it returns `None` for a transport that cannot stream, which is why the
    //    caller has to decide (the in-process transport always can).
    let client = LocalClient::new(core.clone());
    let mut events = client
        .subscribe()
        .context("the in-process transport always streams events")?;

    // 5. The session snapshot is the same value `/info` renders. Everything in
    //    it is typed; no front-end parses a rendered line back into data.
    let Reply::Session { session } = client.request(Request::SessionInfo).await? else {
        bail!("expected a session reply");
    };
    println!("nickname       : {}", session.nickname);
    println!("identity       : {}", session.identity);
    println!("fingerprint    : {}", session.identity_fingerprint);
    println!("version        : {}", session.version);
    println!("encryption     : {}", session.encryption);
    println!("listening on   : {}", session.network_port);

    // 6. A request round-trip. `Ping` echoes its argument, so it doubles as the
    //    liveness probe a front-end and a monitor both use.
    let Reply::Pong { echo } = client
        .request(Request::Ping {
            echo: Some("hello from the embedding program".to_string()),
        })
        .await?
    else {
        bail!("expected a pong");
    };
    println!("ping echoed    : {}", echo.unwrap_or_default());

    // 7. A refusal is a value, not a failure of the session: the caller gets a
    //    machine readable code and a severity and keeps the same handle. An
    //    empty friend-request key cannot be accepted on any transport.
    match client
        .request(Request::AcceptPeerRequest {
            public_key: String::new(),
        })
        .await
    {
        Ok(reply) => println!("accept         : {reply:?}"),
        Err(error) => println!(
            "accept refused : {} [{:?}/{:?}]",
            error.message, error.code, error.severity
        ),
    }

    // 8. State changes are published as events. `SetNickname` is the smallest
    //    request that also broadcasts, so the stream below is deterministic.
    client
        .request(Request::SetNickname {
            nickname: "embedded-demo".to_string(),
        })
        .await?;
    loop {
        let event = tokio::time::timeout(EVENT_TIMEOUT, events.recv())
            .await
            .context("timed out waiting for the nickname event")?
            .context("the event stream closed")?;
        match event {
            CoreEvent::NicknameChanged { nickname } => {
                println!("nickname event : {nickname}");
                break;
            }
            // Startup and shutdown are announced on the same stream; only the
            // nickname change ends the loop below.
            CoreEvent::Ready { .. } => println!("event          : ready"),
            CoreEvent::Shutdown { reason } => println!("event          : shutdown ({reason})"),
            other => println!("event          : {other:?}"),
        }
    }

    // 9. Operational counters, as `/stats` reads them.
    let Reply::Statistics { statistics } = client.request(Request::Statistics).await? else {
        bail!("expected a statistics reply");
    };
    println!("messages sent  : {}", statistics.messages_sent);
    println!("uptime (s)     : {}", statistics.uptime_seconds);

    // 10. Shutdown is an ordinary request, so the same call works over a socket.
    //     The session snapshot, the transport and the database are closed
    //     *before* the reply is sent: returning from here means the state is
    //     already on disk.
    core.shutdown().await.context("shutdown")?;
    Ok(())
}
