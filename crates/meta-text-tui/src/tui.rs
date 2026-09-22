/*!
 * tui.rs
 *
 * Terminal user interface for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 */

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

#[cfg(not(feature = "terminal-ui"))]
use tracing::warn;
use tracing::{debug, info};

use crate::error::MetaTextResult;

#[cfg(feature = "terminal-ui")]
use crate::error::MetaTextError;
#[cfg(feature = "terminal-ui")]
use crate::utils::truncate_string;
#[cfg(feature = "terminal-ui")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "terminal-ui")]
use tui::style::Style;

/// How far the conversation pane is scrolled, and how far it can be.
///
/// The key loop owns the offset and moves it, while the pane is the side that
/// knows its own height and how many rows the log needs. Publishing the page size
/// and the bound the other way round keeps one page per key press: a page is the
/// pane's height in *rows*, not five log lines, and those two are the same only
/// while no line wraps.
#[cfg(feature = "terminal-ui")]
#[derive(Debug)]
struct PaneScroll {
    /// Rows one `PageUp` / `PageDown` moves.
    page: AtomicUsize,

    /// Largest offset that still shows log content, or [`usize::MAX`] while the
    /// pane has not reached the start of the log yet (it learns the bound by
    /// walking back to it) - a value that never blocks a key press.
    limit: AtomicUsize,

    /// Rows that arrived since the previous frame, for a pane that must not
    /// follow them (`[ui] auto_scroll = false`).
    arrived: AtomicUsize,

    /// How many log lines the previous frame drew, so the next one can tell which
    /// lines are new.
    seen_lines: AtomicUsize,

    /// Whether a first frame has been drawn. Nothing has "arrived" before there is
    /// a previous frame to compare with, and a pane that does not auto-scroll would
    /// otherwise treat the startup banner as output that came in while the user was
    /// reading.
    primed: AtomicBool,
}

#[cfg(feature = "terminal-ui")]
impl PaneScroll {
    /// Move the offset by one page, or by nothing when it is already at the bound.
    ///
    /// `PageUp` and a mouse wheel both come through here, so the mark in the pane
    /// title means the same thing whichever one the user reached for.
    fn step(&self, scroll: usize, towards_older: bool) -> usize {
        let page = self.page.load(Ordering::Relaxed).max(1);
        if towards_older {
            scroll
                .saturating_add(page)
                .min(self.limit.load(Ordering::Relaxed))
        } else {
            scroll.saturating_sub(page)
        }
    }

    /// The rows the log gained since the previous frame, and remember this one.
    ///
    /// Only a pane that does not auto-scroll needs the count, and it has to be
    /// measured in display rows: the same line count is a different amount of
    /// movement once a line wraps.
    fn count_arrivals(&self, lines: &[String], width: usize) -> usize {
        let seen = self.seen_lines.swap(lines.len(), Ordering::Relaxed);

        // Nothing has arrived before the first frame: the log it draws was already
        // there when the interface started.
        if !self.primed.swap(true, Ordering::Relaxed) {
            return 0;
        }
        if lines.len() <= seen {
            return 0;
        }
        lines[seen..]
            .iter()
            .map(|line| wrap_line(line, width).len())
            .sum()
    }
}

/// The palette a full screen interface draws with (`[ui] theme`).
///
/// A theme is a choice rather than a flag, so the two values the interface can draw
/// are named here; the configuration refuses any other name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Colours for a dark background (the default).
    #[default]
    Dark,
    /// Colours for a light background, where dark grey is unreadable.
    Light,
}

/// The `[ui]` preferences a front-end passes to the full screen interface.
///
/// The presentation layer cannot read the configuration itself - it is written
/// against the core protocol, not the backend - so the composition root hands the
/// applicable settings over. Every field is a promise the interface keeps; a
/// setting it cannot honour (see `message_format`) is refused by the
/// configuration instead of being accepted and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiOptions {
    /// Draw with colours (`[ui] enable_colors`). Without them every style is the
    /// terminal's own, which is what a pipe, a screen reader or a monochrome
    /// terminal needs.
    pub colors: bool,

    /// The palette to draw with (`[ui] theme`).
    pub theme: Theme,

    /// Capture the mouse and scroll the conversation pane with its wheel
    /// (`[ui] enable_mouse`). The capture also takes over text selection, which
    /// most terminals restore with a modifier (`Shift`, or `Option` on macOS).
    pub mouse: bool,

    /// Follow new output. With this off the pane keeps showing the rows it was
    /// showing while output arrives, and the title says so.
    pub auto_scroll: bool,
}

impl Default for UiOptions {
    /// The shipped `config.toml` defaults, so a front-end that passes nothing
    /// behaves like one that loaded the reference configuration.
    fn default() -> Self {
        Self {
            colors: true,
            theme: Theme::Dark,
            mouse: true,
            auto_scroll: true,
        }
    }
}

#[cfg(feature = "terminal-ui")]
impl Default for PaneScroll {
    fn default() -> Self {
        Self {
            page: AtomicUsize::new(1),
            limit: AtomicUsize::new(usize::MAX),
            arrived: AtomicUsize::new(0),
            seen_lines: AtomicUsize::new(0),
            primed: AtomicBool::new(false),
        }
    }
}

/// Maximum number of console lines kept for the TUI scrollback.
pub const OUTPUT_CAPACITY: usize = 1000;

/// How many submitted lines the full screen input keeps for recall.
pub const INPUT_HISTORY_CAPACITY: usize = 100;

/// Editable single line input buffer with a command history.
///
/// The full screen interface reads raw key events instead of buffered lines,
/// so it needs its own line editor: this type owns the draft text, the cursor
/// position and the history that `Up` / `Down` navigate. It is deliberately
/// independent of any terminal, which makes the editing rules unit testable
/// without a TTY.
///
/// Positions are counted in `char`s, so the cursor stays correct for multi-byte
/// input. The history is bounded by [`INPUT_HISTORY_CAPACITY`].
#[derive(Debug, Default)]
pub struct InputBuffer {
    /// The draft line, split into characters for cursor arithmetic.
    chars: Vec<char>,

    /// Cursor position as a character index in `0..=chars.len()`.
    cursor: usize,

    /// Submitted, non-empty lines, oldest first.
    history: Vec<String>,

    /// Index into `history` while browsing it, or `None` when editing.
    history_index: Option<usize>,

    /// Draft preserved while browsing the history, restored on `Down`.
    draft: String,
}

impl InputBuffer {
    /// Create an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current draft as a `String`.
    #[must_use]
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position as a character offset from the start of the line.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the draft is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Insert one character at the cursor and move past it.
    pub fn insert(&mut self, character: char) {
        self.chars.insert(self.cursor, character);
        self.cursor += 1;
        self.stop_browsing();
    }

    /// Insert a whole string at the cursor (bracketed paste).
    ///
    /// The editor is one line, so a newline cannot be part of a draft: pasting a
    /// paragraph would put characters into it that the row cannot draw and that
    /// the user cannot see to edit. Line breaks therefore become spaces - the
    /// message stays one line, which is what the pane and the transport expect -
    /// and every other control character is dropped rather than sent as part of
    /// a message.
    pub fn insert_str(&mut self, text: &str) {
        for character in text.chars() {
            let character = match character {
                '\n' | '\r' | '\t' => ' ',
                other if other.is_control() => continue,
                other => other,
            };
            self.chars.insert(self.cursor, character);
            self.cursor += 1;
        }
        self.stop_browsing();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
        self.stop_browsing();
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
        self.stop_browsing();
    }

    /// Drop the whole draft and put the cursor back at the start (Ctrl+U).
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.stop_browsing();
    }

    /// Move the cursor one character left.
    pub const fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one character right.
    pub const fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// Move the cursor to the start of the line (Home / Ctrl+A).
    pub const fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the line (End / Ctrl+E).
    pub const fn move_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Submit the draft: return it, clear the line and record it in history.
    ///
    /// Whitespace-only lines are returned but not stored, and a line equal to
    /// the newest entry is not stored twice, so repeatedly pressing Enter does
    /// not fill the history.
    pub fn submit(&mut self) -> String {
        let line = self.text();
        self.chars.clear();
        self.cursor = 0;
        self.stop_browsing();

        if !line.trim().is_empty() && self.history.last() != Some(&line) {
            if self.history.len() >= INPUT_HISTORY_CAPACITY {
                self.history.remove(0);
            }
            self.history.push(line.clone());
        }
        line
    }

    /// Recall the previous entry (`Up`).
    ///
    /// The first call stashes the current draft so it can be restored; further
    /// calls walk back through the history and stop at the oldest entry.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }

        let next = match self.history_index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
        };
        self.history_index = Some(next);
        self.set_from_history(next);
    }

    /// Recall the next entry (`Down`), restoring the draft past the newest one.
    pub fn history_next(&mut self) {
        match self.history_index {
            // Not browsing: nothing to recall forward.
            None => {}
            Some(index) if index + 1 < self.history.len() => {
                self.history_index = Some(index + 1);
                self.set_from_history(index + 1);
            }
            // Past the newest entry: give the user their half-typed draft back.
            Some(_) => {
                self.history_index = None;
                let draft = std::mem::take(&mut self.draft);
                self.chars = draft.chars().collect();
                self.cursor = self.chars.len();
            }
        }
    }

    /// The stored history, oldest first.
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the draft with `history[index]`, cursor at the end.
    fn set_from_history(&mut self, index: usize) {
        if let Some(line) = self.history.get(index) {
            self.chars = line.chars().collect();
            self.cursor = self.chars.len();
        }
    }

    /// Stop browsing the history and drop the stashed draft.
    fn stop_browsing(&mut self) {
        self.history_index = None;
        self.draft.clear();
    }
}

/// Input produced by the full screen interface.
///
/// The rendering engine deliberately knows nothing about the application
/// domain: it only reports what the user typed and when they want to leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiInput {
    /// A line the user submitted with Enter.
    Line(String),

    /// The user asked to leave (Ctrl+C or Ctrl+Q).
    Quit,
}

/// Process wide console output sink.
///
/// The application writes every user facing line through [`write_line`].
/// While the sink is disabled (the default, i.e. plain REPL mode) lines go
/// straight to `stdout`. When the full screen TUI is active the sink is
/// enabled instead and buffered lines are rendered inside the interface, so
/// the terminal is never corrupted by stray writes.
#[derive(Debug, Default)]
pub struct OutputSink {
    /// Whether output is currently captured instead of printed
    enabled: std::sync::atomic::AtomicBool,

    /// Buffered output lines (bounded by [`OUTPUT_CAPACITY`])
    lines: Mutex<VecDeque<String>>,
}

impl OutputSink {
    /// Start capturing output instead of printing it.
    pub fn enable(&self) {
        self.enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop capturing output and drop the buffered lines.
    pub fn disable(&self) {
        self.enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.clear();
    }

    /// Check whether output is currently captured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Append a single line, evicting the oldest line when full.
    pub fn push(&self, line: impl Into<String>) {
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() >= OUTPUT_CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line.into());
        }
    }

    /// Copy the buffered lines for rendering.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop every buffered line.
    pub fn clear(&self) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.clear();
        }
    }
}

/// Global output sink instance.
static OUTPUT_SINK: LazyLock<OutputSink> = LazyLock::new(OutputSink::default);

/// Access the global console output sink.
#[must_use]
pub fn output_sink() -> &'static OutputSink {
    &OUTPUT_SINK
}

/// Write one line of application output.
///
/// Prints to `stdout` unless the TUI is capturing output, in which case the
/// line is buffered for the next rendered frame.
pub fn write_line(line: &str) {
    let sink = output_sink();
    if sink.is_enabled() {
        sink.push(line);
    } else {
        println!("{line}");
    }
}

/// Clear the console screen or the TUI scrollback.
pub fn clear_output() {
    let sink = output_sink();
    if sink.is_enabled() {
        sink.clear();
    } else {
        print!("\u{1b}[2J\u{1b}[H");
    }
}

/// Decide whether the interactive prompt should be emitted.
///
/// Split out from [`write_prompt`] so the policy can be unit tested without a
/// real terminal. The prompt is shown only for a plain REPL running on a real
/// terminal: scripted input (`stdin`/`stdout` not a TTY) and the full screen
/// interface (output sink enabled) must stay free of it.
#[must_use]
const fn should_show_prompt(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    sink_enabled: bool,
) -> bool {
    stdin_is_terminal && stdout_is_terminal && !sink_enabled
}

/// Write the interactive REPL prompt (`>> `) while waiting for the next command.
///
/// The prompt is printed without a trailing newline and flushed immediately so
/// it is visible before the user starts typing. It is deliberately suppressed
/// when the process is not attached to a real terminal (piped or scripted
/// input) and while the full screen interface owns the terminal, keeping the
/// output stream clean for automation and free of stray writes for the TUI.
pub fn write_prompt() {
    use std::io::IsTerminal;

    if !should_show_prompt(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        output_sink().is_enabled(),
    ) {
        return;
    }

    print!(">> ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Structured status information rendered by the TUI header and side panels.
#[derive(Debug, Default, Clone)]
pub struct TuiInfo {
    /// Application name, shown as the header's title
    pub app_name: String,

    /// Local nickname
    pub nickname: String,

    /// Local status message
    pub status_message: String,

    /// Public identity (DID-like hex string)
    pub identity: String,

    /// Active application mode
    pub mode: String,

    /// Short network summary
    pub network: String,

    /// Short database summary
    pub database: String,

    /// Encryption summary
    pub encryption: String,

    /// Contact display names
    pub friends: Vec<String>,

    /// Nicknames of the currently connected peers
    pub peers: Vec<String>,

    /// Addresses still being retried in the background
    pub pending_peers: usize,

    /// Name of the active conversation, if any
    pub active_chat: Option<String>,

    /// Messages sent during this session
    pub messages_sent: u64,

    /// Messages received during this session
    pub messages_received: u64,

    /// Bytes sent during this session
    pub bytes_sent: u64,

    /// Bytes received during this session
    pub bytes_received: u64,
}

/// Shared handle to the TUI status information.
pub type SharedTuiInfo = Arc<Mutex<TuiInfo>>;

/// Terminal user interface manager
#[derive(Debug)]
pub struct TuiManager {
    /// Event sender for communicating with main app
    event_sender: tokio::sync::mpsc::UnboundedSender<TuiInput>,

    /// The `[ui]` preferences the interface is drawn with
    ///
    /// Only the full screen interface reads them: without `terminal-ui` the manager
    /// falls back to the plain REPL, whose output the CLI front-end formats itself.
    /// The field is still stored in that build (the constructor's signature does not
    /// change with the feature), so it is dead only there.
    #[cfg_attr(not(feature = "terminal-ui"), allow(dead_code))]
    options: UiOptions,

    /// Whether TUI is started
    started: bool,

    /// Shared snapshot of the information rendered by the TUI
    info: SharedTuiInfo,

    /// Whether the full screen interface is currently capturing the terminal
    #[cfg(feature = "terminal-ui")]
    active: Arc<AtomicBool>,

    /// Background task running the full screen interface
    #[cfg(feature = "terminal-ui")]
    task: Option<tokio::task::JoinHandle<()>>,
}

/// The manager's lifecycle is `async`/non-`const` by contract rather than by
/// what the body currently does: the terminal backend lives behind the
/// `terminal-ui` feature, which means the very same method awaits in one build
/// (`spawn_blocking`, the rendering task) and does not in the other, and
/// `is_active` is a `const false` without the feature. The signature is shared
/// with the callers that await it, so it must not change with the feature.
#[allow(clippy::unused_async, clippy::missing_const_for_fn)]
impl TuiManager {
    /// Create a new TUI manager
    ///
    /// # Errors
    ///
    /// Currently this never fails; the `Result` is kept so that future UI
    /// backends can validate the terminal before the manager is constructed.
    pub async fn new(
        app_name: &str,
        options: UiOptions,
        event_sender: tokio::sync::mpsc::UnboundedSender<TuiInput>,
    ) -> MetaTextResult<Self> {
        let timestamp = chrono::Utc::now();
        info!(
            "📱 [{}] Initializing TUI manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        // The header renders the configured `[app] name`, so it is installed here
        // rather than left to the first refresh: the interface draws its title
        // before any reply has arrived.
        let initial = TuiInfo {
            app_name: app_name.to_string(),
            ..TuiInfo::default()
        };

        let manager = Self {
            event_sender,
            started: false,
            options,
            info: Arc::new(Mutex::new(initial)),
            #[cfg(feature = "terminal-ui")]
            active: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "terminal-ui")]
            task: None,
        };

        info!(
            "✅ [{}] TUI manager initialized",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        Ok(manager)
    }

    /// Get a shared handle to the information rendered by the TUI
    ///
    /// # Returns
    ///
    /// Returns a clone of the `Arc<Mutex<TuiInfo>>` so the application can
    /// publish status updates without borrowing the manager.
    #[must_use]
    pub fn info_handle(&self) -> SharedTuiInfo {
        Arc::clone(&self.info)
    }

    /// Start the TUI manager
    ///
    /// # Arguments
    ///
    /// * `full_screen` - When `true` (and the crate is built with the
    ///   `terminal-ui` feature and `stdout` is a real terminal) this switches
    ///   to the alternate screen and spawns the rendering/input loop. When
    ///   `false`, or when no TTY is available, the manager only tracks its
    ///   lifecycle state and the caller keeps using the plain REPL.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::MetaTextError::UserInterface`] when the terminal
    /// cannot be put into raw mode or the alternate screen cannot be entered.
    pub async fn start(&mut self, full_screen: bool) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "📱 [{}] Starting TUI manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        if self.started {
            return Ok(());
        }

        #[cfg(feature = "terminal-ui")]
        if full_screen {
            self.spawn_interface();
        }

        // Without the full screen backend the request is accepted but the
        // caller keeps using the plain REPL, so the flag is intentionally
        // unused in this configuration.
        #[cfg(not(feature = "terminal-ui"))]
        {
            let _ = full_screen;
            if full_screen {
                warn!(
                    "⚠️ TUI mode was requested but this build has no `terminal-ui` \
                     feature; continuing with the plain command line interface. \
                     Rebuild with `--features terminal-ui` for the full screen UI."
                );
            }
        }

        self.started = true;

        info!(
            "✅ [{}] TUI manager started successfully",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Shutdown the TUI manager
    ///
    /// Stops the full screen interface (if any) and restores the terminal to
    /// its original state.
    ///
    /// # Errors
    ///
    /// This implementation never fails, but keeps the `Result` return type so
    /// that future UI backends can report errors.
    pub async fn shutdown(&mut self) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "📱 [{}] Shutting down TUI manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        #[cfg(feature = "terminal-ui")]
        {
            // Ask the interface loop to stop and wait for it to restore the
            // terminal before the application prints anything else.
            self.active.store(false, Ordering::SeqCst);
            if let Some(task) = self.task.take() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
            }
            output_sink().disable();
        }

        self.started = false;

        info!(
            "✅ [{}] TUI manager shutdown completed",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Display banner in TUI mode
    ///
    /// A no-op unless the caller renders the banner itself; kept as a function
    /// rather than a macro so a front-end can call it unconditionally. Not
    /// `async`: the only work is a log line.
    pub fn display_banner(&self) {
        debug!("📱 Displaying banner in TUI mode");
    }

    /// Check if TUI is started
    #[must_use]
    pub const fn is_started(&self) -> bool {
        self.started
    }

    /// Check whether the full screen interface currently owns the terminal
    ///
    /// # Returns
    ///
    /// Returns `true` only when the `terminal-ui` feature is enabled *and* the
    /// interface successfully took over the terminal. In that state the
    /// application must route user facing output through
    /// [`crate::tui::write_line`] instead of printing directly.
    #[must_use]
    pub fn is_active(&self) -> bool {
        #[cfg(feature = "terminal-ui")]
        {
            self.active.load(Ordering::SeqCst)
        }
        #[cfg(not(feature = "terminal-ui"))]
        {
            false
        }
    }

    /// Get a clone of the shared event sender
    ///
    /// The event channel connects the user interface with the application
    /// coordinator. Input handlers (for example the interactive REPL reader)
    /// use this sender to forward input lines from the interface.
    ///
    /// # Returns
    ///
    /// Returns a cloned handle to the unbounded application event sender.
    #[must_use]
    pub fn event_sender(&self) -> tokio::sync::mpsc::UnboundedSender<TuiInput> {
        self.event_sender.clone()
    }
}

/// Full screen terminal interface, only compiled with the `terminal-ui` feature.
#[cfg(feature = "terminal-ui")]
impl TuiManager {
    /// Take over the terminal and spawn the render/input loop.
    ///
    /// Terminal setup happens inside the background task, so failures fall
    /// back to the plain REPL instead of propagating to the caller.
    fn spawn_interface(&mut self) {
        use std::io::IsTerminal;

        if !std::io::stdout().is_terminal() {
            info!("📱 Output is not a terminal; keeping the plain REPL");
            return;
        }

        // Enable capture before the caller starts emitting so that no line is
        // printed over the alternate screen by accident.
        output_sink().enable();
        self.active.store(true, Ordering::SeqCst);

        let sender = self.event_sender.clone();
        let info = Arc::clone(&self.info);
        let active = Arc::clone(&self.active);
        let options = self.options;

        self.task = Some(tokio::task::spawn_blocking(move || {
            if let Err(error) = run_interface(&sender, &info, &active, options) {
                // Terminal setup failed: fall back to the plain REPL.
                active.store(false, Ordering::SeqCst);
                output_sink().disable();
                info!(?error, "TUI interface did not start");
            }
        }));
    }
}

/// Set up the terminal, run the render loop and always restore the terminal.
///
/// # Errors
///
/// Returns [`MetaTextError::UserInterface`] when raw mode or the alternate
/// screen cannot be enabled.
#[cfg(feature = "terminal-ui")]
fn run_interface(
    sender: &tokio::sync::mpsc::UnboundedSender<TuiInput>,
    info: &SharedTuiInfo,
    active: &AtomicBool,
    options: UiOptions,
) -> Result<(), MetaTextError> {
    use crossterm::event::DisableBracketedPaste;
    use crossterm::execute;
    use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

    let mut terminal = setup_terminal(options).map_err(|error| MetaTextError::UserInterface {
        message: format!("Failed to initialise the terminal interface: {error}"),
        component: "Tui".to_string(),
        source: Some(Box::new(error)),
    })?;

    render_loop(&mut terminal, sender, info, active, options);

    // Restore the terminal no matter how the loop ended.
    let _ = terminal.show_cursor();
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    );
    // A capture that is left on keeps the terminal from selecting text after the
    // interface is gone, so it is released explicitly rather than by luck.
    if options.mouse {
        let _ = execute!(
            terminal.backend_mut(),
            crossterm::event::DisableMouseCapture
        );
    }
    Ok(())
}

/// Enter raw mode and the alternate screen, capturing the mouse when asked to.
#[cfg(feature = "terminal-ui")]
fn setup_terminal(
    options: UiOptions,
) -> std::io::Result<tui::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>> {
    use crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    use tui::backend::CrosstermBackend;
    use tui::Terminal;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    if options.mouse {
        // `[ui] enable_mouse`: the wheel scrolls the conversation pane. The
        // terminal's own text selection then needs a modifier, which is why this
        // is a setting rather than a constant.
        execute!(stdout, EnableMouseCapture)?;
    }
    Terminal::new(CrosstermBackend::new(stdout))
}

/// What one key press asks the full screen interface to do.
///
/// The mapping is separated from the event loop so it can be unit tested
/// without a terminal; the loop only performs the effect.
#[cfg(feature = "terminal-ui")]
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    /// The key only changed the input buffer; nothing else to do.
    None,

    /// A complete line was submitted and should be dispatched.
    Line(String),

    /// The user asked to leave.
    Quit,

    /// The user asked to clear the scrollback (Ctrl+L).
    ClearOutput,

    /// Scroll the conversation pane up (`PageUp`).
    ScrollUp,

    /// Scroll the conversation pane down (`PageDown`).
    ScrollDown,
}

/// Apply one key press to the input buffer and report what to do next.
///
/// `Char` keys are treated as text unless a control modifier is present, so
/// `Ctrl+A` / `Ctrl+E` jump to the line ends and `Ctrl+U` clears it instead of
/// inserting letters. Bracketed paste is handled by the caller.
#[cfg(feature = "terminal-ui")]
fn apply_key(input: &mut InputBuffer, key: crossterm::event::KeyEvent) -> KeyAction {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    if key.kind != KeyEventKind::Press {
        return KeyAction::None;
    }

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c' | 'q') if control => KeyAction::Quit,
        KeyCode::Char('u') if control => {
            input.clear();
            KeyAction::None
        }
        KeyCode::Char('a') if control => {
            input.move_home();
            KeyAction::None
        }
        KeyCode::Char('e') if control => {
            input.move_end();
            KeyAction::None
        }
        KeyCode::Char('l') if control => KeyAction::ClearOutput,
        KeyCode::Char(character) if !control => {
            input.insert(character);
            KeyAction::None
        }
        KeyCode::Backspace => {
            input.backspace();
            KeyAction::None
        }
        KeyCode::Delete => {
            input.delete();
            KeyAction::None
        }
        KeyCode::Left => {
            input.move_left();
            KeyAction::None
        }
        KeyCode::Right => {
            input.move_right();
            KeyAction::None
        }
        KeyCode::Home => {
            input.move_home();
            KeyAction::None
        }
        KeyCode::End => {
            input.move_end();
            KeyAction::None
        }
        KeyCode::Up => {
            input.history_previous();
            KeyAction::None
        }
        KeyCode::Down => {
            input.history_next();
            KeyAction::None
        }
        KeyCode::Esc => {
            input.clear();
            KeyAction::None
        }
        KeyCode::Enter => KeyAction::Line(input.submit()),
        KeyCode::PageUp => KeyAction::ScrollUp,
        KeyCode::PageDown => KeyAction::ScrollDown,
        _ => KeyAction::None,
    }
}

/// Poll for keyboard input and redraw the interface until asked to stop.
#[cfg(feature = "terminal-ui")]
fn render_loop(
    terminal: &mut tui::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>,
    sender: &tokio::sync::mpsc::UnboundedSender<TuiInput>,
    info: &SharedTuiInfo,
    active: &AtomicBool,
    options: UiOptions,
) {
    use crossterm::event::{self, Event};
    use std::time::Duration;

    let mut input = InputBuffer::new();
    let mut scroll: usize = 0;
    let scroll_state = PaneScroll::default();
    let palette = Palette::from_options(options);

    while active.load(Ordering::SeqCst) {
        let lines = output_sink().snapshot();
        let snapshot = info.lock().map(|guard| guard.clone()).unwrap_or_default();
        let draft = input.text();

        let state = FrameState {
            info: &snapshot,
            lines: &lines,
            input: &draft,
            cursor: input.cursor(),
            scroll,
            pane: &scroll_state,
            palette: &palette,
        };
        let _ = terminal.draw(|frame| draw_interface(frame, &state));

        // With auto-scroll off the pane keeps showing what it was showing: the rows
        // that arrived are exactly how far the view would otherwise have been
        // pushed, so the offset grows by them (and the title then says it is not
        // following, which is the same mark `PageUp` leaves).
        if !options.auto_scroll {
            let arrived = scroll_state.arrived.swap(0, Ordering::Relaxed);
            let limit = scroll_state.limit.load(Ordering::Relaxed);
            scroll = scroll.saturating_add(arrived).min(limit);
        }

        if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }

        match event::read() {
            Ok(Event::Key(key)) => match apply_key(&mut input, key) {
                KeyAction::None => {}
                KeyAction::Quit => {
                    let _ = sender.send(TuiInput::Quit);
                    break;
                }
                KeyAction::Line(line) => {
                    scroll = 0;
                    if sender.send(TuiInput::Line(line)).is_err() {
                        break;
                    }
                }
                KeyAction::ClearOutput => {
                    clear_output();
                    scroll = 0;
                }
                KeyAction::ScrollUp => scroll = scroll_state.step(scroll, true),
                KeyAction::ScrollDown => scroll = scroll_state.step(scroll, false),
            },
            // The wheel scrolls the same pane the page keys do, because mouse
            // capture was enabled at startup when the options asked for it.
            Ok(Event::Mouse(mouse)) => match mouse.kind {
                crossterm::event::MouseEventKind::ScrollUp => {
                    scroll = scroll_state.step(scroll, true);
                }
                crossterm::event::MouseEventKind::ScrollDown => {
                    scroll = scroll_state.step(scroll, false);
                }
                _ => {}
            },
            // Bracketed paste arrives as one event instead of a key storm.
            Ok(Event::Paste(text)) => input.insert_str(&text),
            _ => {}
        }
    }
}

/// Everything one frame is drawn from.
///
/// The draw functions take this rather than seven parameters: a frame is assembled
/// once per tick, and a request that grows another piece of state would otherwise
/// thread another argument through the whole chain.
#[cfg(feature = "terminal-ui")]
#[derive(Debug)]
struct FrameState<'a> {
    /// Snapshot of the session the header and panes render.
    info: &'a TuiInfo,

    /// The scrollback the conversation pane draws its tail from.
    lines: &'a [String],

    /// The draft in the input row.
    input: &'a str,

    /// Caret position in the draft, in characters.
    cursor: usize,

    /// Rows hidden below the conversation pane (`PageUp` moves it, `PageDown` undoes it).
    scroll: usize,

    /// What the pane reports back to the key loop.
    pane: &'a PaneScroll,

    /// The colours to draw with.
    palette: &'a Palette,
}

/// Draw a single frame of the full screen interface.
///
/// Layout: a status header on top, the friend list and the conversation log
/// side by side, and the input line with a key hint at the bottom.
#[cfg(feature = "terminal-ui")]
fn draw_interface<B: tui::backend::Backend>(frame: &mut tui::Frame<'_, B>, state: &FrameState<'_>) {
    use tui::layout::{Constraint, Direction, Layout};

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(frame.size());

    // `Layout::split` returns one `Rect` per constraint — three above, two below — so
    // these indices are in range by construction. The type system cannot say so, which
    // is why the count is stated here next to the constraints it comes from.
    draw_header(frame, chunks[0], state);

    // Body: contacts and peers on the left, conversation log on the right.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(20)])
        .split(chunks[1]);

    draw_sidebar(frame, body[0], state);
    draw_log(frame, body[1], state);
    draw_input(frame, chunks[2], state);
}

/// Header: identity and subsystem status on two lines
#[cfg(feature = "terminal-ui")]
fn draw_header<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    state: &FrameState<'_>,
) {
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let info = state.info;
    let palette = state.palette;
    let dim = palette.dim;
    // The configured `[app] name`, which the manager installs before the first
    // frame; `TuiInfo::default()` (a unit test, or a snapshot that has not been
    // refreshed yet) falls back to the product name.
    let title = if info.app_name.trim().is_empty() {
        " metaText ".to_string()
    } else {
        format!(" {} ", info.app_name.trim())
    };

    let header = Paragraph::new(vec![
        Spans::from(vec![
            Span::styled(format!(" {} ", info.nickname), palette.nickname),
            Span::raw(format!("  {}", info.status_message)),
            Span::raw(format!("  [{}]", info.mode)),
        ]),
        Spans::from(vec![
            Span::styled(format!(" id {} ", truncate_string(&info.identity, 24)), dim),
            Span::styled(format!(" enc {} ", info.encryption), dim),
            Span::styled(format!(" db {} ", info.database), dim),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(header, area);
}

/// Sidebar: contacts on top, live peers below
#[cfg(feature = "terminal-ui")]
fn draw_sidebar<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    state: &FrameState<'_>,
) {
    use tui::layout::{Constraint, Direction, Layout};
    use tui::text::Span;
    use tui::widgets::{Block, Borders, List, ListItem};

    let info = state.info;
    let palette = state.palette;

    let numbered = |names: &Vec<String>| -> Vec<ListItem<'static>> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ListItem::new(Span::raw(format!(" {}. {name}", index + 1))))
            .collect()
    };
    let placeholder = |text: &str| -> Vec<ListItem<'static>> {
        vec![ListItem::new(Span::styled(text.to_string(), palette.dim))]
    };

    let friends = if info.friends.is_empty() {
        placeholder(" (no friends yet)")
    } else {
        numbered(&info.friends)
    };

    let peer_rows = u16::try_from(info.peers.len() + 2).unwrap_or(3).clamp(3, 8);
    // Two constraints, so `split` returns exactly two rectangles: `sidebar[0]` and
    // `sidebar[1]` below are in range by construction.
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(peer_rows)])
        .split(area);

    let friend_base = info.active_chat.as_deref().map_or_else(
        || " friends ".to_string(),
        |chat| format!(" friends (chatting: {chat}) "),
    );
    let friend_hidden = info
        .friends
        .len()
        .saturating_sub(usize::from(sidebar[0].height.saturating_sub(2)));
    let friend_list = List::new(friends).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title_with_overflow(&friend_base, friend_hidden)),
    );
    frame.render_widget(friend_list, sidebar[0]);

    let peers = if info.peers.is_empty() {
        let text = if info.pending_peers > 0 {
            format!(" retrying {} address(es)…", info.pending_peers)
        } else {
            " (nobody connected)".to_string()
        };
        placeholder(&text)
    } else {
        numbered(&info.peers)
    };
    let peer_hidden = info
        .peers
        .len()
        .saturating_sub(usize::from(sidebar[1].height.saturating_sub(2)));
    let peer_list = List::new(peers).block(Block::default().borders(Borders::ALL).title(
        title_with_overflow(&format!(" peers ({}) ", info.peers.len()), peer_hidden),
    ));
    frame.render_widget(peer_list, sidebar[1]);
}

/// A list pane's title, marked when the pane cannot show every entry.
///
/// The widget clips what does not fit, and a clipped list is indistinguishable
/// from a short one - the friend list alone holds up to 1024 entries, so a
/// sidebar that hides a dozen of them has to say so.
#[cfg(feature = "terminal-ui")]
fn title_with_overflow(base: &str, hidden: usize) -> String {
    if hidden == 0 {
        return base.to_string();
    }
    format!("{} · +{hidden} more ", base.trim_end())
}

/// The styles the interface draws with, derived from [`UiOptions`].
///
/// Kept as one value rather than a colour per call site so that "no colours" and
/// "light theme" are single decisions: a style built inline somewhere would keep
/// its colour whatever the user asked for.
#[cfg(feature = "terminal-ui")]
#[derive(Debug, Clone, Copy)]
struct Palette {
    /// The nickname badge in the header.
    nickname: Style,

    /// Secondary text: the identity line, placeholders, hints.
    dim: Style,

    /// The input row's prompt.
    prompt: Style,
}
#[cfg(feature = "terminal-ui")]
impl Palette {
    /// Colours for a dark background (the default theme).
    fn dark() -> Self {
        use tui::style::{Color, Modifier, Style};

        Self {
            nickname: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            dim: Style::default().fg(Color::DarkGray),
            prompt: Style::default().fg(Color::Green),
        }
    }

    /// Colours for a light background, where dark grey is unreadable.
    fn light() -> Self {
        use tui::style::{Color, Modifier, Style};

        Self {
            nickname: Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            dim: Style::default().fg(Color::Gray),
            prompt: Style::default().fg(Color::Blue),
        }
    }

    /// No colour at all: every style is the terminal's own.
    fn plain() -> Self {
        Self {
            nickname: Style::default(),
            dim: Style::default(),
            prompt: Style::default(),
        }
    }

    /// The palette the options ask for.
    fn from_options(options: UiOptions) -> Self {
        if options.colors {
            match options.theme {
                Theme::Dark => Self::dark(),
                Theme::Light => Self::light(),
            }
        } else {
            Self::plain()
        }
    }
}

/// Conversation pane: the tail of the log, shifted by the scroll offset
#[cfg(feature = "terminal-ui")]
fn draw_log<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    state: &FrameState<'_>,
) {
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let info = state.info;
    let lines = state.lines;
    let scroll = state.scroll;
    let pane = state.pane;
    let palette = state.palette;

    // The pane's own columns: one border on each side is not text space.
    let width = usize::from(area.width.saturating_sub(2));
    let visible_height = usize::from(area.height.saturating_sub(2));
    let (rows, limit) = visible_log_rows(lines, width, visible_height, scroll);

    // Tell the key loop what a page is here and how far back the log goes, so
    // `PageUp` / `PageDown` and the mouse wheel move by the pane's height instead
    // of a fixed number of log lines, which is a different amount of movement once
    // lines wrap. The arrivals are for a pane that must not follow them.
    pane.page.store(visible_height.max(1), Ordering::Relaxed);
    pane.limit.store(limit, Ordering::Relaxed);
    pane.arrived
        .store(pane.count_arrivals(lines, width), Ordering::Relaxed);

    let shown: Vec<Spans<'_>> = rows
        .into_iter()
        .map(|row| Spans::from(Span::raw(row)))
        .collect();

    // No `Wrap` here on purpose: the rows are already wrapped, and a widget that
    // wraps a second time can overflow the pane and clip the newest rows again.
    let log = Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::styled(pane_title(info, scroll > 0), palette.dim)),
    ));

    frame.render_widget(log, area);
}

/// The conversation pane's title.
///
/// A pane that shows older rows looks exactly like a quiet conversation - it stops
/// following the newest entry as soon as it is scrolled - so the title says so, and
/// names the key that goes back to the bottom.
#[cfg(feature = "terminal-ui")]
fn pane_title(info: &TuiInfo, scrolled: bool) -> String {
    let following = if scrolled { "· scrolled (PgDn) " } else { "" };
    format!(
        " conversation · sent {} · recv {} {following}",
        info.messages_sent, info.messages_received
    )
}

/// Columns one character occupies when a terminal draws it.
///
/// The pane wraps its own rows and the input row places its caret by column, so
/// both need the measure the widget uses. `unicode-width` is that measure: it
/// counts an emoji as two columns and a combining mark or variation selector as
/// none, which keeps a row's columns and its characters from drifting apart.
#[cfg(feature = "terminal-ui")]
fn char_columns(character: char) -> usize {
    use unicode_width::UnicodeWidthChar;

    character.width().unwrap_or(0)
}

/// Split one log line into the rows a pane of `width` columns draws it on.
#[cfg(feature = "terminal-ui")]
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }

    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for character in line.chars() {
        let columns = char_columns(character);
        // A character that no longer fits starts the next row, which is what a
        // terminal does; without the check a two column glyph would be split.
        if used > 0 && used.saturating_add(columns) > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(character);
        used = used.saturating_add(columns);
    }

    // A line that fits - the empty line included - is still one row.
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// The rows the conversation pane shows, and how far it can be scrolled.
///
/// The window is taken in *display rows*, not in log lines. Selecting log lines
/// and leaving the wrapping to the widget is what made answers disappear: a
/// pane 24 rows high was handed 24 lines that needed 28 rows, and because a
/// paragraph is drawn from its top, the newest rows - the answer the user had
/// just asked for - fell outside the pane until the log was cleared.
///
/// The second value is the largest `scroll` that still shows log content, or
/// [`usize::MAX`] while that is not known yet: the walk stops as soon as it has
/// filled the window, so the pane only learns the bound once it has reached the
/// start of the log. Reporting the bound instead of the count keeps scrolling by
/// a page cheap on a long log.
#[cfg(feature = "terminal-ui")]
fn visible_log_rows(
    lines: &[String],
    width: usize,
    visible: usize,
    scroll: usize,
) -> (Vec<String>, usize) {
    if visible == 0 {
        return (Vec::new(), 0);
    }

    // Rows are collected newest first and stop once the window and the rows
    // scrolled past below it are covered; `wanted` is the most that can be needed.
    let wanted = visible.saturating_add(scroll);
    let mut rows: VecDeque<String> = VecDeque::new();
    let mut reached_the_start = true;
    for line in lines.iter().rev() {
        for row in wrap_line(line, width).into_iter().rev() {
            rows.push_front(row);
        }
        if rows.len() >= wanted {
            reached_the_start = false;
            break;
        }
    }

    let limit = if reached_the_start {
        rows.len().saturating_sub(visible)
    } else {
        usize::MAX
    };

    // Scrolling past the start shows the oldest rows instead of an empty pane:
    // the offset outlives the rows it counted.
    let scroll = scroll.min(limit);
    let end = rows.len().saturating_sub(scroll);
    let start = end.saturating_sub(visible);
    let take = end.saturating_sub(start);
    let shown: Vec<String> = rows.into_iter().skip(start).take(take).collect();
    (shown, limit)
}

/// The part of the draft the input row shows, and the caret's column in it.
///
/// The row is one line high, so a draft longer than the pane has to scroll: the
/// window keeps the caret inside and a leading ellipsis marks the text that is
/// off to the left. Columns are display columns rather than characters, so a
/// draft with emoji or CJK keeps the caret where the next character will go.
#[cfg(feature = "terminal-ui")]
fn input_view(input: &str, cursor: usize, width: usize) -> (String, u16) {
    if width == 0 {
        return (String::new(), 0);
    }

    let characters: Vec<char> = input.chars().collect();
    let cursor = cursor.min(characters.len());

    // Display columns before each character, so the caret can be measured.
    let mut prefix: Vec<usize> = Vec::with_capacity(characters.len() + 1);
    let mut used = 0usize;
    prefix.push(0);
    for character in &characters {
        used = used.saturating_add(char_columns(*character));
        prefix.push(used);
    }

    // Drop leading characters only as far as the caret requires, and count the
    // ellipsis as the column it takes: the caret must stay inside the row.
    let caret = prefix[cursor];
    let mut start = 0usize;
    while start < cursor {
        let ellipsis = usize::from(start > 0);
        if caret.saturating_sub(prefix[start]).saturating_add(ellipsis) < width {
            break;
        }
        start += 1;
    }

    let mut offset = caret.saturating_sub(prefix[start]);
    let mut text: String = characters[start..].iter().collect();
    if start > 0 {
        offset = offset.saturating_add(1);
        text.insert(0, '…');
    }

    // `offset` is below `width` by construction; the conversion is still checked
    // rather than infallible, as everywhere in this crate.
    (text, u16::try_from(offset).unwrap_or(u16::MAX))
}

/// Footer: the input line plus a short key hint in the block title
#[cfg(feature = "terminal-ui")]
fn draw_input<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    state: &FrameState<'_>,
) {
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let input = state.input;
    let cursor = state.cursor;
    let palette = state.palette;

    // One border on each side plus the two column `> ` prefix leave these columns
    // for the draft itself.
    let text_width = usize::from(area.width.saturating_sub(4));
    let (draft, caret) = input_view(input, cursor, text_width);
    let footer = Paragraph::new(Spans::from(vec![
        Span::styled("> ", palette.prompt),
        Span::raw(draft),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(Spans::from(Span::styled(
                " Enter send | Up/Down history | Ctrl+U clear | Ctrl+C/Q quit | PgUp/PgDn scroll ",
                palette.dim,
            ))),
    );

    frame.render_widget(footer, area);

    // Park the terminal cursor where the next character will be inserted so the
    // user can see the caret while editing. The text starts after the left border
    // and the `> ` prefix (three columns), and `input_view` reports the caret in
    // display columns within the visible part of the draft.
    if area.height > 2 && area.width > 5 {
        let x = area.x.saturating_add(3).saturating_add(caret);
        frame.set_cursor(x, area.y + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane title says when the pane has stopped following the newest entry:
    /// older rows look exactly like a quiet conversation otherwise.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_pane_title_marks_a_scrolled_pane() {
        let info = TuiInfo {
            messages_sent: 3,
            messages_received: 4,
            ..TuiInfo::default()
        };
        assert_eq!(pane_title(&info, false), " conversation · sent 3 · recv 4 ");

        let scrolled = pane_title(&info, true);
        assert!(scrolled.contains("scrolled (PgDn)"), "{scrolled}");
        assert!(
            scrolled.contains("sent 3") && scrolled.contains("recv 4"),
            "{scrolled}"
        );
        assert!(
            scrolled.ends_with(' '),
            "the title keeps its trailing space"
        );
    }

    /// A pasted paragraph stays one line: line breaks become spaces and control
    /// characters are dropped, so nothing enters the draft that the row cannot
    /// draw or the user cannot see to edit.
    #[test]
    fn test_paste_stays_on_one_line() {
        let mut input = InputBuffer::new();
        input.insert_str("first line\nsecond line\r\nthird\tcolumn\u{7}");
        assert_eq!(input.text(), "first line second line  third column");
    }

    /// A line is split into rows of the pane's width, and a row never exceeds it.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_wrap_line_fits_the_pane() {
        assert_eq!(wrap_line("hello", 10), vec!["hello"]);
        assert_eq!(wrap_line("", 10), vec![""]);

        let rows = wrap_line(&"ab".repeat(7), 5);
        assert_eq!(rows, vec!["ababa", "babab", "abab"]);
        for row in &rows {
            assert!(
                row.chars().count() <= 5,
                "row {row:?} is wider than the pane"
            );
        }
    }

    /// A wide character moves to the next row instead of being split, and it
    /// costs two columns there - the same accounting the terminal does.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_wrap_line_counts_display_columns() {
        // Two columns for the emoji, one for its variation selector: four
        // columns of text do not fit three, so the emoji moves down.
        let rows = wrap_line("ab\u{1f517}\u{fe0f}", 3);
        assert_eq!(rows, vec!["ab", "\u{1f517}\u{fe0f}"]);
    }

    /// The pane window is measured in display rows, so a log whose lines wrap
    /// still ends with the newest entry. Taking the window in log lines handed
    /// the widget more rows than it could draw and clipped exactly those.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_visible_log_rows_keeps_the_newest_row() {
        let mut lines: Vec<String> = (0..20).map(|index| format!("line {index}")).collect();
        // The line before the newest one needs four rows in a ten column pane.
        lines.push("x".repeat(31));
        lines.push("the newest answer".to_string());

        let (rows, limit) = visible_log_rows(&lines, 10, 6, 0);
        assert_eq!(rows.len(), 6);
        assert_eq!(limit, usize::MAX, "the walk filled the pane, not the log");
        // The newest entry is at the bottom, even though its own line wraps into
        // two rows: before the window was measured in rows it was clipped away.
        let drawn = rows.concat();
        assert!(
            drawn.ends_with("the newest answer"),
            "the newest answer must be drawn, found {drawn:?}"
        );
        for row in &rows {
            assert!(
                row.chars().count() <= 10,
                "row {row:?} is wider than the pane"
            );
        }
    }

    /// Scrolling walks into older rows and stops at the start of the log, which is
    /// where the pane learns how far back it can go.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_visible_log_rows_scrolls_back() {
        let lines: Vec<String> = (0..10).map(|index| format!("line {index}")).collect();

        let (newest, _) = visible_log_rows(&lines, 20, 3, 0);
        assert_eq!(newest, vec!["line 7", "line 8", "line 9"]);

        let (scrolled, _) = visible_log_rows(&lines, 20, 3, 4);
        assert_eq!(scrolled, vec!["line 3", "line 4", "line 5"]);

        // Scrolling past the start shows the oldest rows rather than nothing: the
        // offset can outlive the rows it counted, and the bound comes back with it.
        let (past_the_top, limit) = visible_log_rows(&lines, 20, 3, 99);
        assert_eq!(past_the_top, vec!["line 0", "line 1", "line 2"]);
        assert_eq!(limit, 7, "ten rows of content, three of them visible");

        // An empty log has nothing to show, whatever the offset says.
        let (empty, limit) = visible_log_rows(&[], 20, 3, 0);
        assert!(empty.is_empty());
        assert_eq!(limit, 0);
    }

    /// The offset counts display rows, not log lines: one `PageUp` moves by the
    /// pane's height, and must not jump further just because a line wraps.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_visible_log_rows_scrolls_by_rows() {
        // The first line is one row, the second needs two in a six column pane.
        let lines = vec!["abcdef".to_string(), "abcdefghijkl".to_string()];

        let (rows, _) = visible_log_rows(&lines, 6, 2, 0);
        assert_eq!(rows, vec!["abcdef", "ghijkl"]);

        // One row back shows the row above: the window now ends with the first
        // half of the second line.
        let (rows, _) = visible_log_rows(&lines, 6, 2, 1);
        assert_eq!(rows, vec!["abcdef", "abcdef"]);
    }

    /// A list pane says how many entries it cannot show: the widget clips what does
    /// not fit, and a clipped list looks exactly like a short one.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_title_with_overflow() {
        assert_eq!(title_with_overflow(" friends ", 0), " friends ");
        assert_eq!(
            title_with_overflow(" friends (chatting: Bob) ", 12),
            " friends (chatting: Bob) · +12 more "
        );
        assert_eq!(
            title_with_overflow(" peers (3) ", 1),
            " peers (3) · +1 more "
        );
    }

    /// The input row shows the draft, and scrolls a long one so the caret stays
    /// visible: a chat draft is far wider than the pane.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_input_view_keeps_the_caret_visible() {
        // A short draft is shown as typed, with the caret at its offset.
        let (text, caret) = input_view("hello", 5, 20);
        assert_eq!(text, "hello");
        assert_eq!(caret, 5);

        // Editing in the middle keeps the caret where it is.
        let (text, caret) = input_view("hello", 2, 20);
        assert_eq!(text, "hello");
        assert_eq!(caret, 2);

        // A long draft scrolls: the tail is shown, marked with an ellipsis, and
        // the caret lands inside the row.
        let draft = "a".repeat(30);
        let (text, caret) = input_view(&draft, 30, 10);
        assert!(text.starts_with('…'));
        assert_eq!(text.chars().filter(|c| *c == 'a').count(), 8);
        assert_eq!(caret, 9);
        assert!(caret < 10, "caret {caret} is outside a ten column row");
    }

    /// The caret is placed by display column, so emoji and CJK do not push it
    /// away from the character it is next to.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_input_view_counts_columns_not_characters() {
        // "日本語" is six columns wide, so the caret after it is at column six.
        let (_, caret) = input_view("日本語", 3, 20);
        assert_eq!(caret, 6);

        // Seven columns of CJK do not fit six, so the row scrolls.
        let (text, caret) = input_view("日本語", 3, 6);
        assert!(text.starts_with('…'));
        assert!(caret < 6);
    }

    #[tokio::test]
    async fn test_tui_manager_creation() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<TuiInput>();

        let manager = TuiManager::new("config", UiOptions::default(), event_sender)
            .await
            .unwrap();
        assert!(!manager.is_started());
    }

    #[tokio::test]
    async fn test_tui_startup_shutdown() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<TuiInput>();

        let mut manager = TuiManager::new("config", UiOptions::default(), event_sender)
            .await
            .unwrap();

        // Start TUI (without taking over the terminal in tests)
        manager.start(false).await.unwrap();
        assert!(manager.is_started());

        // Shutdown TUI
        manager.shutdown().await.unwrap();
        assert!(!manager.is_started());
    }

    /// The output sink captures lines in order and clears on demand
    #[test]
    fn test_output_sink_captures_lines() {
        let sink = OutputSink::default();
        assert!(!sink.is_enabled());

        sink.push("first");
        assert_eq!(sink.snapshot(), vec!["first".to_string()]);

        sink.enable();
        assert!(sink.is_enabled());
        sink.push("second");
        assert_eq!(
            sink.snapshot(),
            vec!["first".to_string(), "second".to_string()]
        );

        sink.clear();
        assert!(sink.snapshot().is_empty());

        sink.disable();
        assert!(!sink.is_enabled());
    }

    /// The sink never grows past its capacity
    #[test]
    fn test_output_sink_evicts_oldest_line() {
        let sink = OutputSink::default();
        for index in 0..(OUTPUT_CAPACITY + 5) {
            sink.push(index.to_string());
        }

        let lines = sink.snapshot();
        assert_eq!(lines.len(), OUTPUT_CAPACITY);
        assert_eq!(lines[0], "5");
        assert_eq!(
            lines[OUTPUT_CAPACITY - 1],
            (OUTPUT_CAPACITY + 4).to_string()
        );
    }

    /// The rendered status snapshot starts empty
    #[test]
    fn test_tui_info_default() {
        let info = TuiInfo::default();
        assert!(info.nickname.is_empty());
        assert!(info.friends.is_empty());
        assert!(info.active_chat.is_none());
        assert_eq!(info.messages_sent, 0);
        assert_eq!(info.bytes_received, 0);
    }

    /// The info handle shares state with the manager
    #[tokio::test]
    async fn test_tui_info_handle_is_shared() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<TuiInput>();
        let manager = TuiManager::new("config", UiOptions::default(), event_sender)
            .await
            .unwrap();

        manager.info_handle().lock().unwrap().nickname = "Alice".to_string();
        assert_eq!(manager.info_handle().lock().unwrap().nickname, "Alice");

        // Without a TTY the interface never becomes active.
        assert!(!manager.is_active());
    }

    /// The renderer draws the header, friends list, log and input line
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_draw_interface_renders_state() {
        use tui::backend::TestBackend;
        use tui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();

        let info = TuiInfo {
            app_name: "metaText".to_string(),
            nickname: "Alice".to_string(),
            status_message: "keep on metaverse".to_string(),
            identity: "ABCDEF0123456789".to_string(),
            mode: "TUI".to_string(),
            network: "running".to_string(),
            database: "connected".to_string(),
            encryption: "enabled".to_string(),
            friends: vec!["Bob".to_string()],
            peers: vec!["carol".to_string()],
            pending_peers: 1,
            active_chat: Some("Bob".to_string()),
            messages_sent: 2,
            messages_received: 1,
            bytes_sent: 42,
            bytes_received: 7,
        };
        let lines = vec!["hello world".to_string()];

        terminal
            .draw(|frame| {
                let state = FrameState {
                    info: &info,
                    lines: &lines,
                    input: "/help",
                    cursor: 5,
                    scroll: 0,
                    pane: &PaneScroll::default(),
                    palette: &Palette::dark(),
                };
                draw_interface(frame, &state);
            })
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();

        assert!(rendered.contains("Alice"), "nickname missing: {rendered}");
        assert!(rendered.contains("TUI"), "mode missing: {rendered}");
        assert!(rendered.contains("Bob"), "friend missing: {rendered}");
        assert!(rendered.contains("carol"), "peer missing: {rendered}");
        assert!(
            rendered.contains("peers (1)"),
            "peer title missing: {rendered}"
        );
        assert!(
            rendered.contains("hello world"),
            "log line missing: {rendered}"
        );
        assert!(rendered.contains("/help"), "input missing: {rendered}");
        assert!(rendered.contains("sent 2"), "counter missing: {rendered}");
    }

    /// The prompt is shown only for a plain REPL on a real terminal
    #[test]
    fn test_should_show_prompt_policy() {
        // A plain REPL on a real terminal gets the prompt.
        assert!(should_show_prompt(true, true, false));

        // Piped stdin (scripted input) suppresses it.
        assert!(!should_show_prompt(false, true, false));

        // Piped stdout (redirected output) suppresses it.
        assert!(!should_show_prompt(true, false, false));
        assert!(!should_show_prompt(false, false, false));

        // The full screen interface owns its own input line.
        assert!(!should_show_prompt(true, true, true));
    }

    /// Typing, cursor movement and mid-line editing behave like a line editor
    #[test]
    fn test_input_buffer_edits_at_cursor() {
        let mut input = InputBuffer::new();
        input.insert_str("hlo");
        assert_eq!(input.text(), "hlo");
        assert_eq!(input.cursor(), 3);

        // Move between `h` and `l` and insert the missing `e`.
        input.move_left();
        input.move_left();
        assert_eq!(input.cursor(), 1);
        input.insert('e');
        assert_eq!(input.text(), "helo");
        assert_eq!(input.cursor(), 2);

        // Insert the missing `l`, then remove it again with Backspace.
        input.move_end();
        assert_eq!(input.cursor(), 4);
        input.move_left();
        input.insert('l');
        assert_eq!(input.text(), "hello");
        input.backspace();
        assert_eq!(input.text(), "helo");

        // Delete removes the character under the cursor.
        input.move_home();
        input.delete();
        assert_eq!(input.text(), "elo");
        assert_eq!(input.cursor(), 0);

        // Home/End clamp instead of panicking.
        input.move_left();
        assert_eq!(input.cursor(), 0);
        input.move_end();
        input.move_right();
        assert_eq!(input.cursor(), 3);

        // Ctrl+U empties the line.
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }

    /// Multi-byte characters count as one cursor step
    #[test]
    fn test_input_buffer_handles_multibyte_characters() {
        let mut input = InputBuffer::new();
        input.insert_str("héllo");
        assert_eq!(input.text(), "héllo");
        assert_eq!(input.cursor(), 5);

        input.move_left();
        input.backspace();
        assert_eq!(input.text(), "hélo");
        assert_eq!(input.cursor(), 3);
    }

    /// Submitting stores history once, in order, and collapses duplicates
    #[test]
    fn test_input_buffer_history_recording() {
        let mut input = InputBuffer::new();

        input.insert_str("/help");
        assert_eq!(input.submit(), "/help");
        assert!(input.is_empty());

        input.insert_str("/help");
        assert_eq!(input.submit(), "/help");

        // Empty input is returned but not recorded.
        assert_eq!(input.submit(), String::new());

        input.insert_str("/quit");
        assert_eq!(input.submit(), "/quit");

        assert_eq!(input.history(), &["/help".to_string(), "/quit".to_string()]);
    }

    /// Up/Down walk the history and restore the half-typed draft
    #[test]
    fn test_input_buffer_history_navigation() {
        let mut input = InputBuffer::new();
        for line in ["first", "second", "third"] {
            input.insert_str(line);
            input.submit();
        }

        // Start a fresh line, then recall backwards.
        input.insert_str("draft");
        input.history_previous();
        assert_eq!(input.text(), "third");
        input.history_previous();
        assert_eq!(input.text(), "second");
        input.history_previous();
        assert_eq!(input.text(), "first");
        // Walking back past the oldest entry stays there.
        input.history_previous();
        assert_eq!(input.text(), "first");

        // Walking forward returns through the list to the stashed draft.
        input.history_next();
        assert_eq!(input.text(), "second");
        input.history_next();
        assert_eq!(input.text(), "third");
        input.history_next();
        assert_eq!(input.text(), "draft");

        // Editing leaves browsing mode, so Down has nothing to recall.
        input.history_previous();
        input.insert('!');
        assert_eq!(input.text(), "third!");
        input.history_next();
        assert_eq!(input.text(), "third!");
    }

    /// History is bounded so a long session cannot grow without limit
    #[test]
    fn test_input_buffer_history_is_bounded() {
        let mut input = InputBuffer::new();
        for index in 0..(INPUT_HISTORY_CAPACITY + 10) {
            input.insert_str(&format!("line-{index}"));
            input.submit();
        }

        assert_eq!(input.history().len(), INPUT_HISTORY_CAPACITY);
        // The oldest entries were evicted, the newest one is present.
        assert_eq!(input.history().first().unwrap(), "line-10");
        assert_eq!(
            input.history().last().unwrap(),
            &format!("line-{}", INPUT_HISTORY_CAPACITY + 9)
        );
    }

    /// The key mapping turns typing into a submitted line and drives history.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_apply_key_typing_and_submit() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut input = InputBuffer::new();
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        for character in ['/', 'h', 'i'] {
            assert_eq!(
                apply_key(&mut input, plain(KeyCode::Char(character))),
                KeyAction::None
            );
        }
        assert_eq!(input.text(), "/hi");

        assert_eq!(
            apply_key(&mut input, plain(KeyCode::Enter)),
            KeyAction::Line("/hi".to_string())
        );
        assert!(input.is_empty());
    }

    /// Control keys edit the line and can request quit or a screen clear.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_apply_key_control_actions() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        let mut input = InputBuffer::new();
        input.insert_str("abc");

        // Ctrl+U clears the draft instead of inserting a `u`.
        assert_eq!(
            apply_key(&mut input, ctrl(KeyCode::Char('u'))),
            KeyAction::None
        );
        assert!(input.is_empty());

        // Ctrl+L asks for a scrollback clear, Ctrl+C / Ctrl+Q to quit.
        assert_eq!(
            apply_key(&mut input, ctrl(KeyCode::Char('l'))),
            KeyAction::ClearOutput
        );
        assert_eq!(
            apply_key(&mut input, ctrl(KeyCode::Char('c'))),
            KeyAction::Quit
        );
        assert_eq!(
            apply_key(&mut input, ctrl(KeyCode::Char('q'))),
            KeyAction::Quit
        );

        // Page keys map to scrolling.
        assert_eq!(
            apply_key(
                &mut input,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
            ),
            KeyAction::ScrollUp
        );
        assert_eq!(
            apply_key(
                &mut input,
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
            ),
            KeyAction::ScrollDown
        );

        // Releasing a key does nothing.
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(apply_key(&mut input, release), KeyAction::None);
    }

    /// Up recalls the previous submitted line through the key mapping.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_apply_key_history_recall() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut input = InputBuffer::new();

        input.insert_str("/info");
        assert_eq!(
            apply_key(&mut input, key(KeyCode::Enter)),
            KeyAction::Line("/info".to_string())
        );

        assert_eq!(apply_key(&mut input, key(KeyCode::Up)), KeyAction::None);
        assert_eq!(input.text(), "/info");
        assert_eq!(apply_key(&mut input, key(KeyCode::Down)), KeyAction::None);
        assert!(input.is_empty());
    }

    /// `[ui] enable_colors` and `[ui] theme` pick one palette, and no colours means
    /// no colours: a style built inline somewhere would keep its colour whatever
    /// the user asked for.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_palette_follows_the_options() {
        use tui::style::{Color, Style};

        let dark = Palette::from_options(UiOptions::default());
        assert_eq!(dark.nickname.bg, Some(Color::Cyan));
        assert_eq!(dark.dim.fg, Some(Color::DarkGray));

        let light = Palette::from_options(UiOptions {
            theme: Theme::Light,
            ..UiOptions::default()
        });
        assert_eq!(light.nickname.bg, Some(Color::Blue));
        assert_eq!(
            light.dim.fg,
            Some(Color::Gray),
            "dark grey is unreadable on a light background"
        );

        let plain = Palette::from_options(UiOptions {
            colors: false,
            ..UiOptions::default()
        });
        for style in [plain.nickname, plain.dim, plain.prompt] {
            assert_eq!(style, Style::default(), "no colour was asked for");
        }
    }

    /// A page move is the pane's height in rows, never further than the log goes.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_pane_scroll_step_is_a_page_clamped_to_the_bound() {
        let pane = PaneScroll::default();
        pane.page.store(5, Ordering::Relaxed);

        pane.limit.store(usize::MAX, Ordering::Relaxed);
        assert_eq!(pane.step(0, true), 5, "one page towards older rows");
        assert_eq!(pane.step(5, false), 0, "one page back to the newest row");
        assert_eq!(pane.step(0, false), 0, "the newest row is the floor");

        // A log with seven rows behind the window cannot be scrolled past them.
        pane.limit.store(7, Ordering::Relaxed);
        assert_eq!(pane.step(5, true), 7);
        assert_eq!(pane.step(7, true), 7);
    }

    /// A pane that does not auto-scroll counts the rows output added, because that
    /// is exactly how far the view has to move to stay where it was.
    #[cfg(feature = "terminal-ui")]
    #[test]
    fn test_arrivals_are_counted_in_rows() {
        let pane = PaneScroll::default();
        // The first frame has nothing to compare against: the log it draws was
        // already there, so none of it "arrived" while the user was reading.
        let first = vec!["one".to_string(), "two".to_string()];
        assert_eq!(
            pane.count_arrivals(&first, 20),
            0,
            "the first frame is not new"
        );

        let long = "a line that needs several rows in a narrow pane".to_string();
        let grown = vec!["one".to_string(), "two".to_string(), long.clone()];
        assert_eq!(
            pane.count_arrivals(&grown, 8),
            wrap_line(&long, 8).len(),
            "the new line is counted in the rows it takes, not as one line"
        );
        assert!(wrap_line(&long, 8).len() > 1, "the line has to wrap");

        assert_eq!(pane.count_arrivals(&grown, 8), 0, "nothing arrived since");
        assert_eq!(pane.count_arrivals(&[], 8), 0, "a cleared log adds nothing");
    }
}
