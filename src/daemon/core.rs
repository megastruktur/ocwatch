use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;

use crate::config::Config;
use crate::daemon::bell::BellNotifier;
use crate::discovery::{local, remote, ActiveSession, ScanResult};
use crate::ipc::{self, BroadcastTx, ClientMessage, DaemonMessage};
use crate::opencode::client::OcClient;
use crate::ssh::SshManager;
use crate::types::{HostStatus, SessionInfo, SessionState};

/// Internal runtime state for a session (NOT serialized).
struct SessionRuntime {
    last_state: SessionState,
    oc_base_url: String,
}

pub struct DaemonCore {
    config: Config,
    ssh_manager: SshManager,
    sessions: HashMap<String, SessionInfo>,
    session_runtime: HashMap<String, SessionRuntime>,
    hosts: HashMap<String, HostStatus>,
    broadcast_tx: BroadcastTx,
    started_at: Instant,
    bell_notifier: BellNotifier,
}

impl DaemonCore {
    pub fn new(config: Config) -> Self {
        let (broadcast_tx, _) = ipc::new_broadcast();
        DaemonCore {
            config,
            ssh_manager: SshManager::new(),
            sessions: HashMap::new(),
            session_runtime: HashMap::new(),
            hosts: HashMap::new(),
            broadcast_tx,
            started_at: Instant::now(),
            bell_notifier: BellNotifier::new(),
        }
    }

    /// Main daemon run loop. Does not return until signal received.
    pub async fn run(mut self) -> Result<()> {
        let socket_path = crate::daemon::lifecycle::socket_path();
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind Unix socket: {:?}", socket_path))?;

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set socket permissions")?;

        // Initial scan of all hosts
        self.scan_all_hosts().await;

        let mut poll_interval = tokio::time::interval(Duration::from_secs(
            self.config.poll_interval_secs as u64,
        ));

        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .context("Failed to register SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )
        .context("Failed to register SIGINT handler")?;

        tracing::info!("Daemon event loop started");

        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received, shutting down");
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("SIGINT received, shutting down");
                    break;
                }
                _ = poll_interval.tick() => {
                    self.scan_all_hosts().await;
                    self.broadcast_snapshot();
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            self.handle_client(stream).await;
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
            }
        }

        self.ssh_manager.disconnect_all().await;
        tracing::info!("Daemon core stopped");
        Ok(())
    }

    async fn scan_all_hosts(&mut self) {
        let local_scan = local::scan_local().await;
        self.update_from_scan("local", &local_scan, true, None)
            .await;

        let host_configs: Vec<_> = self
            .config
            .hosts
            .iter()
            .filter(|h| h.ssh_target.is_some())
            .cloned()
            .collect();

        for host_config in host_configs {
            let host_name = host_config.name.clone();

            if !self.ssh_manager.is_connected(&host_name).await {
                if let Err(e) = self.ssh_manager.connect(&host_config).await {
                    tracing::warn!("SSH connect to {} failed: {}", host_name, e);
                    self.mark_host_unreachable(&host_name, e.to_string());
                    continue;
                }
            }

            let scan = remote::scan_remote_v2(&mut self.ssh_manager, &host_name).await;
            self.update_from_scan(&host_name, &scan, true, None)
                .await;
        }
    }

    fn mark_host_unreachable(&mut self, host_name: &str, error: String) {
        self.remove_host_sessions(host_name);
        self.hosts.insert(
            host_name.to_string(),
            HostStatus {
                name: host_name.to_string(),
                connected: false,
                session_count: 0,
                last_poll_unix_ms: None,
                error: Some(error),
            },
        );
    }

    fn remove_host_sessions(&mut self, host_name: &str) {
        let prefix = format!("{}:", host_name);
        self.sessions.retain(|key, _| !key.starts_with(&prefix));
        self.session_runtime.retain(|key, _| !key.starts_with(&prefix));
    }

    async fn update_from_scan(
        &mut self,
        host: &str,
        scan: &ScanResult,
        connected: bool,
        error: Option<String>,
    ) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut seen_keys: HashSet<String> = HashSet::new();

        let statuses = if let Some(port) = scan.server_port {
            let base_url = format!("http://localhost:{}", port);
            match OcClient::new(&base_url) {
                Ok(client) => client.get_session_statuses().await.ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        for active in &scan.active_sessions {
            let inferred_state = active.inferred_state.clone();
            let state = match &statuses {
                Some(statuses) => statuses
                    .get(&active.session_id)
                    .and_then(|status| status.status.as_deref())
                    .map(SessionState::from_oc_str)
                    .or(inferred_state.clone())
                    .unwrap_or(SessionState::Idle),
                None => inferred_state.unwrap_or(SessionState::Disconnected),
            };
            let key = format!("{}:{}", host, active.session_id);
            seen_keys.insert(key.clone());

            let oc_port = scan.server_port.unwrap_or(0);
            let oc_base_url = format!("http://localhost:{}", oc_port);

            let activity_age_secs = {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                now.saturating_sub(active.time_updated_ms) / 1000
            };

            let info = SessionInfo {
                id: active.session_id.clone(),
                host: host.to_string(),
                state: state.clone(),
                title: active.title.clone(),
                working_dir: active.directory.clone(),
                activity_age_secs,
                oc_port,
                tmux_session: active.tmux_session.clone(),
                tmux_window: active.tmux_window.clone(),
                tmux_pane: active.tmux_pane_index.map(|i| i.to_string()),
            };

            let prev_state = self
                .session_runtime
                .get(&key)
                .map(|r| r.last_state.clone());

            if let Some(prev) = prev_state {
                let reason = match &state {
                    SessionState::Idle => "idle",
                    SessionState::WaitingForPermission => "permission",
                    SessionState::WaitingForInput => "input",
                    SessionState::Error => "error",
                    _ => "attention",
                };
                if self.bell_notifier.should_bell(&key, &prev, &state) {
                    self.bell_notifier.fire_bell(&info, reason);
                    let _ = self.broadcast_tx.send(DaemonMessage::Bell {
                        session_id: info.id.clone(),
                        host: info.host.clone(),
                        reason: reason.to_string(),
                    });
                }
            }

            self.session_runtime.insert(
                key.clone(),
                SessionRuntime {
                    last_state: state.clone(),
                    oc_base_url,
                },
            );

            if self
                .sessions
                .get(&key)
                .map(|s| s.state != state)
                .unwrap_or(true)
            {
                let _ = self.broadcast_tx.send(DaemonMessage::SessionUpdated {
                    session: info.clone(),
                });
            }

            self.sessions.insert(key, info);
        }

        self.cleanup_stale_sessions(host, &seen_keys);

        self.hosts.insert(
            host.to_string(),
            HostStatus {
                name: host.to_string(),
                connected,
                session_count: scan.active_sessions.len(),
                last_poll_unix_ms: Some(now_ms),
                error,
            },
        );
    }

    fn cleanup_stale_sessions(&mut self, host: &str, seen_keys: &HashSet<String>) {
        let prefix = format!("{}:", host);
        self.sessions
            .retain(|key, _| !key.starts_with(&prefix) || seen_keys.contains(key));
        self.session_runtime
            .retain(|key, _| !key.starts_with(&prefix) || seen_keys.contains(key));
    }



    /// Broadcast a full state snapshot to all connected clients.
    fn broadcast_snapshot(&self) {
        let sessions: Vec<SessionInfo> = self.sessions.values().cloned().collect();
        let hosts: Vec<HostStatus> = self.hosts.values().cloned().collect();
        let _ = self
            .broadcast_tx
            .send(DaemonMessage::StateSnapshot { sessions, hosts });
    }

    /// Build a DaemonStatus response.
    fn build_status(&self) -> DaemonMessage {
        DaemonMessage::DaemonStatus {
            running: true,
            pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            socket: crate::daemon::lifecycle::socket_path()
                .display()
                .to_string(),
            hosts: self.hosts.values().cloned().collect(),
            sessions: self.sessions.values().cloned().collect(),
        }
    }

    /// Handle a single client connection (simplified: synchronous read-then-close).
    /// For v1: client sends a message, daemon responds, then client may stay subscribed.
    async fn handle_client(&mut self, stream: tokio::net::UnixStream) {
        use tokio::io::{BufReader};

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let msg: Option<ClientMessage> = match ipc::read_message(&mut reader).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Invalid client message: {}", e);
                return;
            }
        };

        let msg = match msg {
            Some(m) => m,
            None => return,
        };

        match msg {
            ClientMessage::GetStatus => {
                let status = self.build_status();
                let _ = ipc::send_message(&mut write_half, &status).await;
            }
            ClientMessage::Subscribe => {
                let snapshot = DaemonMessage::StateSnapshot {
                    sessions: self.sessions.values().cloned().collect(),
                    hosts: self.hosts.values().cloned().collect(),
                };
                let _ = ipc::send_message(&mut write_half, &snapshot).await;

                let mut rx = self.broadcast_tx.subscribe();
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(msg) => {
                                if ipc::send_message(&mut write_half, &msg).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
            ClientMessage::InjectEvent { session_id, state } => {
                let new_state = SessionState::from_oc_str(&state);
                let key = self
                    .sessions
                    .keys()
                    .find(|k| k.ends_with(&session_id))
                    .cloned();

                if let Some(key) = key {
                    if let Some(info) = self.sessions.get_mut(&key) {
                        let old_state = info.state.clone();
                        info.state = new_state.clone();

                        let reason = match &new_state {
                            SessionState::Idle => "idle",
                            SessionState::WaitingForPermission => "permission",
                            SessionState::WaitingForInput => "input",
                            SessionState::Error => "error",
                            _ => "attention",
                        };
                        if self.bell_notifier.should_bell(&key, &old_state, &new_state) {
                            if let Some(session) = self.sessions.get(&key) {
                                self.bell_notifier.fire_bell(session, reason);
                                let _ = self.broadcast_tx.send(DaemonMessage::Bell {
                                    session_id: session.id.clone(),
                                    host: session.host.clone(),
                                    reason: reason.to_string(),
                                });
                            }
                        }

                        let _ = self.broadcast_tx.send(DaemonMessage::SessionUpdated {
                            session: self.sessions[&key].clone(),
                        });
                    }
                }

                let _ = ipc::send_message(
                    &mut write_half,
                    &DaemonMessage::Error {
                        message: "inject-event acknowledged".to_string(),
                    },
                )
                .await;
            }
            ClientMessage::Approve { session_id } => {
                let oc_url = self
                    .session_runtime
                    .iter()
                    .find(|(k, _)| k.ends_with(&session_id))
                    .map(|(_, rt)| rt.oc_base_url.clone());

                if let Some(url) = oc_url {
                    match OcClient::new(&url) {
                        Ok(client) => {
                            let patch = serde_json::json!({"permission": [{"action": "allow"}]});
                            let response = match client.update_session(&session_id, &patch).await {
                                Ok(_) => DaemonMessage::Error {
                                    message: format!("Approved: {}", session_id),
                                },
                                Err(e) => DaemonMessage::Error {
                                    message: format!("Approve failed: {}", e),
                                },
                            };
                            let _ = ipc::send_message(&mut write_half, &response).await;
                        }
                        Err(e) => {
                            let _ = ipc::send_message(
                                &mut write_half,
                                &DaemonMessage::Error {
                                    message: format!("Approve failed: {}", e),
                                },
                            )
                            .await;
                        }
                    }
                }
            }
            ClientMessage::RefreshAll => {
                self.scan_all_hosts().await;
                self.broadcast_snapshot();
            }
            ClientMessage::Shutdown => {
                tracing::info!("Shutdown requested via IPC");
                let _ = ipc::send_message(
                    &mut write_half,
                    &DaemonMessage::Error {
                        message: "Shutting down".to_string(),
                    },
                )
                .await;
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(std::process::id() as i32), Signal::SIGTERM);
            }
            _ => {}
        }
    }
}
