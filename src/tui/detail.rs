use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;
use crate::types::{SessionInfo, SessionState};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title(" Detail ")
        .border_style(Style::default().fg(Color::DarkGray));

    let session = match app.selected_session() {
        None => {
            let p = Paragraph::new("Select a session")
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        }
        Some(s) => s,
    };

    let lines = build_detail_lines(session);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, area);
}

pub fn desired_height(app: &App, area_width: u16) -> u16 {
    let inner_width = area_width.saturating_sub(2).max(1) as usize;

    let content_height = match app.selected_session() {
        Some(session) => build_detail_plain_lines(session)
            .into_iter()
            .map(|line| wrapped_line_count(&line, inner_width))
            .sum::<usize>(),
        None => 1,
    };

    u16::try_from(content_height.saturating_add(2)).unwrap_or(u16::MAX)
}

fn build_detail_lines(s: &SessionInfo) -> Vec<Line<'static>> {
    let state_color = match &s.state {
        SessionState::Idle => Color::DarkGray,
        SessionState::Busy => Color::Green,
        SessionState::WaitingForPermission | SessionState::WaitingForInput => Color::Yellow,
        SessionState::Error => Color::Red,
        _ => Color::DarkGray,
    };

    let label = Style::default().fg(Color::DarkGray);
    let value = Style::default().fg(Color::White);
    let section = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        row(label, value, "Title", &truncate(&s.title, 35)),
        Line::from(vec![
            Span::styled("State:   ", label),
            Span::styled(s.state.icon().to_string(), Style::default().fg(state_color)),
            Span::raw(" "),
            Span::styled(s.state.to_string(), value),
        ]),
        row(label, value, "Host", &s.host),
        row(label, value, "Dir", &s.working_dir),
        row(label, value, "Updated", &s.activity_age_human()),
    ];

    if let Some(parent_id) = &s.parent_id {
        lines.push(row(label, value, "Parent", parent_id));
    }

    if s.tmux_session.is_some() || s.tmux_window.is_some() || s.tmux_pane.is_some() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "─── tmux ────────────────────────",
            section,
        )));
        lines.push(row(
            label,
            value,
            "Session",
            s.tmux_session.as_deref().unwrap_or("—"),
        ));
        lines.push(row(
            label,
            value,
            "Window",
            s.tmux_window.as_deref().unwrap_or("—"),
        ));
        lines.push(row(
            label,
            value,
            "Pane",
            s.tmux_pane.as_deref().unwrap_or("—"),
        ));
    }

    lines
}

fn build_detail_plain_lines(s: &SessionInfo) -> Vec<String> {
    let mut lines = vec![
        row_text("Title", &truncate(&s.title, 35)),
        format!("State:   {} {}", s.state.icon(), s.state),
        row_text("Host", &s.host),
        row_text("Dir", &s.working_dir),
        row_text("Updated", &s.activity_age_human()),
    ];

    if let Some(parent_id) = &s.parent_id {
        lines.push(row_text("Parent", parent_id));
    }

    if s.tmux_session.is_some() || s.tmux_window.is_some() || s.tmux_pane.is_some() {
        lines.push(String::new());
        lines.push("─── tmux ────────────────────────".to_string());
        lines.push(row_text(
            "Session",
            s.tmux_session.as_deref().unwrap_or("—"),
        ));
        lines.push(row_text("Window", s.tmux_window.as_deref().unwrap_or("—")));
        lines.push(row_text("Pane", s.tmux_pane.as_deref().unwrap_or("—")));
    }

    lines
}

fn row(label: Style, value: Style, key: &'static str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(row_prefix(key), label),
        Span::styled(val.to_string(), value),
    ])
}

fn row_text(key: &'static str, val: &str) -> String {
    format!("{}{}", row_prefix(key), val)
}

fn row_prefix(key: &'static str) -> String {
    format!("{:<8} ", format!("{}:", key))
}

fn wrapped_line_count(line: &str, width: usize) -> usize {
    let width = width.max(1);
    let line_len = line.chars().count();

    if line_len == 0 {
        1
    } else {
        line_len.div_ceil(width)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
