use crate::ipc::ClientMessage;
use crate::types::{SessionInfo, SessionState};

/// Handle approve action from TUI (user pressed 'a').
/// Returns a status message to display.
pub async fn handle_approve(
    session: &SessionInfo,
    daemon_tx: &tokio::sync::mpsc::Sender<ClientMessage>,
) -> String {
    if session.state != SessionState::WaitingForPermission {
        return "Nothing to approve (session not waiting for permission)".to_string();
    }

    let msg = ClientMessage::Approve {
        session_id: session.id.clone(),
    };

    match daemon_tx.send(msg).await {
        Ok(_) => format!(
            "Approved: {}",
            session.title.chars().take(40).collect::<String>()
        ),
        Err(_) => "Failed to send approve request".to_string(),
    }
}

/// Handle drop-in action from TUI (user pressed Enter).
/// Returns the action to execute (local tmux switch, remote SSH, etc).
pub fn handle_drop_in(session: &SessionInfo) -> DropInAction {
    if let (Some(tmux_session), Some(tmux_window), Some(tmux_pane)) = (
        &session.tmux_session,
        &session.tmux_window,
        &session.tmux_pane,
    ) {
        if session.host == "local" {
            DropInAction::LocalTmux {
                session: tmux_session.clone(),
                window: tmux_window.clone(),
                pane: tmux_pane.clone(),
            }
        } else {
            DropInAction::RemoteTmux {
                ssh_target: session.host.clone(),
                tmux_session: tmux_session.clone(),
                tmux_window: tmux_window.clone(),
            }
        }
    } else if session.host != "local" {
        DropInAction::RemoteSsh {
            ssh_target: session.host.clone(),
        }
    } else {
        DropInAction::NoTmux
    }
}

pub enum DropInAction {
    LocalTmux {
        session: String,
        window: String,
        pane: String,
    },
    RemoteTmux {
        ssh_target: String,
        tmux_session: String,
        tmux_window: String,
    },
    RemoteSsh {
        ssh_target: String,
    },
    NoTmux,
}

impl DropInAction {
    /// Execute the drop-in action. Returns a status message if action failed.
    pub fn execute(self) -> Option<String> {
        match self {
            DropInAction::LocalTmux {
                session,
                window,
                pane,
            } => {
                let target = format!("{}:{}.{}", session, window, pane);
                let result = std::process::Command::new("tmux")
                    .args(["switch-client", "-t", &target])
                    .status();
                if result.map(|s| s.success()).unwrap_or(false) {
                    None
                } else {
                    // Fallback: select window + pane individually
                    let _ = std::process::Command::new("tmux")
                        .args(["select-window", "-t", &format!("{}:{}", session, window)])
                        .status();
                    let _ = std::process::Command::new("tmux")
                        .args(["select-pane", "-t", &format!(".{}", pane)])
                        .status();
                    None
                }
            }
            DropInAction::RemoteTmux {
                ssh_target,
                tmux_session,
                tmux_window,
            } => {
                let attach_cmd = format!("tmux attach -t {}:{}", tmux_session, tmux_window);
                let _ = std::process::Command::new("ssh")
                    .args(["-t", &ssh_target, &attach_cmd])
                    .status();
                None
            }
            DropInAction::RemoteSsh { ssh_target } => {
                let _ = std::process::Command::new("ssh")
                    .args(["-t", &ssh_target])
                    .status();
                None
            }
            DropInAction::NoTmux => {
                Some("No tmux info — cannot drop in to this session".to_string())
            }
        }
    }
}
