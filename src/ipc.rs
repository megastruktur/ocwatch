use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use crate::types::{HostStatus, SessionInfo};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentDirEntry {
    pub host: String,
    pub directory: String,
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AttachSpec {
    LocalTmux {
        session: String,
        window: Option<String>,
        pane: Option<String>,
    },
    Exec {
        program: String,
        args: Vec<String>,
        tmux_window_name: Option<String>,
    },
}

// ─── Message Types ────────────────────────────────────────────────────────────

/// Messages sent FROM the daemon TO connected clients.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// Full state snapshot — sent immediately when a client subscribes.
    StateSnapshot {
        sessions: Vec<SessionInfo>,
        hosts: Vec<HostStatus>,
    },
    /// A single session was updated (state change, token update, etc.)
    SessionUpdated {
        session: SessionInfo,
    },
    /// A tmux bell should fire for this session.
    Bell {
        session_id: String,
        host: String,
        reason: String, // "idle", "error", "permission", "input"
    },
    /// Generic error message.
    Error {
        message: String,
    },
    /// Response to GetStatus — full daemon state as JSON.
    DaemonStatus {
        running: bool,
        pid: u32,
        uptime_secs: u64,
        socket: String,
        hosts: Vec<HostStatus>,
        sessions: Vec<SessionInfo>,
    },
    RecentDirs {
        entries: Vec<RecentDirEntry>,
        is_complete: bool,
    },
    AttachReady {
        attach: AttachSpec,
    },
}

/// Messages sent FROM clients TO the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to state updates (TUI sends this on connect).
    Subscribe,
    /// Approve a pending permission request.
    Approve {
        session_id: String,
    },
    /// Request drop-in to a session (daemon handles routing).
    DropIn {
        session_id: String,
    },
    /// Force re-scan all hosts.
    RefreshAll,
    /// Request a unified recent directory list.
    GetRecentDirs {
        limit: u8,
    },
    /// Create a tmux session on a host and launch opencode.
    CreateSession {
        host: String,
        directory: String,
        name_hint: Option<String>,
    },
    /// Gracefully stop the daemon.
    Shutdown,
    /// Request current DaemonStatus (for 'daemon status' CLI command).
    GetStatus,
    /// Inject a synthetic state change for QA testing.
    InjectEvent {
        session_id: String,
        state: String,
    },
}

// ─── Message I/O ─────────────────────────────────────────────────────────────

/// Send a single JSON Lines message over a UnixStream.
/// Serializes to JSON, appends newline, and flushes.
pub async fn send_message<T: Serialize>(
    stream: &mut (impl AsyncWriteExt + Unpin),
    message: &T,
) -> Result<()> {
    let json = serde_json::to_string(message)
        .context("Failed to serialize IPC message")?;
    stream.write_all(json.as_bytes()).await
        .context("Failed to write IPC message")?;
    stream.write_all(b"\n").await
        .context("Failed to write newline")?;
    stream.flush().await
        .context("Failed to flush IPC stream")?;
    Ok(())
}

/// Read a single JSON Lines message from a BufReader.
/// Returns None if the connection was closed.
pub async fn read_message<T: for<'de> Deserialize<'de>>(
    reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>,
) -> Result<Option<T>> {
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line).await
        .context("Failed to read IPC message")?;
    
    if bytes_read == 0 {
        return Ok(None); // Connection closed
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let message: T = serde_json::from_str(trimmed)
        .with_context(|| format!("Failed to deserialize IPC message: {}", trimmed))?;
    
    Ok(Some(message))
}

// ─── Client Connection ────────────────────────────────────────────────────────

/// Returns the socket path used by the daemon.
/// Delegates to daemon::lifecycle::socket_path() but duplicated here to avoid circular deps.
pub fn socket_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ocwatch")
        .join("ocwatch.sock")
}

/// Connect to the running daemon's Unix socket.
/// Returns the stream ready for use.
pub async fn connect_to_daemon() -> Result<UnixStream> {
    let socket = socket_path();
    
    if !socket.exists() {
        anyhow::bail!(
            "Daemon is not running. Start it with: ocwatch daemon start\n\
             (socket not found: {})", 
            socket.display()
        );
    }

    UnixStream::connect(&socket).await
        .with_context(|| format!("Failed to connect to daemon socket: {:?}", socket))
}

// ─── Broadcast Channel Type ───────────────────────────────────────────────────

/// Channel capacity for daemon→client broadcast messages.
pub const BROADCAST_CAPACITY: usize = 64;

/// Type alias for the broadcast sender used by the daemon to fan-out to all clients.
pub type BroadcastTx = tokio::sync::broadcast::Sender<DaemonMessage>;
/// Type alias for the broadcast receiver used by each connected client.
pub type BroadcastRx = tokio::sync::broadcast::Receiver<DaemonMessage>;

/// Create a new broadcast channel for daemon→client communication.
pub fn new_broadcast() -> (BroadcastTx, BroadcastRx) {
    tokio::sync::broadcast::channel(BROADCAST_CAPACITY)
}
