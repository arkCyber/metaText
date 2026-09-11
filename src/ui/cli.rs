/*!
 * cli.rs
 *
 * Line oriented REPL front-end for the metaText core service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Reads lines from stdin on a bounded queue (backpressure aware)
 * - Renders core replies and events through the shared presenter
 * - Exits cleanly on `/quit`, EOF (Ctrl+D) or Ctrl+C
 */

use anyhow::Result;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::ipc::client::LocalClient;
use crate::tui::write_prompt;

use super::presenter::Presenter;

/// How many unread input lines may be buffered before reading is paused.
const INPUT_QUEUE_CAPACITY: usize = 64;

/// The interactive command line front-end.
#[derive(Debug)]
pub struct CliFrontend {
    /// Shared presentation and dispatch logic.
    presenter: Presenter,
}

impl CliFrontend {
    /// Create the front-end for a connected core client.
    #[must_use]
    pub fn new(core: LocalClient) -> Self {
        Self {
            presenter: Presenter::new(core),
        }
    }

    /// Run until the user quits, stdin closes or Ctrl+C is pressed.
    ///
    /// # Errors
    ///
    /// Returns an error only for an unrecoverable terminal failure; domain
    /// errors are rendered and the session continues.
    pub async fn run(&mut self) -> Result<()> {
        // Events are optional: without a subscriber the loop still works.
        let mut events = self.presenter.take_events();

        self.presenter.announce(false).await;

        let (input_tx, mut input_rx) = mpsc::channel::<String>(INPUT_QUEUE_CAPACITY);
        spawn_stdin_reader(input_tx);

        // Show the prompt immediately so it is obvious the REPL is ready.
        write_prompt();

        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        loop {
            tokio::select! {
                line = input_rx.recv() => {
                    match line {
                        Some(line) => {
                            if self.presenter.handle_line(&line).await {
                                break;
                            }
                            write_prompt();
                        }
                        // stdin reached EOF (Ctrl+D): treat it as a request to
                        // leave, matching the historical behaviour.
                        None => break,
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            if self.presenter.handle_event(&event).await {
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

        self.presenter.request_shutdown().await;
        Ok(())
    }
}

/// Forward lines typed on stdin into the input queue.
///
/// The task ends at EOF, which closes the queue and stops the REPL loop.
fn spawn_stdin_reader(sender: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sender.send(line).await.is_err() {
                break;
            }
        }
    });
}

impl CliFrontend {
    /// Access the presenter for tests and diagnostics.
    #[must_use]
    pub const fn presenter(&self) -> &Presenter {
        &self.presenter
    }
}
