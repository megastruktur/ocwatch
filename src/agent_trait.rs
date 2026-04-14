use async_trait::async_trait;
use anyhow::Result;
use crate::types::SessionState;

#[derive(Clone, Debug)]
pub enum AgentEvent {
    StatusChanged { session_id: String, new_state: SessionState },
    PermissionAsked { session_id: String, tool: String, description: String },
    Heartbeat,
    Error { message: String },
}

#[async_trait]
pub trait CodingAgent: Send + Sync {
    async fn get_status(&self, session_id: &str, oc_base_url: &str) -> Result<SessionState>;
    async fn send_message(&self, session_id: &str, oc_base_url: &str, message: &str) -> Result<()>;
    async fn approve(&self, session_id: &str, oc_base_url: &str) -> Result<()>;
    fn agent_type(&self) -> &'static str;
}
