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
use std::sync::atomic::{AtomicBool, Ordering};

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

    /// Insert a whole string at the cursor (used for pasted input).
    pub fn insert_str(&mut self, text: &str) {
        for character in text.chars() {
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

impl TuiManager {
    /// Create a new TUI manager
    ///
    /// # Errors
    ///
    /// Currently this never fails; the `Result` is kept so that future UI
    /// backends can validate the terminal before the manager is constructed.
    pub async fn new(
        _config: &str,
        event_sender: tokio::sync::mpsc::UnboundedSender<TuiInput>,
    ) -> MetaTextResult<Self> {
        let timestamp = chrono::Utc::now();
        info!(
            "📱 [{}] Initializing TUI manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        let manager = Self {
            event_sender,
            started: false,
            info: Arc::new(Mutex::new(TuiInfo::default())),
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
    /// Returns [`MetaTextError::UserInterface`] when the terminal cannot be
    /// put into raw mode or the alternate screen cannot be entered.
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
    pub async fn display_banner(&self) {
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

        self.task = Some(tokio::task::spawn_blocking(move || {
            if let Err(error) = run_interface(&sender, &info, &active) {
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
) -> Result<(), MetaTextError> {
    use crossterm::event::DisableBracketedPaste;
    use crossterm::execute;
    use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

    let mut terminal = setup_terminal().map_err(|error| MetaTextError::UserInterface {
        message: format!("Failed to initialise the terminal interface: {error}"),
        component: "Tui".to_string(),
        source: Some(Box::new(error)),
    })?;

    render_loop(&mut terminal, sender, info, active);

    // Restore the terminal no matter how the loop ended.
    let _ = terminal.show_cursor();
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    );
    Ok(())
}

/// Enter raw mode and the alternate screen.
#[cfg(feature = "terminal-ui")]
fn setup_terminal(
) -> std::io::Result<tui::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>> {
    use crossterm::event::EnableBracketedPaste;
    use crossterm::execute;
    use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    use tui::backend::CrosstermBackend;
    use tui::Terminal;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
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
) {
    use crossterm::event::{self, Event};
    use std::time::Duration;

    let mut input = InputBuffer::new();
    let mut scroll: usize = 0;

    while active.load(Ordering::SeqCst) {
        let lines = output_sink().snapshot();
        let snapshot = info.lock().map(|guard| guard.clone()).unwrap_or_default();
        let draft = input.text();

        let _ = terminal.draw(|frame| {
            draw_interface(frame, &snapshot, &lines, &draft, input.cursor(), scroll);
        });

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
                KeyAction::ScrollUp => {
                    scroll = (scroll + 5).min(lines.len().saturating_sub(1));
                }
                KeyAction::ScrollDown => scroll = scroll.saturating_sub(5),
            },
            // Bracketed paste arrives as one event instead of a key storm.
            Ok(Event::Paste(text)) => input.insert_str(&text),
            _ => {}
        }
    }
}

/// Draw a single frame of the full screen interface.
///
/// Layout: a status header on top, the friend list and the conversation log
/// side by side, and the input line with a key hint at the bottom.
#[cfg(feature = "terminal-ui")]
fn draw_interface<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    info: &TuiInfo,
    lines: &[String],
    input: &str,
    cursor: usize,
    scroll: usize,
) {
    use tui::layout::{Constraint, Direction, Layout};

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(frame.size());

    draw_header(frame, chunks[0], info);

    // Body: contacts and peers on the left, conversation log on the right.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(20)])
        .split(chunks[1]);

    draw_sidebar(frame, body[0], info);
    draw_log(frame, body[1], info, lines, scroll);
    draw_input(frame, chunks[2], input, cursor);
}

/// Header: identity and subsystem status on two lines
#[cfg(feature = "terminal-ui")]
fn draw_header<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    info: &TuiInfo,
) {
    use tui::style::{Color, Modifier, Style};
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let dim = Style::default().fg(Color::DarkGray);

    let header = Paragraph::new(vec![
        Spans::from(vec![
            Span::styled(
                format!(" {} ", info.nickname),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {}", info.status_message)),
            Span::raw(format!("  [{}]", info.mode)),
        ]),
        Spans::from(vec![
            Span::styled(format!(" id {} ", truncate_string(&info.identity, 24)), dim),
            Span::styled(format!(" enc {} ", info.encryption), dim),
            Span::styled(format!(" db {} ", info.database), dim),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title(" metaText "));

    frame.render_widget(header, area);
}

/// Sidebar: contacts on top, live peers below
#[cfg(feature = "terminal-ui")]
fn draw_sidebar<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    info: &TuiInfo,
) {
    use tui::layout::{Constraint, Direction, Layout};
    use tui::style::{Color, Style};
    use tui::text::Span;
    use tui::widgets::{Block, Borders, List, ListItem};

    let numbered = |names: &Vec<String>| -> Vec<ListItem<'static>> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ListItem::new(Span::raw(format!(" {}. {name}", index + 1))))
            .collect()
    };
    let placeholder = |text: &str| -> Vec<ListItem<'static>> {
        vec![ListItem::new(Span::styled(
            text.to_string(),
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let friends = if info.friends.is_empty() {
        placeholder(" (no friends yet)")
    } else {
        numbered(&info.friends)
    };
    let friend_title = info.active_chat.as_ref().map_or_else(
        || " friends ".to_string(),
        |chat| format!(" friends (chatting: {chat}) "),
    );
    let friend_list =
        List::new(friends).block(Block::default().borders(Borders::ALL).title(friend_title));

    let peer_rows = u16::try_from(info.peers.len() + 2).unwrap_or(3).clamp(3, 8);
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(peer_rows)])
        .split(area);

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
    let peer_list = List::new(peers).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" peers ({}) ", info.peers.len())),
    );
    frame.render_widget(peer_list, sidebar[1]);
}

/// Conversation pane: the tail of the log, shifted by the scroll offset
#[cfg(feature = "terminal-ui")]
fn draw_log<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    info: &TuiInfo,
    lines: &[String],
    scroll: usize,
) {
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph, Wrap};

    let visible_height = usize::from(area.height.saturating_sub(2));
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(visible_height);
    let shown: Vec<Spans<'_>> = lines[start..end]
        .iter()
        .map(|line| Spans::from(Span::raw(line.clone())))
        .collect();

    let log = Paragraph::new(shown)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " conversation · sent {} · recv {} ",
            info.messages_sent, info.messages_received
        )))
        .wrap(Wrap { trim: false });

    frame.render_widget(log, area);
}

/// Footer: the input line plus a short key hint in the block title
#[cfg(feature = "terminal-ui")]
fn draw_input<B: tui::backend::Backend>(
    frame: &mut tui::Frame<'_, B>,
    area: tui::layout::Rect,
    input: &str,
    cursor: usize,
) {
    use tui::style::{Color, Style};
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let footer =
        Paragraph::new(Spans::from(vec![
            Span::styled("> ", Style::default().fg(Color::Green)),
            Span::raw(input),
        ]))
        .block(Block::default().borders(Borders::ALL).title(
            " Enter send | Up/Down history | Ctrl+U clear | Ctrl+C/Q quit | PgUp/PgDn scroll ",
        ));

    frame.render_widget(footer, area);

    // Park the terminal cursor where the next character will be inserted so the
    // user can see the caret while editing. The text starts after the left
    // border and the `> ` prefix (three columns); the cursor is a character
    // offset, which matches the widget's monospace assumption.
    if area.height > 2 && area.width > 5 {
        let offset = u16::try_from(cursor).unwrap_or(u16::MAX);
        let x = area.x.saturating_add(3).saturating_add(offset);
        let last_column = area.x + area.width - 2;
        frame.set_cursor(x.min(last_column), area.y + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tui_manager_creation() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<TuiInput>();

        let manager = TuiManager::new("config", event_sender).await.unwrap();
        assert!(!manager.is_started());
    }

    #[tokio::test]
    async fn test_tui_startup_shutdown() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<TuiInput>();

        let mut manager = TuiManager::new("config", event_sender).await.unwrap();

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
        let manager = TuiManager::new("config", event_sender).await.unwrap();

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
            .draw(|frame| draw_interface(frame, &info, &lines, "/help", 5, 0))
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
}
