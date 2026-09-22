/*!
 * repl_session.rs
 *
 * Application case: attach a user interface to the core, in process.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `CliFrontend` on a `LocalClient`: the same line oriented REPL `meta-text
 *   --mode cli` runs, driven from a program that already has a core
 * - every user-visible string comes from `meta-text-tui`'s presenter, so an
 *   embedding program gets the wording, the slash commands and the event
 *   rendering for free
 *
 * ```bash
 * printf '/info\n/stats\n/quit\n' | cargo run --example repl_session
 * ```
 *
 * Run it with a terminal on stdin and it is the ordinary interactive REPL; run
 * it with a pipe and the session is scripted, because the prompt is only drawn
 * when stdin *and* stdout are terminals. `/quit`, Ctrl+C and end-of-file all end
 * the loop, and the core is stopped afterwards.
 */

use anyhow::{Context, Result};
use meta_text::cli::{AppMode, CliArgs};
use meta_text::config::AppConfig;
use meta_text::ipc::{CoreService, LocalClient};
use meta_text::CliFrontend;

/// Build a core, hand it to the REPL and stop once the REPL returns.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. The REPL keeps the state it is given, so the example's state is a
    //    temporary directory, exactly as in `embed_core`.
    let data_dir = tempfile::tempdir().context("create a data directory")?;

    let mut config = AppConfig::default();
    config.app.auto_save_interval = 0;
    config.network.port = 0;
    config.database.connection_string = data_dir
        .path()
        .join("core.db")
        .to_string_lossy()
        .into_owned();

    let args = CliArgs {
        data_dir: Some(data_dir.path().to_path_buf()),
        port: Some(0),
        // The mode is only a label in the session snapshot (`/info` shows it);
        // the front-end itself is chosen by which constructor is called here.
        mode: AppMode::Cli,
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args)
        .await
        .context("build the core service")?;
    service.start().await.context("start the subsystems")?;
    let core = service.spawn();

    // 2. `CliFrontend` takes the client contract, not the handle, so the same
    //    constructor would accept a `RemoteClient` pointed at another process
    //    (`cargo run -- --mode core --ipc-listen …`). The handle moves into the
    //    client: the REPL owns the session from here.
    let mut frontend = CliFrontend::new(LocalClient::new(core));

    // 3. Run until the user quits, stdin closes or Ctrl+C is pressed. Domain
    //    errors are rendered into the session; only a terminal failure returns.
    frontend.run().await.context("run the REPL")?;

    // 4. Nothing is left to stop: a front-end's `run` asks the core to shut down
    //    when it returns, and that request persists the session and closes the
    //    subsystems *before* it replies. A program that wants the core to outlive
    //    the interface drives the presenter itself, as `embed_core` shows.
    Ok(())
}
