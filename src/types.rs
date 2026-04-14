use serde::{Deserialize, Serialize};
use std::fmt;

/// Session state from OpenCode. Maps to OC status strings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Busy,
    Idle,
    #[serde(rename = "waiting_for_permission")]
    WaitingForPermission,
    #[serde(rename = "waiting_for_input")]
    WaitingForInput,
    Error,
    Compacting,
    Completed,
    /// OC instance unreachable (daemon-internal state)
    Disconnected,
    Unknown,
}

impl SessionState {
    /// Parse from OC API status string. Handles both snake_case and camelCase.
    pub fn from_oc_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "busy" | "running" => SessionState::Busy,
            "idle" => SessionState::Idle,
            "waiting_for_permission" | "waitingforpermission" => SessionState::WaitingForPermission,
            "waiting_for_input" | "waitingforinput" => SessionState::WaitingForInput,
            "error" => SessionState::Error,
            "compacting" => SessionState::Compacting,
            "completed" | "done" => SessionState::Completed,
            "disconnected" => SessionState::Disconnected,
            _ => SessionState::Unknown,
        }
    }

    /// Returns true if this state warrants a tmux bell notification.
    pub fn should_bell(&self) -> bool {
        matches!(
            self,
            SessionState::Idle
                | SessionState::WaitingForPermission
                | SessionState::WaitingForInput
                | SessionState::Error
        )
    }

    /// Short display string for TUI status bar aggregate counts.
    pub fn short_label(&self) -> &'static str {
        match self {
            SessionState::Busy => "B",
            SessionState::Idle => "I",
            SessionState::WaitingForPermission | SessionState::WaitingForInput => "W",
            SessionState::Error => "E",
            SessionState::Compacting => "C",
            SessionState::Completed => "D",
            SessionState::Disconnected => "X",
            SessionState::Unknown => "?",
        }
    }

    /// Unicode icon for TUI list display.
    pub fn icon(&self) -> &'static str {
        match self {
            SessionState::Idle => "●",
            SessionState::Busy => "◐",
            SessionState::WaitingForPermission => "◉",
            SessionState::WaitingForInput => "?",
            SessionState::Error => "✗",
            SessionState::Disconnected => "○",
            SessionState::Compacting => "⟳",
            SessionState::Completed => "✓",
            SessionState::Unknown => "·",
        }
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SessionState::Busy => "Busy",
            SessionState::Idle => "Idle",
            SessionState::WaitingForPermission => "Waiting for Permission",
            SessionState::WaitingForInput => "Waiting for Input",
            SessionState::Error => "Error",
            SessionState::Compacting => "Compacting",
            SessionState::Completed => "Completed",
            SessionState::Disconnected => "Disconnected",
            SessionState::Unknown => "Unknown",
        };
        write!(f, "{}", s)
    }
}

/// Full session metadata. All fields use serializable primitives ONLY.
/// IMPORTANT: Do NOT add std::time::Instant or std::time::Duration here.
/// Internal runtime state uses separate non-serialized structs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session ID from OpenCode (e.g. "ses_274abc")
    pub id: String,
    /// Host name from config ("local" or configured name like "megaserver")
    pub host: String,
    /// Current session state
    pub state: SessionState,
    /// Session title (first user prompt or user-defined)
    pub title: String,
    /// AI model being used (e.g. "claude-sonnet-4-6") — may be None until first message
    pub model: Option<String>,
    /// Working directory for this session
    pub working_dir: String,
    /// Token counts — 0 if not yet available
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cache: u64,
    /// Current tool being executed, if any
    pub current_tool: Option<String>,
    /// Seconds since session was created (computed from OC time.created)
    /// NOTE: u64 not Duration — Serialize/Deserialize safe
    pub uptime_secs: u64,
    /// OpenCode HTTP API port (local or forwarded via SSH)
    pub oc_port: u16,
    /// tmux coordinates — None for remote sessions without tmux
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub tmux_pane: Option<String>,
}

impl SessionInfo {
    /// Composite key for daemon's session map: "{host}:{id}"
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.id)
    }

    /// Format uptime as human-readable string (e.g. "1h 23m", "45s")
    pub fn uptime_human(&self) -> String {
        let secs = self.uptime_secs;
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

/// Host connection status. Uses primitive types only (no Instant).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostStatus {
    /// Host name from config
    pub name: String,
    /// Whether the SSH/local connection is alive
    pub connected: bool,
    /// Number of active sessions on this host
    pub session_count: usize,
    /// Unix timestamp millis of last successful poll
    /// NOTE: Option<u64> not Option<Instant> — Serialize/Deserialize safe
    pub last_poll_unix_ms: Option<u64>,
    /// Last error message if not connected
    pub error: Option<String>,
}
