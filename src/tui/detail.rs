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

fn build_detail_lines(s: &SessionInfo) -> Vec<Line<'static>> {
    let state_color = match &s.state {
        SessionState::Idle => Color::Green,
        SessionState::Busy => Color::Yellow,
        SessionState::WaitingForPermission
        | SessionState::WaitingForInput
        | SessionState::Error => Color::Red,
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
        row(label, value, "Model", s.model.as_deref().unwrap_or("—")),
        row(label, value, "Dir", &truncate(&s.working_dir, 35)),
        row(
            label,
            value,
            "Tool",
            s.current_tool.as_deref().unwrap_or("—"),
        ),
        row(label, value, "Uptime", &s.uptime_human()),
        Line::raw(""),
        Line::from(Span::styled("─── Tokens ─────────────────────", section)),
        row(label, value, "Input", &fmt_num(s.tokens_in)),
        row(label, value, "Output", &fmt_num(s.tokens_out)),
        row(label, value, "Cache", &fmt_num(s.tokens_cache)),
    ];

    if s.tmux_session.is_some() {
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

fn row(label: Style, value: Style, key: &'static str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<8} ", format!("{}:", key)), label),
        Span::styled(val.to_string(), value),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn fmt_num(n: u64) -> String {
    if n == 0 {
        "—".to_string()
    } else {
        format!("{}", n)
    }
}
