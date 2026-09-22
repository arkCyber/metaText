/*!
 * tui_session.rs
 *
 * Application case: attach the full screen interface, in process.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `TuiFrontend` on a `LocalClient`: the interface `meta-text --mode tui` runs
 * - one front-end, two rendering modes: with a terminal on stdout it takes the
 *   alternate screen, without one it reads stdin and behaves like the REPL, so
 *   the same program is scriptable
 *
 * ```bash
 * # The full screen interface, built with the real terminal backend
 * cargo run --features terminal-ui --example tui_session
 *
 * # The same program scripted: no terminal on stdin or stdout, so the front-end
 * # falls back to line input and exits when the script ends
 * printf '/info\n/quit\n' | cargo run --example tui_session
 * ```
 *
 * `Ctrl+C` and `Ctrl+Q` quit the full screen interface; `/quit`, end-of-file and
 * `Ctrl+C` end the fallback. The feature only changes *how* the engine draws —
 * without `terminal-ui` the engine compiles to a console router — so the example
 * compiles and runs either way.
 */

use anyhow::{Context, Result};
use meta_text::cli::CliArgs;
use meta_text::config::AppConfig;
use meta_text::ipc::{CoreService, LocalClient};
use meta_text::tui::UiOptions;
use meta_text::ui::TuiFrontend;

/// Build a core, attach the full screen interface and stop when it returns.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. State, as everywhere in `examples/`, in a temporary directory.
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
        nickname: Some("tui-example".to_string()),
        ..CliArgs::default()
    };

    let mut service = CoreService::new(config, args)
        .await
        .context("build the core service")?;
    service.start().await.context("start the subsystems")?;
    let core = service.spawn();

    // 2. `start` prepares the terminal. It reports whether the alternate screen
    //    is really owned: `request_full_screen = true` is a *request*, refused
    //    when stdout is not a terminal — which is exactly what keeps a piped run
    //    from emitting control sequences into the pipe. The handle moves into the
    //    client: the interface owns the session from here.
    let mut frontend = TuiFrontend::start(
        LocalClient::new(core),
        "metaText",
        true,
        UiOptions::default(),
    )
    .await
    .context("prepare the terminal interface")?;
    println!("full screen: {}", frontend.is_full_screen());

    // 3. Run until the user quits. The interface renders events as they arrive,
    //    so a message from a peer appears without any polling here. Like the
    //    REPL, it asks the core to stop when it returns.
    frontend.run().await.context("run the interface")?;
    Ok(())
}
