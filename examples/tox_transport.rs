/*!
 * tox_transport.rs
 *
 * Application case: the Tox transport itself — an instance on the DHT.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `ToxClient` / `ToxConfig`: the instance, its 76 character address and the
 *   savedata file that makes the address survive a restart
 * - `default_bootstrap_nodes`: the DHT nodes a fresh instance dials
 * - `ToxEvent`: friend requests, connection changes and messages, as toxcore
 *   reports them
 *
 * ```bash
 * # Needs libtoxcore (macOS: `brew install toxcore`, Debian/Ubuntu:
 * # `apt install libtoxcore-dev`) and outbound UDP.
 * cargo run --features tox-protocol --example tox_transport
 *
 * # Hand the printed address to a friend, or dial one:
 * cargo run --features tox-protocol --example tox_transport -- <76 hex characters>
 * ```
 *
 * This drives the transport *directly*, which is what the core does underneath:
 * `meta-text --transport tox` wraps the same client, so contact list, history and
 * `/info` keep working unchanged. Use the core for a real conversation — this
 * example exists to show the layer, and to make `ToxError::Unavailable` visible
 * on a machine without the C library.
 */

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use meta_text::tox::{default_bootstrap_nodes, ToxClient, ToxConfig, ToxError};

/// How long the example watches the event stream before shutting down.
const WATCH: Duration = Duration::from_secs(10);

/// Start an instance, print its address, watch its events and stop.
fn main() -> Result<()> {
    // 1. The savedata file lives in a data directory; the identity (and therefore
    //    the address) is created on the first start and reloaded afterwards.
    let data_dir = tempfile::tempdir().context("create a data directory")?;

    let mut config = ToxConfig::new(data_dir.path(), "metaText example");
    config.bootstrap_nodes = default_bootstrap_nodes();
    println!("savedata      : {}", config.savedata_path().display());

    // 2. Without a usable libtoxcore the build still compiles — the module is a
    //    stub — so the failure arrives here as a value rather than as a link
    //    error. Say what to install instead of printing a backtrace.
    let client = match ToxClient::start(config) {
        Ok(client) => client,
        Err(ToxError::Unavailable) => {
            eprintln!(
                "this build has no usable libtoxcore: install it (macOS: `brew install \
                 toxcore`, Debian/Ubuntu: `apt install libtoxcore-dev`) and rebuild with \
                 `--features tox-protocol`"
            );
            return Ok(());
        }
        Err(error) => return Err(error).context("start the Tox instance"),
    };

    // 3. The address is what a friend adds: 38 bytes, printed as 76 hex
    //    characters, checksum included.
    println!("my Tox address: {}", client.address());

    // 4. Optionally request a friendship. Nothing happens until the other side
    //    accepts, so the request is queued rather than sent as a message.
    if let Some(peer) = std::env::args().nth(1) {
        let friend_number = client
            .add_friend(&peer, "hello from the metaText example")
            .context("ask for a friendship")?;
        println!("friend request sent (friend #{friend_number})");
    } else {
        println!("(pass a 76 character Tox address as the first argument to add a friend)");
    }

    // 5. Watch. A friend request from a stranger, a connection change and a
    //    message all arrive on the same stream; the DHT needs a few seconds
    //    before the first bootstrap answers.
    let deadline = Instant::now() + WATCH;
    let mut seen = 0_usize;
    while Instant::now() < deadline {
        if let Some(event) = client.next_event(Duration::from_millis(250)) {
            seen += 1;
            println!("event         : {event:?}");
        }
    }
    println!("watched {seen} event(s) in {}s", WATCH.as_secs());

    // 6. `shutdown` saves the identity and stops the worker thread. Dropping the
    //    client does the same on a best effort basis; being explicit reports an
    //    I/O failure instead of hiding it in a `Drop`.
    client.shutdown().context("stop the Tox instance")?;
    println!("stopped");
    Ok(())
}
