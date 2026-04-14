use anyhow::Result;
use async_trait::async_trait;

use crate::types::SessionState;

/// Events emitted by CodingAgent SSE streams.
/// Note: NOT Serialize/Deserialize — these are internal runtime types.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// Session changed state
    StatusChanged {
        session_id: String,
        new_state: SessionState,
    },
    /// Permission dialog appeared
    PermissionAsked {
        session_id: String,
        tool: String,
        description: String,
    },
    /// Server keepalive
    Heartbeat,
    /// Error from the agent's event stream
    Error { message: String },
}

/// Abstraction over AI coding agents (currently: OpenCode only).
/// Trait is intentionally minimal — only 4 operations needed for v1.
/// CodingAgent is NOT responsible for discovery — that's handled by scanners.
/// CodingAgent is NOT responsible for SSH — ports are pre-forwarded by SshManager.
///
/// OBJECT SAFETY: This trait must be usable as `Box<dyn CodingAgent>`.
/// Ensure no generic methods or Self-returning methods are added.
#[async_trait]
pub trait CodingAgent: Send + Sync {
    /// Get current session state from the agent's HTTP API.
    /// `oc_base_url`: e.g. "http://localhost:4096" (already forwarded if remote)
    async fn get_status(&self, session_id: &str, oc_base_url: &str) -> Result<SessionState>;

    /// Send a text message to the session.
    async fn send_message(
        &self,
        session_id: &str,
        oc_base_url: &str,
        message: &str,
    ) -> Result<()>;

    /// Approve a pending permission request.
    async fn approve(&self, session_id: &str, oc_base_url: &str) -> Result<()>;

    /// Agent type identifier for logging/debug.
    fn agent_type(&self) -> &'static str;
}
