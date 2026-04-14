//! HTTP and SSE client for the OpenCode REST API.
//!
//! OpenCode API (sst/opencode, confirmed from source code):
//! - GET  /session            → Vec<OcSession>
//! - GET  /session/status     → HashMap<String, OcSessionStatus>  (empty {} when no active sessions)
//! - GET  /session/:id        → OcSession
//! - POST /session/:id/message → send a message (streams)
//! - POST /session/:id/abort  → abort active session
//! - PATCH /session/:id       → update session (for permission approval)
//! - GET  /event              → SSE stream of OcEvent
//!
//! Session object fields (verified from live API):
//! {
//!   "id": "ses_274abc...",
//!   "slug": "happy-panda",
//!   "projectID": "global",
//!   "directory": "/path/to/project",
//!   "title": "task description",
//!   "version": "1.4.3",
//!   "time": { "created": 1776000000000, "updated": 1776000000000 }
//! }

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::SystemTime;

use crate::types::SessionState;

// ─── OpenCode API Types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct OcTime {
    pub created: u64, // Unix millis
    pub updated: u64, // Unix millis
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcSession {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub project_id: String,
    pub directory: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    pub version: String,
    pub time: OcTime,
}

impl OcSession {
    /// Compute uptime in seconds from time.created.
    pub fn uptime_secs(&self) -> u64 {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        (now_ms.saturating_sub(self.time.created)) / 1000
    }
}

/// OpenCode session status — returned by GET /session/status
/// Maps session ID → status string (e.g. "busy", "idle")
#[derive(Debug, Clone, Deserialize)]
pub struct OcSessionStatus {
    #[serde(default)]
    pub status: Option<String>,
}

/// A raw SSE event from GET /event
#[derive(Debug, Clone)]
pub struct OcEvent {
    pub event_type: String,
    pub data: String,
}

// Parsed event payload types
#[derive(Debug, Clone, Deserialize)]
pub struct OcSessionStatusEvent {
    pub session_id: Option<String>,
    pub status: Option<String>,
}

// ─── OcClient ─────────────────────────────────────────────────────────────────

/// HTTP client for a single OpenCode instance.
/// `base_url` should be like "http://localhost:4096" (no trailing slash).
pub struct OcClient {
    pub base_url: String,
    client: Client,
}

impl OcClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;
        Ok(OcClient {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }

    /// GET /session — list all sessions
    pub async fn list_sessions(&self) -> Result<Vec<OcSession>> {
        let url = format!("{}/session", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        if !resp.status().is_success() {
            anyhow::bail!("GET /session returned {}", resp.status());
        }

        let sessions: Vec<OcSession> = resp
            .json()
            .await
            .context("Failed to parse GET /session response")?;
        Ok(sessions)
    }

    /// GET /session/status — map of session_id → status
    /// Returns empty map if no sessions are actively running.
    pub async fn get_session_statuses(&self) -> Result<HashMap<String, OcSessionStatus>> {
        let url = format!("{}/session/status", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        if !resp.status().is_success() {
            anyhow::bail!("GET /session/status returned {}", resp.status());
        }

        let statuses: HashMap<String, OcSessionStatus> = resp
            .json()
            .await
            .context("Failed to parse GET /session/status response")?;
        Ok(statuses)
    }

    /// GET /session/:id
    pub async fn get_session(&self, session_id: &str) -> Result<OcSession> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        if !resp.status().is_success() {
            anyhow::bail!("GET /session/{} returned {}", session_id, resp.status());
        }

        let session: OcSession = resp
            .json()
            .await
            .context("Failed to parse session response")?;
        Ok(session)
    }

    /// POST /session/:id/message — send a message to a session
    pub async fn send_message(&self, session_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/session/{}/message", self.base_url, session_id);
        let body = serde_json::json!({
            "parts": [{"type": "text", "text": text}]
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {} failed", url))?;

        if !resp.status().is_success() {
            anyhow::bail!("POST /session/{}/message returned {}", session_id, resp.status());
        }
        Ok(())
    }

    /// POST /session/:id/abort — abort active session
    pub async fn abort_session(&self, session_id: &str) -> Result<()> {
        let url = format!("{}/session/{}/abort", self.base_url, session_id);
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {} failed", url))?;

        if !resp.status().is_success() {
            anyhow::bail!("POST /session/{}/abort returned {}", session_id, resp.status());
        }
        Ok(())
    }

    /// PATCH /session/:id — update session metadata (used for permission approval)
    /// For permission approval, sends: {"permission": [{"action": "allow"}]} or similar
    pub async fn update_session(&self, session_id: &str, patch: &serde_json::Value) -> Result<()> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let resp = self
            .client
            .patch(&url)
            .json(patch)
            .send()
            .await
            .with_context(|| format!("PATCH {} failed", url))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("PATCH /session/{} returned {}: {}", session_id, status, body);
        }
        Ok(())
    }

    /// GET /event — subscribe to SSE stream.
    /// Returns a channel receiver for events; SSE reconnects are handled internally.
    pub async fn subscribe_events(&self) -> Result<tokio::sync::mpsc::Receiver<OcEvent>> {
        use reqwest_eventsource::{Event, EventSource};

        let url = format!("{}/event", self.base_url);
        let request = self.client.get(&url);
        let mut es = EventSource::new(request).context("Failed to create EventSource")?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Message(msg)) => {
                        let oc_event = OcEvent {
                            event_type: if msg.event.is_empty() { "message".to_string() } else { msg.event },
                            data: msg.data,
                        };
                        if tx.send(oc_event).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Ok(Event::Open) => {
                        tracing::debug!("SSE stream opened");
                    }
                    Err(e) => {
                        tracing::warn!("SSE error: {}", e);
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Derive SessionState from status map + session list.
    /// The /session/status endpoint is sparse (empty when idle).
    pub fn session_state_from_status(
        session_id: &str,
        statuses: &HashMap<String, OcSessionStatus>,
    ) -> SessionState {
        match statuses.get(session_id) {
            Some(s) => match s.status.as_deref() {
                Some(status_str) => SessionState::from_oc_str(status_str),
                None => SessionState::Idle,
            },
            None => SessionState::Idle, // Not in status map = idle
        }
    }
}

// ─── Debug Helper ─────────────────────────────────────────────────────────────

/// Print session list from OC API as JSON (used by `ocwatch debug oc-client`)
pub async fn debug_query(base_url: &str) -> Result<()> {
    let client = OcClient::new(base_url)?;

    let sessions = client
        .list_sessions()
        .await
        .context("Failed to list sessions")?;

    let statuses = client.get_session_statuses().await.unwrap_or_default();

    let output: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let state = OcClient::session_state_from_status(&s.id, &statuses);
            serde_json::json!({
                "id": s.id,
                "title": s.title,
                "directory": s.directory,
                "state": format!("{}", state),
                "uptime_secs": s.uptime_secs(),
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
