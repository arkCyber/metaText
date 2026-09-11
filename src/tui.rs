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
use crate::types::AppEvent;

#[cfg(feature = "terminal-ui")]
use crate::error::MetaTextError;
#[cfg(feature = "terminal-ui")]
use crate::utils::truncate_string;
#[cfg(feature = "terminal-ui")]
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of console lines kept for the TUI scrollback.
pub const OUTPUT_CAPACITY: usize = 1000;

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
    event_sender: tokio::sync::mpsc::UnboundedSender<AppEvent>,

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
        event_sender: tokio::sync::mpsc::UnboundedSender<AppEvent>,
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
    /// use this sender to forward [`AppEvent::UserInput`] messages.
    ///
    /// # Returns
    ///
    /// Returns a cloned handle to the unbounded application event sender.
    #[must_use]
    pub fn event_sender(&self) -> tokio::sync::mpsc::UnboundedSender<AppEvent> {
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
    sender: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    info: &SharedTuiInfo,
    active: &AtomicBool,
) -> Result<(), MetaTextError> {
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
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    Ok(())
}

/// Enter raw mode and the alternate screen.
#[cfg(feature = "terminal-ui")]
fn setup_terminal(
) -> std::io::Result<tui::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>> {
    use crossterm::execute;
    use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    use tui::backend::CrosstermBackend;
    use tui::Terminal;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

/// Poll for keyboard input and redraw the interface until asked to stop.
#[cfg(feature = "terminal-ui")]
fn render_loop(
    terminal: &mut tui::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>,
    sender: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    info: &SharedTuiInfo,
    active: &AtomicBool,
) {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::time::Duration;

    let mut input = String::new();
    let mut scroll: usize = 0;

    while active.load(Ordering::SeqCst) {
        let lines = output_sink().snapshot();
        let snapshot = info.lock().map(|guard| guard.clone()).unwrap_or_default();

        let _ = terminal.draw(|frame| draw_interface(frame, &snapshot, &lines, &input, scroll));

        if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }

        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('c' | 'q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = sender.send(AppEvent::Shutdown);
                break;
            }
            KeyCode::Char(character) => input.push(character),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => input.clear(),
            KeyCode::Enter => {
                let line = std::mem::take(&mut input);
                scroll = 0;
                if sender.send(AppEvent::UserInput(line)).is_err() {
                    break;
                }
            }
            KeyCode::PageUp => {
                scroll = (scroll + 5).min(lines.len().saturating_sub(1));
            }
            KeyCode::PageDown => scroll = scroll.saturating_sub(5),
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
    draw_input(frame, chunks[2], input);
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
) {
    use tui::style::{Color, Style};
    use tui::text::{Span, Spans};
    use tui::widgets::{Block, Borders, Paragraph};

    let footer = Paragraph::new(Spans::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green)),
        Span::raw(input),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Enter to send | Ctrl+C/Q quit | PgUp/PgDn scroll "),
    );

    frame.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tui_manager_creation() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

        let manager = TuiManager::new("config", event_sender).await.unwrap();
        assert!(!manager.is_started());
    }

    #[tokio::test]
    async fn test_tui_startup_shutdown() {
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

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
        let (event_sender, _event_receiver) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
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
            .draw(|frame| draw_interface(frame, &info, &lines, "/help", 0))
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
}
