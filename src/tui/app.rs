//! TUI application — App struct, event loop, layout skeleton.
//! Panel content rendering is in session_list.rs, detail.rs, status_bar.rs (Task 12).

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Paragraph},
    Frame, Terminal,
};
use std::io;
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::ipc::{connect_to_daemon, read_message, send_message, ClientMessage, DaemonMessage};
use crate::types::{HostStatus, SessionInfo, SessionState};

/// Transient status message shown in status bar.
struct StatusMsg {
    text: String,
    expires: Instant,
}

/// Main TUI application state.
pub struct App {
    /// Sessions received from daemon.
    pub sessions: Vec<SessionInfo>,
    /// Host statuses from daemon.
    pub hosts: Vec<HostStatus>,
    /// Currently selected index in session list.
    pub selected_index: usize,
    /// Whether the TUI should quit.
    pub should_quit: bool,
    /// Whether we're connected to the daemon.
    pub daemon_connected: bool,
    /// Channel to send messages to daemon.
    daemon_tx: Option<mpsc::Sender<ClientMessage>>,
    /// Transient status message.
    status_msg: Option<StatusMsg>,
}

impl App {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            hosts: Vec::new(),
            selected_index: 0,
            should_quit: false,
            daemon_connected: false,
            daemon_tx: None,
            status_msg: None,
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>, duration: Duration) {
        self.status_msg = Some(StatusMsg {
            text: msg.into(),
            expires: Instant::now() + duration,
        });
    }

    pub fn current_status_msg(&self) -> Option<&str> {
        self.status_msg.as_ref().and_then(|m| {
            if Instant::now() < m.expires {
                Some(m.text.as_str())
            } else {
                None
            }
        })
    }

    pub fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selected_index)
    }

    pub fn move_down(&mut self) {
        if !self.sessions.is_empty() {
            self.selected_index = (self.selected_index + 1).min(self.sessions.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    async fn send_to_daemon(&self, msg: ClientMessage) {
        if let Some(tx) = &self.daemon_tx {
            let _ = tx.send(msg).await;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Main TUI Entry Point ─────────────────────────────────────────────────────

/// Run the TUI. Connects to daemon, then runs the event loop.
pub async fn run_tui() -> Result<()> {
    let stream = match connect_to_daemon().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, stream).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    stream: UnixStream,
) -> Result<()> {
    let mut app = App::new();
    app.daemon_connected = true;

    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Outbound channel: main loop → daemon writer task
    let (daemon_tx, mut daemon_rx) = mpsc::channel::<ClientMessage>(32);
    app.daemon_tx = Some(daemon_tx);

    // Send initial Subscribe, then hand write_half to the writer task
    {
        let mut write_half = write_half;
        send_message(&mut write_half, &ClientMessage::Subscribe).await?;

        tokio::spawn(async move {
            while let Some(msg) = daemon_rx.recv().await {
                if send_message(&mut write_half, &msg).await.is_err() {
                    break;
                }
            }
        });
    }

    // Inbound channel: daemon reader task → main loop
    let (msg_tx, mut msg_rx) = mpsc::channel::<DaemonMessage>(64);

    tokio::spawn(async move {
        loop {
            match read_message::<DaemonMessage>(&mut reader).await {
                Ok(Some(msg)) => {
                    if msg_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| render(f, &app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                handle_key(&mut app, key.code, key.modifiers).await;
            }
        }

        // Drain daemon messages (non-blocking)
        loop {
            match msg_rx.try_recv() {
                Ok(msg) => handle_daemon_message(&mut app, msg),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    app.daemon_connected = false;
                    app.set_status("Daemon disconnected", Duration::from_secs(300));
                    break;
                }
            }
        }

        // Periodic tick: clear expired status messages
        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
            if let Some(m) = &app.status_msg {
                if Instant::now() >= m.expires {
                    app.status_msg = None;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

// ─── Key Handling ──────────────────────────────────────────────────────────────

async fn handle_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char('r') => {
            app.send_to_daemon(ClientMessage::RefreshAll).await;
            app.set_status("Refreshing...", Duration::from_secs(2));
        }
        KeyCode::Char('a') => {
            if let Some(session) = app.selected_session() {
                let session_id = session.id.clone();
                app.send_to_daemon(ClientMessage::Approve { session_id }).await;
                app.set_status("Approving...", Duration::from_secs(3));
            } else {
                app.set_status("No session selected", Duration::from_secs(2));
            }
        }
        KeyCode::Enter => {
            if let Some(session) = app.selected_session() {
                let session_id = session.id.clone();
                // Drop-in: actual logic in interaction.rs (Task 14)
                app.send_to_daemon(ClientMessage::DropIn { session_id }).await;
            }
        }
        KeyCode::Char('?') => {
            app.set_status(
                "j/k: navigate  a: approve  enter: drop-in  r: refresh  q: quit",
                Duration::from_secs(5),
            );
        }
        _ => {}
    }
}

// ─── Daemon Message Handling ───────────────────────────────────────────────────

fn handle_daemon_message(app: &mut App, msg: DaemonMessage) {
    match msg {
        DaemonMessage::StateSnapshot { sessions, hosts } => {
            app.sessions = sessions;
            app.hosts = hosts;
            clamp_index(app);
        }
        DaemonMessage::SessionUpdated { session } => {
            if let Some(existing) = app.sessions.iter_mut().find(|s| s.id == session.id) {
                *existing = session;
            } else {
                app.sessions.push(session);
            }
        }
        DaemonMessage::Bell {
            session_id, reason, ..
        } => {
            app.set_status(
                format!("⚠ {session_id} needs attention ({reason})"),
                Duration::from_secs(10),
            );
        }
        DaemonMessage::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        DaemonMessage::DaemonStatus {
            sessions, hosts, ..
        } => {
            app.sessions = sessions;
            app.hosts = hosts;
            clamp_index(app);
        }
    }
}

fn clamp_index(app: &mut App) {
    if app.sessions.is_empty() {
        app.selected_index = 0;
    } else {
        app.selected_index = app.selected_index.min(app.sessions.len() - 1);
    }
}

// ─── Rendering ─────────────────────────────────────────────────────────────────

/// Main render function — lays out the 3-panel structure.
fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    if area.width < 40 || area.height < 10 {
        let msg =
            Paragraph::new("Terminal too small\n(min 40x10)").style(Style::default().fg(Color::Red));
        f.render_widget(msg, area);
        return;
    }

    // Vertical split: main content + status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let main_area = chunks[0];
    let status_area = chunks[1];

    // Horizontal split: session list (60%) | detail (40%)
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(main_area);

    render_session_list(f, app, main_chunks[0]);
    render_detail(f, app, main_chunks[1]);
    render_status_bar(f, app, status_area);
}

fn render_session_list(f: &mut Frame, app: &App, area: Rect) {
    let content = if !app.daemon_connected {
        "Daemon disconnected".to_string()
    } else if app.sessions.is_empty() {
        "No sessions found\n\nStart OpenCode with:\n  opencode --port 0".to_string()
    } else {
        app.sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let prefix = if i == app.selected_index { "▶ " } else { "  " };
                let title: String = s.title.chars().take(40).collect();
                format!("{prefix}{} {title} [{}]", s.state.icon(), s.host)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let block = Block::bordered().title(" Sessions ");
    let paragraph = Paragraph::new(content).block(block);
    f.render_widget(paragraph, area);
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let content = match app.selected_session() {
        None => "Select a session".to_string(),
        Some(s) => {
            let title: String = s.title.chars().take(50).collect();
            let dir: String = s.working_dir.chars().take(40).collect();
            format!(
                "Title:   {title}\nState:   {} {}\nHost:    {}\nModel:   {}\nDir:     {dir}\nUptime:  {}",
                s.state.icon(),
                s.state,
                s.host,
                s.model.as_deref().unwrap_or("—"),
                s.uptime_human(),
            )
        }
    };

    let block = Block::bordered().title(" Detail ");
    let paragraph = Paragraph::new(content).block(block);
    f.render_widget(paragraph, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let text = if !app.daemon_connected {
        "⚠ Daemon disconnected — start with: ocwatch daemon start".to_string()
    } else {
        let counts = count_by_state(&app.sessions);
        let left = format!(
            "B:{} I:{} W:{} E:{}",
            counts.busy, counts.idle, counts.waiting, counts.error
        );
        let right = "[?] help  [a] approve  [⏎] drop-in  [q] quit";
        match app.current_status_msg() {
            Some(center) => format!("{left}  {center}  {right}"),
            None => format!("{left}  {right}"),
        }
    };

    let paragraph = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(paragraph, area);
}

struct StateCounts {
    busy: usize,
    idle: usize,
    waiting: usize,
    error: usize,
}

fn count_by_state(sessions: &[SessionInfo]) -> StateCounts {
    StateCounts {
        busy: sessions
            .iter()
            .filter(|s| s.state == SessionState::Busy)
            .count(),
        idle: sessions
            .iter()
            .filter(|s| s.state == SessionState::Idle)
            .count(),
        waiting: sessions
            .iter()
            .filter(|s| {
                s.state == SessionState::WaitingForPermission
                    || s.state == SessionState::WaitingForInput
            })
            .count(),
        error: sessions
            .iter()
            .filter(|s| s.state == SessionState::Error)
            .count(),
    }
}
