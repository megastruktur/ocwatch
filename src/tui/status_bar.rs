use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::types::{SessionInfo, SessionState};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    if !app.daemon_connected {
        let p = Paragraph::new("⚠ Daemon disconnected — start with: ocwatch daemon start")
            .style(Style::default().fg(Color::Red));
        f.render_widget(p, area);
        return;
    }

    let counts = count_by_state(&app.sessions);
    let left = format!(
        "B:{} I:{} W:{} E:{}",
        counts.0, counts.1, counts.2, counts.3
    );
    let right = "[←/→] tree  [?] help  [n] new  [a] approve  [⏎] drop-in  [q] quit";
    let center = app.current_status_msg().unwrap_or("").to_string();

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left.len() as u16 + 2),
            Constraint::Min(1),
            Constraint::Length(right.len() as u16),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(left).style(Style::default().fg(Color::DarkGray)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(center).style(Style::default().fg(Color::Yellow)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(right).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn count_by_state(sessions: &[SessionInfo]) -> (usize, usize, usize, usize) {
    let busy = sessions
        .iter()
        .filter(|s| s.state == SessionState::Busy)
        .count();
    let idle = sessions
        .iter()
        .filter(|s| s.state == SessionState::Idle)
        .count();
    let waiting = sessions
        .iter()
        .filter(|s| {
            s.state == SessionState::WaitingForPermission
                || s.state == SessionState::WaitingForInput
        })
        .count();
    let error = sessions
        .iter()
        .filter(|s| s.state == SessionState::Error)
        .count();
    (busy, idle, waiting, error)
}
