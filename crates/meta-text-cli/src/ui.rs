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

use anyhow::{Context, Result};
use std::io::BufRead;
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
        spawn_stdin_reader(input_tx).context("start the keyboard reader")?;

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
/// The reader runs on a plain OS thread rather than through `tokio::io::stdin()`.
/// Tokio's stdin drives a **blocking** read on the runtime's blocking pool, and
/// dropping the runtime waits for that pool to drain; a read with no line (or
/// EOF) to return therefore keeps the process alive *after* `/quit` whenever
/// stdin stays open, which is exactly what an interactive terminal and a parent
/// process holding the pipe do. A thread the runtime does not join cannot block
/// teardown, so the interface returning is enough for the process to exit. EOF
/// still closes the queue and stops the REPL loop.
///
/// # Errors
///
/// Returns the failure to create the thread — a resource limit, typically
/// `EAGAIN` — rather than panicking. The reader is not optional: without it the
/// interface would show a prompt no input can ever reach, so the caller reports
/// the fault instead of running a session that cannot be typed into.
fn spawn_stdin_reader(sender: mpsc::Sender<String>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("stdin-reader".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                // Bind the read before matching so the stdin guard is released at
                // the end of the statement rather than living across the arms.
                let read = stdin.lock().read_line(&mut line);
                match read {
                    // EOF, or a read the terminal cannot satisfy any more.
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = line.trim_end_matches(['\n', '\r']).to_string();
                        // A closed queue means the interface has already left.
                        if sender.blocking_send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .map(|_| ())
}

impl CliFrontend {
    /// Access the presenter for tests and diagnostics.
    #[must_use]
    pub const fn presenter(&self) -> &Presenter {
        &self.presenter
    }
}
