use crate::types::{SessionInfo, SessionState};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const BELL_COOLDOWN: Duration = Duration::from_secs(30);

pub struct BellNotifier {
    last_bell: HashMap<String, Instant>,
}

impl BellNotifier {
    pub fn new() -> Self {
        BellNotifier {
            last_bell: HashMap::new(),
        }
    }

    pub fn should_bell(
        &mut self,
        session_id: &str,
        old_state: &SessionState,
        new_state: &SessionState,
    ) -> bool {
        if old_state == new_state {
            return false;
        }
        if !new_state.should_bell() {
            return false;
        }
        if let Some(&last) = self.last_bell.get(session_id) {
            if last.elapsed() < BELL_COOLDOWN {
                return false;
            }
        }
        self.last_bell
            .insert(session_id.to_string(), Instant::now());
        true
    }

    pub fn fire_bell(&self, session: &SessionInfo, reason: &str) {
        tracing::info!("bell fired for session {} reason={}", session.id, reason);

        // Strategy 1: write BEL to the OC session's tmux pane TTY
        if let (Some(tmux_session), Some(tmux_window), Some(tmux_pane)) = (
            session.tmux_session.as_deref(),
            session.tmux_window.as_deref(),
            session.tmux_pane.as_deref(),
        ) {
            let target = format!("{}.{}", tmux_session, tmux_window);
            let pane_target = format!("{}:{}.{}", tmux_session, tmux_window, tmux_pane);

            let tty_result = std::process::Command::new("tmux")
                .args(["display-message", "-t", &pane_target, "-p", "#{pane_tty}"])
                .output();

            if let Ok(out) = tty_result {
                let pane_tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !pane_tty.is_empty() && pane_tty.starts_with("/dev/") {
                    let _ = std::fs::write(&pane_tty, "\x07");
                    return;
                }
            }

            // Strategy 2: tmux display-message as fallback (shows notification in status bar)
            let msg = format!(
                "ocwatch: {} needs attention ({})",
                session.title.chars().take(30).collect::<String>(),
                reason
            );
            let _ = std::process::Command::new("tmux")
                .args(["display-message", "-t", &target, "-d", "5000", &msg])
                .spawn();
        } else {
            // Strategy 3: broadcast to all tmux panes via tmux display-message
            let msg = format!(
                "ocwatch: {} needs attention ({})",
                session.title.chars().take(30).collect::<String>(),
                reason
            );
            let _ = std::process::Command::new("tmux")
                .args(["display-message", "-d", "5000", &msg])
                .spawn();
        }
    }
}

impl Default for BellNotifier {
    fn default() -> Self {
        Self::new()
    }
}
