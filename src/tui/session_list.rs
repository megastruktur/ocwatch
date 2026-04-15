use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
    Frame,
};
use std::collections::{HashMap, HashSet};

use crate::tui::app::App;
use crate::types::{SessionInfo, SessionState};

#[derive(Clone, Copy)]
struct SessionRow {
    session_index: usize,
    depth: usize,
    has_children: bool,
    is_expanded: bool,
}

struct CwdGroup {
    label: String,
    session_rows: Vec<SessionRow>,
}

pub fn ordered_session_indices(
    sessions: &[SessionInfo],
    expanded_session_keys: &HashSet<String>,
) -> Vec<usize> {
    grouped_session_rows_by_host(sessions, expanded_session_keys)
        .into_iter()
        .flat_map(|(_, groups)| {
            groups
                .into_iter()
                .flat_map(|group| group.session_rows.into_iter().map(|row| row.session_index))
        })
        .collect()
}

pub fn parent_session_index(sessions: &[SessionInfo], session_index: usize) -> Option<usize> {
    let session = sessions.get(session_index)?;
    let parent_id = session.parent_id.as_deref()?;

    sessions
        .iter()
        .enumerate()
        .find(|(_, candidate)| {
            candidate.host == session.host
                && candidate.id == parent_id
                && candidate.parent_id.is_none()
        })
        .map(|(index, _)| index)
}

pub fn session_has_children(sessions: &[SessionInfo], session_index: usize) -> bool {
    let Some(session) = sessions.get(session_index) else {
        return false;
    };

    if session.parent_id.is_some() {
        return false;
    }

    sessions.iter().any(|candidate| {
        candidate.host == session.host
            && candidate.parent_id.as_deref() == Some(session.id.as_str())
    })
}

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
        let is_loading = app.hosts.is_empty()
            || app
                .hosts
                .iter()
                .any(|host| host.last_poll_unix_ms.is_none());
        let empty_state = if is_loading {
            "Scanning hosts…\n\nSessions will appear as each host responds."
        } else {
            "No sessions found\n\nStart OpenCode:\n  opencode --port 0"
        };
        let widget = Paragraph::new(empty_state)
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }

    let (items, visual_to_session) = build_session_items(app, area.width);

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

fn build_session_items(app: &App, area_width: u16) -> (Vec<ListItem<'static>>, Vec<Option<usize>>) {
    let by_host = grouped_session_rows_by_host(&app.sessions, &app.expanded_session_keys);
    let title_width = area_width.saturating_sub(10) as usize;

    let mut items = Vec::new();
    let mut visual_to_session: Vec<Option<usize>> = Vec::new();

    for (host, cwd_groups) in by_host {
        let total_host_sessions = app
            .sessions
            .iter()
            .filter(|session| session.host == host)
            .count();
        let header = Line::from(vec![Span::styled(
            format!("▼ {} ({})", host, total_host_sessions),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]);
        items.push(ListItem::new(header));
        visual_to_session.push(None);

        for cwd_group in cwd_groups {
            let cwd_header = Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("▾ {}", cwd_group.label),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            items.push(ListItem::new(cwd_header));
            visual_to_session.push(None);

            for row in cwd_group.session_rows {
                let session = &app.sessions[row.session_index];
                let (icon, color) = state_icon_color(&session.state);
                let attention_marker = if app.session_has_attention(session) {
                    Span::styled("⚠ ", Style::default().fg(Color::Yellow))
                } else {
                    Span::raw("  ")
                };
                let prefix = format!(
                    "  {}",
                    session_row_prefix(row.depth, row.has_children, row.is_expanded)
                );
                let title = truncate(
                    &session.title,
                    title_width.saturating_sub(prefix.chars().count()),
                );
                let line = Line::from(vec![
                    Span::styled(
                        prefix,
                        session_row_prefix_style(row.depth, row.has_children),
                    ),
                    Span::styled(icon.to_string(), Style::default().fg(color)),
                    Span::raw(" "),
                    attention_marker,
                    Span::raw(title),
                ]);
                items.push(ListItem::new(line));
                visual_to_session.push(Some(row.session_index));
            }
        }
    }

    (items, visual_to_session)
}

fn grouped_session_rows_by_host(
    sessions: &[SessionInfo],
    expanded_session_keys: &HashSet<String>,
) -> Vec<(String, Vec<CwdGroup>)> {
    let mut by_host: Vec<(String, Vec<usize>)> = Vec::new();

    for (idx, session) in sessions.iter().enumerate() {
        if let Some(group) = by_host.iter_mut().find(|(host, _)| host == &session.host) {
            group.1.push(idx);
        } else {
            by_host.push((session.host.clone(), vec![idx]));
        }
    }

    by_host
        .into_iter()
        .map(|(host, indices)| {
            (
                host,
                build_host_cwd_groups(sessions, &indices, expanded_session_keys),
            )
        })
        .collect()
}

fn build_host_cwd_groups(
    sessions: &[SessionInfo],
    session_indices: &[usize],
    expanded_session_keys: &HashSet<String>,
) -> Vec<CwdGroup> {
    let mut by_cwd: Vec<(String, Vec<usize>)> = Vec::new();
    let group_labels = unique_cwd_group_labels(
        session_indices
            .iter()
            .map(|&index| sessions[index].working_dir.as_str()),
    );
    let host_rows = build_host_session_rows(sessions, session_indices, expanded_session_keys);
    let mut group_positions: HashMap<String, usize> = HashMap::new();

    for (position, row) in host_rows.iter().enumerate() {
        let cwd = sessions[row.session_index].working_dir.clone();
        group_positions.entry(cwd).or_insert(position);
    }

    for &index in session_indices {
        let cwd = sessions[index].working_dir.clone();

        if let Some((_, indices)) = by_cwd.iter_mut().find(|(group_cwd, _)| group_cwd == &cwd) {
            indices.push(index);
        } else {
            by_cwd.push((cwd, vec![index]));
        }
    }

    let mut groups = by_cwd
        .into_iter()
        .map(|(cwd, indices)| {
            let label = group_labels
                .get(&cwd)
                .cloned()
                .unwrap_or_else(|| directory_basename(&cwd).to_string());
            let rows = build_host_session_rows(sessions, &indices, expanded_session_keys);
            let position = group_positions.get(&cwd).copied().unwrap_or(usize::MAX);

            (
                position,
                CwdGroup {
                    label,
                    session_rows: rows,
                },
            )
        })
        .collect::<Vec<_>>();

    groups.sort_by(
        |(left_position, left_group), (right_position, right_group)| {
            left_position
                .cmp(right_position)
                .then_with(|| left_group.label.cmp(&right_group.label))
        },
    );

    groups.into_iter().map(|(_, group)| group).collect()
}

fn build_host_session_rows(
    sessions: &[SessionInfo],
    session_indices: &[usize],
    expanded_session_keys: &HashSet<String>,
) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    let indices_by_id = session_indices
        .iter()
        .map(|&index| (sessions[index].id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut children_by_parent: HashMap<&str, Vec<usize>> = HashMap::new();
    let root_session_ids = session_indices
        .iter()
        .filter(|&&index| sessions[index].parent_id.is_none())
        .map(|&index| sessions[index].id.as_str())
        .collect::<HashSet<_>>();

    for &index in session_indices {
        if let Some(parent_id) = sessions[index].parent_id.as_deref() {
            if root_session_ids.contains(parent_id) {
                children_by_parent.entry(parent_id).or_default().push(index);
            }
        }
    }

    for children in children_by_parent.values_mut() {
        children.sort_by_cached_key(|&index| session_sort_key(&sessions[index]));
    }

    let mut root_indices = session_indices
        .iter()
        .copied()
        .filter(|&index| {
            sessions[index]
                .parent_id
                .as_deref()
                .and_then(|parent_id| indices_by_id.get(parent_id))
                .is_none()
        })
        .collect::<Vec<_>>();
    root_indices.sort_by_cached_key(|index| session_sort_key(&sessions[*index]));
    let mut visited = HashSet::new();

    for index in root_indices {
        append_session_row(
            sessions,
            index,
            0,
            &children_by_parent,
            expanded_session_keys,
            &mut visited,
            &mut rows,
        );
    }

    for &index in session_indices {
        let has_known_parent = sessions[index]
            .parent_id
            .as_deref()
            .map(|parent_id| indices_by_id.contains_key(parent_id))
            .unwrap_or(false);

        if !has_known_parent && visited.insert(index) {
            rows.push(SessionRow {
                session_index: index,
                depth: 0,
                has_children: session_has_children(sessions, index),
                is_expanded: expanded_session_keys.contains(&sessions[index].key()),
            });
        }
    }

    rows
}

fn append_session_row(
    sessions: &[SessionInfo],
    session_index: usize,
    depth: usize,
    children_by_parent: &HashMap<&str, Vec<usize>>,
    expanded_session_keys: &HashSet<String>,
    visited: &mut HashSet<usize>,
    rows: &mut Vec<SessionRow>,
) {
    if !visited.insert(session_index) {
        return;
    }

    let has_children = children_by_parent
        .get(sessions[session_index].id.as_str())
        .map(|children| !children.is_empty())
        .unwrap_or(false);
    let is_expanded =
        has_children && expanded_session_keys.contains(&sessions[session_index].key());

    rows.push(SessionRow {
        session_index,
        depth,
        has_children,
        is_expanded,
    });

    if !is_expanded {
        return;
    }

    if let Some(children) = children_by_parent.get(sessions[session_index].id.as_str()) {
        for &child_index in children {
            append_session_row(
                sessions,
                child_index,
                depth + 1,
                children_by_parent,
                expanded_session_keys,
                visited,
                rows,
            );
        }
    }
}

fn session_row_prefix(depth: usize, has_children: bool, is_expanded: bool) -> String {
    if has_children {
        let icon = if is_expanded { "▾" } else { "▸" };
        return format!("{}{} ", "  ".repeat(depth), icon);
    }

    if depth == 0 {
        "  ".to_string()
    } else {
        format!("{}↳ ", "  ".repeat(depth))
    }
}

fn session_row_prefix_style(depth: usize, has_children: bool) -> Style {
    if depth == 0 && !has_children {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn session_sort_key(session: &SessionInfo) -> (u64, &str, &str) {
    (
        session.activity_age_secs,
        session.title.as_str(),
        session.id.as_str(),
    )
}

fn unique_cwd_group_labels<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> HashMap<String, String> {
    let unique_paths = paths
        .into_iter()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let segment_lists = unique_paths
        .iter()
        .map(|path| (path.clone(), path_segments(path)))
        .collect::<Vec<_>>();
    let max_depth = segment_lists
        .iter()
        .map(|(_, segments)| segments.len())
        .max()
        .unwrap_or(1);

    for depth in 1..=max_depth {
        let mut labels_by_path = HashMap::new();
        let mut counts = HashMap::new();

        for (path, segments) in &segment_lists {
            let label = path_suffix_label(segments, depth);
            *counts.entry(label.clone()).or_insert(0usize) += 1;
            labels_by_path.insert(path.clone(), label);
        }

        if counts.values().all(|count| *count == 1) {
            return labels_by_path;
        }
    }

    unique_paths
        .into_iter()
        .map(|path| (path.clone(), path))
        .collect()
}

fn directory_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
}

fn path_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn path_suffix_label(segments: &[&str], depth: usize) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }

    let start = segments.len().saturating_sub(depth);
    segments[start..].join("/")
}

fn state_icon_color(state: &SessionState) -> (&'static str, Color) {
    match state {
        SessionState::Idle => ("●", Color::DarkGray),
        SessionState::Busy => ("◐", Color::Green),
        SessionState::WaitingForPermission => ("◉", Color::Yellow),
        SessionState::WaitingForInput => ("?", Color::Yellow),
        SessionState::Error => ("✗", Color::Red),
        SessionState::Disconnected => ("○", Color::DarkGray),
        SessionState::Compacting => ("⟳", Color::Blue),
        SessionState::Completed => ("✓", Color::Green),
        SessionState::Unknown => ("·", Color::DarkGray),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }

    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
