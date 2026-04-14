use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::tui::app::App;
use crate::types::{SessionInfo, SessionState};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title(" Sessions ")
        .border_style(Style::default().fg(Color::DarkGray));

    if !app.daemon_connected {
        let widget = Paragraph::new("⚠ Daemon disconnected")
            .block(block)
            .style(Style::default().fg(Color::Red));
        f.render_widget(widget, area);
        return;
    }

    if app.sessions.is_empty() {
        let widget = Paragraph::new("No sessions found\n\nStart OpenCode:\n  opencode --port 0")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }

    let (items, visual_to_session) = build_session_items(app);

    // Map app.selected_index (session index) to visual list index
    let visual_selected = visual_to_session
        .iter()
        .position(|&si| si == Some(app.selected_index));

    let mut list_state = ListState::default();
    list_state.select(visual_selected);

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut list_state);
}

fn build_session_items(app: &App) -> (Vec<ListItem<'static>>, Vec<Option<usize>>) {
    let mut by_host: Vec<(String, Vec<(usize, &SessionInfo)>)> = Vec::new();
    for (idx, session) in app.sessions.iter().enumerate() {
        if let Some(group) = by_host.iter_mut().find(|(h, _)| h == &session.host) {
            group.1.push((idx, session));
        } else {
            by_host.push((session.host.clone(), vec![(idx, session)]));
        }
    }

    let mut items = Vec::new();
    let mut visual_to_session: Vec<Option<usize>> = Vec::new();

    for (host, sessions) in by_host {
        let header = Line::from(vec![Span::styled(
            format!("▼ {} ({})", host, sessions.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]);
        items.push(ListItem::new(header));
        visual_to_session.push(None);

        for (idx, session) in sessions {
            let (icon, color) = state_icon_color(&session.state);
            let title = truncate(&session.title, 38);
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(icon.to_string(), Style::default().fg(color)),
                Span::raw(" "),
                Span::raw(title),
            ]);
            items.push(ListItem::new(line));
            visual_to_session.push(Some(idx));
        }
    }

    (items, visual_to_session)
}

fn state_icon_color(state: &SessionState) -> (&'static str, Color) {
    match state {
        SessionState::Idle => ("●", Color::Green),
        SessionState::Busy => ("◐", Color::Yellow),
        SessionState::WaitingForPermission => ("◉", Color::Red),
        SessionState::WaitingForInput => ("?", Color::Yellow),
        SessionState::Error => ("✗", Color::Red),
        SessionState::Disconnected => ("○", Color::DarkGray),
        SessionState::Compacting => ("⟳", Color::Blue),
        SessionState::Completed => ("✓", Color::Green),
        SessionState::Unknown => ("·", Color::DarkGray),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
