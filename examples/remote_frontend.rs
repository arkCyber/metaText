/*!
 * remote_frontend.rs
 *
 * Application case: a front-end in *another process* — the socket client.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `RemoteClient`: the same `CoreClient` contract as `LocalClient`, spoken over
 *   the length prefixed JSON frames of the interface document
 * - server policy a client cannot talk its way out of: `Shutdown` is refused
 *   with `Unauthorized`, because only the hosting process may stop the backend
 * - events fan out across the socket, so a remote front-end renders a change
 *   exactly like an in-process one
 *
 * ```bash
 * # Terminal 1 — the backend, serving protocol clients on loopback
 * cargo run -- --mode core --ipc-listen 127.0.0.1:45999
 *
 * # Terminal 2 — this example
 * cargo run --example remote_frontend
 * ```
 *
 * The address defaults to `127.0.0.1:45999` and can be overridden as the first
 * argument; the token is the second argument or `METATEXT_IPC_TOKEN`. A client on
 * loopback needs no token unless the server was started with one.
 */

use anyhow::{bail, Context, Result};
use meta_text::ipc::{CoreClient, CoreEvent, RemoteClient, Reply, Request, PROTOCOL_VERSION};

/// Where `--ipc-listen 127.0.0.1:45999` puts the endpoint.
const DEFAULT_ADDRESS: &str = "127.0.0.1:45999";

/// How long the example waits for an event before giving up on it.
const EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Connect, ask a few questions, follow one event and say goodbye.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. Address and token. Both are deliberately outside the code: a front-end
    //    is configuration, not a constant.
    let mut argv = std::env::args().skip(1);
    let address = argv.next().unwrap_or_else(|| DEFAULT_ADDRESS.to_string());
    let token = argv
        .next()
        .or_else(|| std::env::var("METATEXT_IPC_TOKEN").ok());

    // 2. The handshake sends `Hello` with the version and the token and expects
    //    `Welcome`; a version the server does not speak and a wrong token are
    //    both refused here rather than at the first request.
    let client = RemoteClient::connect(&address, token, "remote_frontend example")
        .await
        .with_context(|| {
            format!(
                "connect to {address} — is a `--mode core --ipc-listen {address}` instance running?"
            )
        })?;
    println!("connected to {address} (protocol version {PROTOCOL_VERSION})");

    // 3. Subscribe first, so the change made below cannot be missed.
    let mut events = client
        .subscribe()
        .context("the TCP transport streams events too")?;

    // 4. Requests are typed exactly as in process; only the transport differs.
    let Reply::Session { session } = client.request(Request::SessionInfo).await? else {
        bail!("expected a session reply");
    };
    println!("host nickname  : {}", session.nickname);
    println!("host identity  : {}", session.identity);
    println!("transport      : {}", session.transport);
    println!("peers          : {}", session.connected_peers);
    println!("friends        : {}", session.friend_count);

    let Reply::Pong { echo } = client
        .request(Request::Ping {
            echo: Some("from another process".to_string()),
        })
        .await?
    else {
        bail!("expected a pong");
    };
    println!("ping echoed    : {}", echo.unwrap_or_default());

    // 5. A change made over the socket is published on the socket: the same
    //    `CoreEvent` the in-process front-end sees, one frame later.
    client
        .request(Request::SetNickname {
            nickname: "remote-client".to_string(),
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
            CoreEvent::Ready { .. } => println!("event          : ready"),
            other => println!("event          : {other:?}"),
        }
    }

    // 6. The server refuses what a socket client must not do: a remote caller
    //    cannot stop the backend it is only a guest of. The refusal is an
    //    ordinary error value, and the connection stays usable afterwards —
    //    which the next request proves.
    match client.request(Request::Shutdown).await {
        Ok(reply) => println!("shutdown       : {reply:?}"),
        Err(error) => println!(
            "shutdown       : refused — {} [{:?}]",
            error.message, error.code
        ),
    }
    let again = client
        .request(Request::Ping {
            echo: Some("still here".to_string()),
        })
        .await?;
    println!("after refusal  : {again:?}");

    // 7. Close politely. Dropping the client would also work, but `Goodbye` lets
    //    the server release the slot and its rate-limit budget immediately.
    client.goodbye().await;
    println!("disconnected");
    Ok(())
}
