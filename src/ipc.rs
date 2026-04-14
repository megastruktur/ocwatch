use crate::types::{HostStatus, SessionInfo};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DaemonMessage {
    StateSnapshot {
        sessions: Vec<SessionInfo>,
        hosts: Vec<HostStatus>,
    },
    SessionUpdated {
        session: SessionInfo,
    },
    Bell {
        session_id: String,
        host: String,
        reason: String,
    },
    Error {
        message: String,
    },
    DaemonStatus {
        running: bool,
        pid: u32,
        uptime_secs: u64,
        socket: String,
        hosts: Vec<HostStatus>,
        sessions: Vec<SessionInfo>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientMessage {
    Subscribe,
    Approve { session_id: String },
    DropIn { session_id: String },
    RefreshAll,
    Shutdown,
    GetStatus,
    InjectEvent { session_id: String, state: String },
}
