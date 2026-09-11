/*!
 * tui.rs
 *
 * Full screen front-end for the metaText core service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Owns the rendering engine and feeds it from the core protocol
 * - Falls back to the line oriented REPL when no terminal is available
 * - Publishes a status snapshot on every state change
 */

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::error::MetaTextResult;
use crate::ipc::client::LocalClient;
use crate::tui::{write_prompt, SharedTuiInfo, TuiInput, TuiManager};

use super::presenter::Presenter;

/// The full screen front-end.
#[derive(Debug)]
pub struct TuiFrontend {
    /// Shared presentation and dispatch logic.
    presenter: Presenter,

    /// Rendering engine (alternate screen, widgets, scrollback sink).
    manager: TuiManager,

    /// Input stream produced by the interface (or by stdin in fallback mode).
    input: mpsc::UnboundedReceiver<TuiInput>,

    /// Clone of the input sender, used to feed stdin in fallback mode.
    input_sender: mpsc::UnboundedSender<TuiInput>,

    /// Snapshot rendered by the engine.
    info: SharedTuiInfo,

    /// Whether the alternate screen is actually owned by the interface.
    full_screen: bool,
}

impl TuiFrontend {
    /// Start the interface and return the front-end.
    ///
    /// # Arguments
    ///
    /// * `core` - Connected core client.
    /// * `app_name` - Application name, shown in the initial snapshot.
    /// * `request_full_screen` - Whether the full screen interface was asked
    ///   for; it is still skipped when `stdout` is not a terminal.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::MetaTextError::UserInterface`] when the terminal
    /// cannot be prepared; callers may then fall back to the REPL.
    pub async fn start(
        core: LocalClient,
        app_name: &str,
        request_full_screen: bool,
    ) -> MetaTextResult<Self> {
        let (sender, input) = mpsc::unbounded_channel::<TuiInput>();
        let mut manager = TuiManager::new(app_name, sender.clone()).await?;
        manager.start(request_full_screen).await?;

        let info = manager.info_handle();
        let full_screen = manager.is_active();

        Ok(Self {
            presenter: Presenter::new(core),
            manager,
            input,
            input_sender: sender,
            info,
            full_screen,
        })
    }

    /// Whether the alternate screen is owned by the interface.
    #[must_use]
    pub const fn is_full_screen(&self) -> bool {
        self.full_screen
    }

    /// Run until the user quits or a signal arrives.
    ///
    /// # Errors
    ///
    /// Returns an error only for an unrecoverable terminal failure.
    pub async fn run(&mut self) -> Result<()> {
        let mut events = self.presenter.take_events();

        self.presenter.announce(self.full_screen).await;
        self.publish();

        // Without the alternate screen the interface does not read stdin, so
        // the front-end owns the input stream exactly like the plain REPL.
        if !self.full_screen {
            self.spawn_stdin_reader();
            write_prompt();
        }

        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        loop {
            tokio::select! {
                input = self.input.recv() => {
                    match input {
                        Some(TuiInput::Line(line)) => {
                            if self.presenter.handle_line(&line).await {
                                break;
                            }
                            self.publish();
                            if !self.full_screen {
                                write_prompt();
                            }
                        }
                        Some(TuiInput::Quit) | None => break,
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            let stop = self.presenter.handle_event(&event).await;
                            self.publish();
                            if stop {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("⚠️ The interface lagged {skipped} event(s)");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = &mut ctrl_c => {
                    info!("🛑 Received Ctrl+C, shutting down");
                    break;
                }
            }
        }

        self.manager
            .shutdown()
            .await
            .context("Failed to stop the terminal interface")?;
        self.presenter.request_shutdown().await;
        Ok(())
    }

    /// Publish the latest snapshot to the rendering engine.
    fn publish(&self) {
        let snapshot = self.presenter.tui_info();
        if let Ok(mut guard) = self.info.lock() {
            *guard = snapshot;
        }
    }

    /// Feed stdin into the input queue (fallback REPL mode only).
    fn spawn_stdin_reader(&self) {
        let sender = self.input_sender.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if sender.send(TuiInput::Line(line)).is_err() {
                    break;
                }
            }
            let _ = sender.send(TuiInput::Quit);
        });
    }
}
