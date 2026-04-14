use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SessionState {
    Busy,
    Idle,
    WaitingForPermission,
    WaitingForInput,
    Error,
    Compacting,
    Completed,
    Disconnected,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub host: String,
    pub state: SessionState,
    pub title: String,
    pub model: Option<String>,
    pub working_dir: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cache: u64,
    pub current_tool: Option<String>,
    pub uptime_secs: u64,
    pub oc_port: u16,
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub tmux_pane: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostStatus {
    pub name: String,
    pub connected: bool,
    pub session_count: usize,
    pub last_poll_unix_ms: Option<u64>,
    pub error: Option<String>,
}
