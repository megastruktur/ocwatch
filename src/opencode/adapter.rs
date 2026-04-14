//! OpenCode implementation of the CodingAgent trait.

use anyhow::Result;
use async_trait::async_trait;

use crate::agent_trait::CodingAgent;
use crate::opencode::client::OcClient;
use crate::types::SessionState;

/// OpenCode agent adapter — implements CodingAgent for OpenCode instances.
pub struct OpenCodeAgent;

impl OpenCodeAgent {
    pub fn new() -> Self {
        OpenCodeAgent
    }
}

impl Default for OpenCodeAgent {
    fn default() -> Self {
        OpenCodeAgent::new()
    }
}

#[async_trait]
impl CodingAgent for OpenCodeAgent {
    async fn get_status(&self, session_id: &str, oc_base_url: &str) -> Result<SessionState> {
        let client = OcClient::new(oc_base_url)?;
        let statuses = client.get_session_statuses().await?;
        Ok(OcClient::session_state_from_status(session_id, &statuses))
    }

    async fn send_message(&self, session_id: &str, oc_base_url: &str, message: &str) -> Result<()> {
        let client = OcClient::new(oc_base_url)?;
        client.send_message(session_id, message).await
    }

    async fn approve(&self, session_id: &str, oc_base_url: &str) -> Result<()> {
        let client = OcClient::new(oc_base_url)?;
        let patch = serde_json::json!({
            "permission": {"action": "allow"}
        });
        client.update_session(session_id, &patch).await
    }

    fn agent_type(&self) -> &'static str {
        "opencode"
    }
}
