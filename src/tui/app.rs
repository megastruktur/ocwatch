//! TUI application — App struct, event loop, layout skeleton.
//! Panel content rendering is in session_list.rs, detail.rs, status_bar.rs.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::ipc::{
    connect_to_daemon, read_message, send_message, AttachSpec, ClientMessage, DaemonMessage,
    RecentDirEntry,
};
use crate::types::{HostStatus, SessionInfo};

struct StatusMsg {
    text: String,
    expires: Instant,
}

struct RecentDirsModal {
    entries: Vec<RecentDirEntry>,
    selected_index: usize,
    is_complete: bool,
}

pub struct App {
    pub sessions: Vec<SessionInfo>,
    pub hosts: Vec<HostStatus>,
    pub expanded_session_keys: HashSet<String>,
    pub selected_index: usize,
    pub should_quit: bool,
    pub daemon_connected: bool,
    daemon_msg_tx: Option<mpsc::Sender<DaemonMessage>>,
    status_msg: Option<StatusMsg>,
    attention_session_keys: HashSet<String>,
    pending_attach: Option<AttachSpec>,
    recent_dirs_cache: Vec<RecentDirEntry>,
    recent_dirs_modal: Option<RecentDirsModal>,
}

impl App {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            hosts: Vec::new(),
            expanded_session_keys: HashSet::new(),
            selected_index: 0,
            should_quit: false,
            daemon_connected: false,
            daemon_msg_tx: None,
            status_msg: None,
            attention_session_keys: HashSet::new(),
            pending_attach: None,
            recent_dirs_cache: Vec::new(),
            recent_dirs_modal: None,
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>, duration: Duration) {
        self.status_msg = Some(StatusMsg {
            text: msg.into(),
            expires: Instant::now() + duration,
        });
    }

    pub fn current_status_msg(&self) -> Option<&str> {
        self.status_msg.as_ref().and_then(|message| {
            if Instant::now() < message.expires {
                Some(message.text.as_str())
            } else {
                None
            }
        })
    }

    pub fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selected_index)
    }

    pub fn session_has_attention(&self, session: &SessionInfo) -> bool {
        self.attention_session_keys.contains(&session.key())
    }

    fn clear_attention_for_selected(&mut self) {
        if let Some(session) = self.selected_session() {
            self.attention_session_keys.remove(&session.key());
        }
    }

    pub fn move_down(&mut self) {
        let ordered = crate::tui::session_list::ordered_session_indices(
            &self.sessions,
            &self.expanded_session_keys,
        );
        if ordered.is_empty() {
            return;
        }

        let current_pos = ordered
            .iter()
            .position(|&idx| idx == self.selected_index)
            .unwrap_or(0);
        let next_pos = (current_pos + 1).min(ordered.len() - 1);
        self.selected_index = ordered[next_pos];
        self.clear_attention_for_selected();
    }

    pub fn move_up(&mut self) {
        let ordered = crate::tui::session_list::ordered_session_indices(
            &self.sessions,
            &self.expanded_session_keys,
        );
        if ordered.is_empty() {
            return;
        }

        let current_pos = ordered
            .iter()
            .position(|&idx| idx == self.selected_index)
            .unwrap_or(0);
        let prev_pos = current_pos.saturating_sub(1);
        self.selected_index = ordered[prev_pos];
        self.clear_attention_for_selected();
    }

    pub fn expand_selected(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if crate::tui::session_list::session_has_children(&self.sessions, self.selected_index) {
            self.expanded_session_keys.insert(session.key());
        }
        self.ensure_selected_visible();
    }

    pub fn collapse_selected(&mut self) {
        let selected_key = self.selected_session().map(SessionInfo::key);
        let selected_has_children =
            crate::tui::session_list::session_has_children(&self.sessions, self.selected_index);

        if let Some(selected_key) = selected_key {
            if selected_has_children && self.expanded_session_keys.remove(&selected_key) {
                self.ensure_selected_visible();
                self.clear_attention_for_selected();
                return;
            }
        }

        if let Some(parent_index) =
            crate::tui::session_list::parent_session_index(&self.sessions, self.selected_index)
        {
            let parent_key = self.sessions[parent_index].key();
            self.expanded_session_keys.remove(&parent_key);
            self.selected_index = parent_index;
            self.ensure_selected_visible();
            self.clear_attention_for_selected();
        }
    }

    fn ensure_selected_visible(&mut self) {
        if self.sessions.is_empty() {
            self.selected_index = 0;
            return;
        }

        let ordered = crate::tui::session_list::ordered_session_indices(
            &self.sessions,
            &self.expanded_session_keys,
        );
        let Some(&first_visible) = ordered.first() else {
            self.selected_index = 0;
            return;
        };

        if ordered.contains(&self.selected_index) {
            return;
        }

        let mut current_index = self.selected_index;
        while let Some(parent_index) =
            crate::tui::session_list::parent_session_index(&self.sessions, current_index)
        {
            if ordered.contains(&parent_index) {
                self.selected_index = parent_index;
                return;
            }
            current_index = parent_index;
        }

        self.selected_index = first_visible;
    }

    fn request_daemon(&self, msg: ClientMessage) {
        let Some(msg_tx) = self.daemon_msg_tx.clone() else {
            return;
        };

        tokio::spawn(async move {
            let stream = match connect_to_daemon().await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = msg_tx
                        .send(DaemonMessage::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
            };

            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            if let Err(error) = send_message(&mut write_half, &msg).await {
                let _ = msg_tx
                    .send(DaemonMessage::Error {
                        message: error.to_string(),
                    })
                    .await;
                return;
            }

            loop {
                match read_message::<DaemonMessage>(&mut reader).await {
                    Ok(Some(message)) => {
                        if msg_tx.send(message).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = msg_tx
                            .send(DaemonMessage::Error {
                                message: error.to_string(),
                            })
                            .await;
                        return;
                    }
                }
            }
        });
    }

    fn open_recent_dirs_modal(&mut self) {
        self.recent_dirs_modal = Some(RecentDirsModal {
            entries: self.recent_dirs_cache.clone(),
            selected_index: 0,
            is_complete: false,
        });
        self.request_daemon(ClientMessage::GetRecentDirs { limit: 10 });
    }

    fn close_recent_dirs_modal(&mut self) {
        self.recent_dirs_modal = None;
    }

    fn update_recent_dirs(&mut self, entries: Vec<RecentDirEntry>, is_complete: bool) {
        self.recent_dirs_cache = entries.clone();
        if let Some(modal) = &mut self.recent_dirs_modal {
            modal.entries = entries;
            modal.is_complete = is_complete;
            if modal.entries.is_empty() {
                modal.selected_index = 0;
            } else {
                modal.selected_index = modal.selected_index.min(modal.entries.len() - 1);
            }
        }
    }

    fn move_recent_dirs_down(&mut self) {
        let Some(modal) = &mut self.recent_dirs_modal else {
            return;
        };
        if modal.entries.is_empty() {
            return;
        }
        modal.selected_index = (modal.selected_index + 1).min(modal.entries.len() - 1);
    }

    fn move_recent_dirs_up(&mut self) {
        let Some(modal) = &mut self.recent_dirs_modal else {
            return;
        };
        if modal.entries.is_empty() {
            return;
        }
        modal.selected_index = modal.selected_index.saturating_sub(1);
    }

    fn selected_recent_dir(&self) -> Option<&RecentDirEntry> {
        self.recent_dirs_modal
            .as_ref()
            .and_then(|modal| modal.entries.get(modal.selected_index))
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run_tui() -> Result<()> {
    let stream = match connect_to_daemon().await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("Error: {error}");
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

    let pending_attach = result?;
    if let Some(attach) = pending_attach {
        if let Some(message) = crate::tui::interaction::execute_attach(attach) {
            eprintln!("{}", message);
        }
    }

    Ok(())
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    stream: UnixStream,
) -> Result<Option<AttachSpec>> {
    let mut app = App::new();
    app.daemon_connected = true;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    send_message(&mut write_half, &ClientMessage::Subscribe).await?;

    let (msg_tx, mut msg_rx) = mpsc::channel::<DaemonMessage>(64);
    app.daemon_msg_tx = Some(msg_tx.clone());

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
        terminal.draw(|frame| render(frame, &app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                handle_key(&mut app, key.code, key.modifiers).await;
            }
        }

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

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
            if let Some(status_msg) = &app.status_msg {
                if Instant::now() >= status_msg.expires {
                    app.status_msg = None;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(app.pending_attach)
}

async fn handle_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    if app.recent_dirs_modal.is_some() {
        handle_recent_dirs_key(app, key, modifiers).await;
        return;
    }

    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Right | KeyCode::Char('l') => app.expand_selected(),
        KeyCode::Left | KeyCode::Char('h') => app.collapse_selected(),
        KeyCode::Char('r') => {
            app.request_daemon(ClientMessage::RefreshAll);
            app.set_status("Refreshing...", Duration::from_secs(2));
        }
        KeyCode::Char('a') => {
            if let Some(session) = app.selected_session() {
                if session.state != crate::types::SessionState::WaitingForPermission {
                    app.set_status(
                        "Nothing to approve (session not waiting for permission)",
                        Duration::from_secs(3),
                    );
                    return;
                }

                app.request_daemon(ClientMessage::Approve {
                    session_id: session.id.clone(),
                });
                app.set_status("Approving...", Duration::from_secs(3));
            } else {
                app.set_status("No session selected", Duration::from_secs(2));
            }
        }
        KeyCode::Enter => {
            if let Some(session) = app.selected_session() {
                app.request_daemon(ClientMessage::DropIn {
                    session_id: session.id.clone(),
                });
                app.set_status("Preparing drop-in...", Duration::from_secs(3));
            }
        }
        KeyCode::Char('n') => {
            app.open_recent_dirs_modal();
            app.set_status("Loading recent directories...", Duration::from_secs(3));
        }
        KeyCode::Char('?') => {
            app.set_status(
                "j/k: navigate  ←/→: collapse/expand  n: new  a: approve  enter: drop-in  r: refresh  q: quit",
                Duration::from_secs(5),
            );
        }
        _ => {}
    }
}

async fn handle_recent_dirs_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match key {
        KeyCode::Esc => app.close_recent_dirs_modal(),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_recent_dirs_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_recent_dirs_up(),
        KeyCode::Enter => {
            let Some(entry) = app.selected_recent_dir().cloned() else {
                return;
            };
            app.close_recent_dirs_modal();
            app.request_daemon(ClientMessage::CreateSession {
                host: entry.host.clone(),
                directory: entry.directory.clone(),
                name_hint: Some(infer_name_from_directory(&entry.directory)),
            });
            app.set_status(
                format!("Creating session in {}", entry.directory),
                Duration::from_secs(4),
            );
        }
        _ => {}
    }
}

fn handle_daemon_message(app: &mut App, msg: DaemonMessage) {
    match msg {
        DaemonMessage::StateSnapshot { sessions, hosts } => {
            replace_sessions(app, sessions, hosts);
        }
        DaemonMessage::SessionUpdated { session } => {
            if !session.state.should_bell() {
                app.attention_session_keys.remove(&session.key());
            }
            if let Some(existing) = app.sessions.iter_mut().find(|s| s.key() == session.key()) {
                *existing = session;
            } else {
                app.sessions.push(session);
            }
            retain_expandable_session_keys(app);
            app.ensure_selected_visible();
        }
        DaemonMessage::Bell {
            session_id,
            host,
            reason,
        } => {
            let key = format!("{}:{}", host, session_id);
            if app
                .selected_session()
                .map(|session| session.key() != key)
                .unwrap_or(true)
            {
                app.attention_session_keys.insert(key);
            }
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
            replace_sessions(app, sessions, hosts);
        }
        DaemonMessage::RecentDirs {
            entries,
            is_complete,
        } => {
            app.update_recent_dirs(entries, is_complete);
            if is_complete {
                app.set_status("Recent directories loaded", Duration::from_secs(2));
            }
        }
        DaemonMessage::AttachReady { attach } => {
            app.pending_attach = Some(attach);
            app.should_quit = true;
        }
    }
}

fn replace_sessions(app: &mut App, sessions: Vec<SessionInfo>, hosts: Vec<HostStatus>) {
    let selected_session_key = app.selected_session().map(SessionInfo::key);

    let live_attention_keys = sessions
        .iter()
        .filter(|session| session.state.should_bell())
        .map(SessionInfo::key)
        .collect::<HashSet<_>>();
    app.attention_session_keys
        .retain(|key| live_attention_keys.contains(key));

    app.sessions = sessions;
    app.hosts = hosts;
    retain_expandable_session_keys(app);

    if let Some(selected_session_key) = selected_session_key {
        if let Some(selected_index) = app
            .sessions
            .iter()
            .position(|session| session.key() == selected_session_key)
        {
            app.selected_index = selected_index;
            app.ensure_selected_visible();
            return;
        }
    }

    clamp_index(app);
}

fn clamp_index(app: &mut App) {
    if app.sessions.is_empty() {
        app.selected_index = 0;
    } else {
        app.selected_index = app.selected_index.min(app.sessions.len() - 1);
        app.ensure_selected_visible();
    }
}

fn retain_expandable_session_keys(app: &mut App) {
    let expandable_keys = app
        .sessions
        .iter()
        .enumerate()
        .filter(|(index, _)| crate::tui::session_list::session_has_children(&app.sessions, *index))
        .map(|(_, session)| session.key())
        .collect::<HashSet<_>>();
    app.expanded_session_keys
        .retain(|key| expandable_keys.contains(key));
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    if area.width < 40 || area.height < 10 {
        let message =
            Paragraph::new("Terminal too small\n(min 40x10)").style(Style::default().fg(Color::Red));
        frame.render_widget(message, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let main_area = chunks[0];
    let status_area = chunks[1];
    let min_session_list_height = 4;
    let max_detail_height = main_area.height.saturating_sub(min_session_list_height).max(1);
    let detail_height = crate::tui::detail::desired_height(app, main_area.width).min(max_detail_height);

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(detail_height)])
        .split(main_area);

    render_session_list(frame, app, main_chunks[0]);
    render_detail(frame, app, main_chunks[1]);
    render_status_bar(frame, app, status_area);

    if app.recent_dirs_modal.is_some() {
        render_recent_dirs_modal(frame, app, area);
    }
}

fn render_session_list(frame: &mut Frame, app: &App, area: Rect) {
    crate::tui::session_list::render(frame, app, area);
}

fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    crate::tui::detail::render(frame, app, area);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    crate::tui::status_bar::render(frame, app, area);
}

fn render_recent_dirs_modal(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = &app.recent_dirs_modal else {
        return;
    };

    let width = area.width.min(90).max(40);
    let height = area.height.min(16).max(8);
    let popup = centered_rect(width, height, area);
    let inner = Block::bordered()
        .title(" New OpenCode Session ")
        .border_style(Style::default().fg(Color::Cyan))
        .inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::bordered()
            .title(" New OpenCode Session ")
            .border_style(Style::default().fg(Color::Cyan)),
        popup,
    );

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let subtitle = if modal.is_complete {
        "Select a recent directory and press Enter"
    } else {
        "Loading recent directories..."
    };
    frame.render_widget(
        Paragraph::new(subtitle).style(Style::default().fg(Color::Yellow)),
        sections[0],
    );

    if modal.entries.is_empty() {
        frame.render_widget(
            Paragraph::new("No recent directories found yet")
                .style(Style::default().fg(Color::DarkGray)),
            sections[1],
        );
    } else {
        let items = modal
            .entries
            .iter()
            .map(|entry| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("[{}] ", entry.host),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(truncate_path(&entry.directory, sections[1].width.saturating_sub(10) as usize)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(modal.selected_index.min(modal.entries.len().saturating_sub(1))));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, sections[1], &mut state);
    }

    frame.render_widget(
        Paragraph::new("Enter: create + attach  Esc: close")
            .style(Style::default().fg(Color::DarkGray)),
        sections[2],
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn infer_name_from_directory(directory: &str) -> String {
    Path::new(directory)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("opencode")
        .to_string()
}

fn truncate_path(path: &str, max: usize) -> String {
    if max == 0 || path.chars().count() <= max {
        return path.to_string();
    }

    let suffix_len = max.saturating_sub(1);
    format!("…{}", path.chars().rev().take(suffix_len).collect::<String>().chars().rev().collect::<String>())
}
