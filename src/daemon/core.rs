use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::{mpsc::{self, UnboundedReceiver, UnboundedSender}, oneshot};

use crate::config::Config;
use crate::daemon::bell::BellNotifier;
use crate::daemon::recent_dirs::RecentDirStore;
use crate::discovery::{local, remote, ActiveSession, ScanResult};
use crate::ipc::{self, AttachSpec, BroadcastTx, ClientMessage, DaemonMessage, RecentDirEntry};
use crate::opencode::client::OcClient;
use crate::ssh::SshManager;
use crate::types::{HostStatus, SessionInfo, SessionState};

/// Internal runtime state for a session (NOT serialized).
struct SessionRuntime {
    last_state: SessionState,
    oc_base_url: String,
}

enum ScanTarget {
    Local,
    Remote(String),
}

enum ScanWorkerCommand {
    StartCycle,
}

enum ScanWorkerEvent {
    HostScanned(ResolvedHostScan),
    CycleComplete,
}

struct ResolvedHostScan {
    host: String,
    connected: bool,
    error: Option<String>,
    completed_unix_ms: u64,
    sessions: Vec<ResolvedSession>,
}

struct ResolvedSession {
    info: SessionInfo,
    oc_base_url: String,
    last_seen_unix_ms: u64,
}

pub struct DaemonCore {
    config: Config,
    ssh_manager: SshManager,
    sessions: HashMap<String, SessionInfo>,
    session_runtime: HashMap<String, SessionRuntime>,
    hosts: HashMap<String, HostStatus>,
    scan_in_progress: bool,
    rescan_requested: bool,
    active_scan_generation: u64,
    completed_scan_generation: u64,
    pending_refresh_responders: Vec<(u64, oneshot::Sender<DaemonMessage>)>,
    broadcast_tx: BroadcastTx,
    started_at: Instant,
    bell_notifier: BellNotifier,
    recent_dirs: RecentDirStore,
    recent_dir_cache_path: PathBuf,
}

impl DaemonCore {
    pub fn new(config: Config) -> Self {
        let (broadcast_tx, _) = ipc::new_broadcast();
        let recent_dir_cache_path = crate::daemon::lifecycle::data_dir().join("recent_dirs.json");
        let recent_dirs = RecentDirStore::load(&recent_dir_cache_path).unwrap_or_default();
        DaemonCore {
            config,
            ssh_manager: SshManager::new(),
            sessions: HashMap::new(),
            session_runtime: HashMap::new(),
            hosts: HashMap::new(),
            scan_in_progress: false,
            rescan_requested: false,
            active_scan_generation: 0,
            completed_scan_generation: 0,
            pending_refresh_responders: Vec::new(),
            broadcast_tx,
            started_at: Instant::now(),
            bell_notifier: BellNotifier::new(),
            recent_dirs,
            recent_dir_cache_path,
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

        self.ensure_host_placeholders();
        let (mut scan_cmd_tx, mut scan_event_rx, mut scan_worker) = spawn_scan_worker(self.config.clone());

        let mut poll_interval = tokio::time::interval(Duration::from_secs(
            self.config.poll_interval_secs as u64,
        ));
        poll_interval.tick().await;
        let _ = self.request_scan_cycle(&scan_cmd_tx);

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
                biased;
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received, shutting down");
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("SIGINT received, shutting down");
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            self.handle_client(stream, &scan_cmd_tx).await;
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                event = scan_event_rx.recv() => {
                    match event {
                        Some(ScanWorkerEvent::HostScanned(scan)) => {
                            self.apply_scan_result(scan);
                            self.broadcast_snapshot();
                        }
                        Some(ScanWorkerEvent::CycleComplete) => {
                            self.scan_in_progress = false;
                            self.completed_scan_generation = self.active_scan_generation;
                            self.flush_refresh_responders();

                            if self.rescan_requested {
                                self.rescan_requested = false;
                                let _ = self.request_scan_cycle(&scan_cmd_tx);
                            }
                        }
                        None => {
                            tracing::error!("Scan worker stopped unexpectedly; restarting");
                            let should_resume_scan = self.scan_in_progress
                                || self.rescan_requested
                                || !self.pending_refresh_responders.is_empty();
                            self.fail_refresh_responders("Refresh aborted because the scan worker stopped");
                            self.scan_in_progress = false;
                            let (new_tx, new_rx, new_worker) = spawn_scan_worker(self.config.clone());
                            scan_cmd_tx = new_tx;
                            scan_event_rx = new_rx;
                            scan_worker = new_worker;

                            if should_resume_scan {
                                self.rescan_requested = false;
                                let _ = self.request_scan_cycle(&scan_cmd_tx);
                            }
                        }
                    }
                }
                _ = poll_interval.tick() => {
                    let _ = self.request_scan_cycle(&scan_cmd_tx);
                }
            }
        }

        self.ssh_manager.disconnect_all().await;
        drop(scan_cmd_tx);
        let _ = scan_worker.await;
        tracing::info!("Daemon core stopped");
        Ok(())
    }

    fn request_scan_cycle(&mut self, scan_cmd_tx: &UnboundedSender<ScanWorkerCommand>) -> bool {
        self.ensure_host_placeholders();

        if self.scan_in_progress {
            self.rescan_requested = true;
            return true;
        }

        self.scan_in_progress = true;
        self.active_scan_generation += 1;

        if scan_cmd_tx.send(ScanWorkerCommand::StartCycle).is_err() {
            self.scan_in_progress = false;
            tracing::error!("Failed to schedule scan cycle: scan worker channel closed");
            return false;
        }

        true
    }

    fn schedule_refresh(
        &mut self,
        scan_cmd_tx: &UnboundedSender<ScanWorkerCommand>,
        responder: oneshot::Sender<DaemonMessage>,
    ) {
        let target_generation = if self.scan_in_progress {
            self.rescan_requested = true;
            self.active_scan_generation + 1
        } else {
            if !self.request_scan_cycle(scan_cmd_tx) {
                let _ = responder.send(DaemonMessage::Error {
                    message: "Failed to start refresh scan".to_string(),
                });
                return;
            }
            self.active_scan_generation
        };

        self.pending_refresh_responders
            .push((target_generation, responder));
    }

    fn ensure_host_placeholders(&mut self) {
        self.hosts.entry("local".to_string()).or_insert(HostStatus {
            name: "local".to_string(),
            connected: false,
            session_count: 0,
            last_poll_unix_ms: None,
            error: None,
        });

        for host_name in self.configured_remote_hosts() {
            self.hosts.entry(host_name.clone()).or_insert(HostStatus {
                name: host_name,
                connected: false,
                session_count: 0,
                last_poll_unix_ms: None,
                error: None,
            });
        }
    }

    fn remove_host_sessions(&mut self, host_name: &str) {
        let prefix = format!("{}:", host_name);
        self.sessions.retain(|key, _| !key.starts_with(&prefix));
        self.session_runtime.retain(|key, _| !key.starts_with(&prefix));
    }

    fn apply_scan_result(&mut self, scan: ResolvedHostScan) {
        let mut seen_keys: HashSet<String> = HashSet::new();

        for resolved in scan.sessions {
            self.record_recent_dir(&scan.host, &resolved.info.working_dir, resolved.last_seen_unix_ms);
            let state = resolved.info.state.clone();
            let key = format!("{}:{}", scan.host, resolved.info.id);
            seen_keys.insert(key.clone());

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
                    self.bell_notifier.fire_bell(&resolved.info, reason);
                    let _ = self.broadcast_tx.send(DaemonMessage::Bell {
                        session_id: resolved.info.id.clone(),
                        host: resolved.info.host.clone(),
                        reason: reason.to_string(),
                    });
                }
            }

            self.session_runtime.insert(
                key.clone(),
                SessionRuntime {
                    last_state: state.clone(),
                    oc_base_url: resolved.oc_base_url.clone(),
                },
            );

            if self
                .sessions
                .get(&key)
                .map(|s| s.state != state)
                .unwrap_or(true)
            {
                let _ = self.broadcast_tx.send(DaemonMessage::SessionUpdated {
                    session: resolved.info.clone(),
                });
            }

            self.sessions.insert(key, resolved.info);
        }

        self.cleanup_stale_sessions(&scan.host, &seen_keys);

        self.hosts.insert(
            scan.host.to_string(),
            HostStatus {
                name: scan.host,
                connected: scan.connected,
                session_count: seen_keys.len(),
                last_poll_unix_ms: Some(scan.completed_unix_ms),
                error: scan.error,
            },
        );

        self.persist_recent_dirs();
    }

    fn cleanup_stale_sessions(&mut self, host: &str, seen_keys: &HashSet<String>) {
        let prefix = format!("{}:", host);
        self.sessions
            .retain(|key, _| !key.starts_with(&prefix) || seen_keys.contains(key));
        self.session_runtime
            .retain(|key, _| !key.starts_with(&prefix) || seen_keys.contains(key));
    }

    fn flush_refresh_responders(&mut self) {
        let snapshot = self.state_snapshot();
        let mut remaining = Vec::new();

        for (generation, responder) in self.pending_refresh_responders.drain(..) {
            if generation <= self.completed_scan_generation {
                let _ = responder.send(snapshot.clone());
            } else {
                remaining.push((generation, responder));
            }
        }

        self.pending_refresh_responders = remaining;
    }

    fn fail_refresh_responders(&mut self, message: &str) {
        for (_, responder) in self.pending_refresh_responders.drain(..) {
            let _ = responder.send(DaemonMessage::Error {
                message: message.to_string(),
            });
        }
    }



    /// Broadcast a full state snapshot to all connected clients.
    fn broadcast_snapshot(&self) {
        let _ = self.broadcast_tx.send(self.state_snapshot());
    }

    fn state_snapshot(&self) -> DaemonMessage {
        let sessions: Vec<SessionInfo> = self.sessions.values().cloned().collect();
        let hosts: Vec<HostStatus> = self.hosts.values().cloned().collect();
        DaemonMessage::StateSnapshot { sessions, hosts }
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

    fn persist_recent_dirs(&self) {
        if let Err(error) = self.recent_dirs.save(&self.recent_dir_cache_path) {
            tracing::warn!("Failed to persist recent dirs: {}", error);
        }
    }

    fn record_recent_dir(&mut self, host: &str, directory: &str, last_seen_unix_ms: u64) {
        if directory.trim().is_empty() {
            return;
        }
        self.recent_dirs.upsert(RecentDirEntry {
            host: host.to_string(),
            directory: directory.to_string(),
            last_seen_unix_ms,
        });
    }

    fn configured_remote_hosts(&self) -> Vec<String> {
        self.config
            .hosts
            .iter()
            .filter(|host| host.ssh_target.is_some())
            .map(|host| host.name.clone())
            .collect()
    }

    fn has_missing_recent_dir_hosts(&self) -> bool {
        if !self.recent_dirs.has_host_entries("local") {
            return true;
        }

        self.configured_remote_hosts()
            .into_iter()
            .any(|host| !self.recent_dirs.has_host_entries(&host))
    }

    fn recent_dirs_response(&self, limit: usize, is_complete: bool) -> DaemonMessage {
        let entries = self.recent_dirs.entries(limit);
        DaemonMessage::RecentDirs {
            entries,
            is_complete,
        }
    }

    async fn hydrate_missing_recent_dirs(&mut self, limit: usize) -> bool {
        let mut updated = false;

        if !self.recent_dirs.has_host_entries("local") {
            let entries = local::recent_directories(limit).await;
            if !entries.is_empty() {
                self.recent_dirs.upsert_many(entries);
                updated = true;
            }
        }

        for host_name in self.configured_remote_hosts() {
            if self.recent_dirs.has_host_entries(&host_name) {
                continue;
            }

            if !self.ssh_manager.is_connected(&host_name).await {
                let Some(host_config) = self.config.hosts.iter().find(|host| host.name == host_name) else {
                    continue;
                };

                if let Err(error) = self.ssh_manager.connect(host_config).await {
                    tracing::warn!("SSH connect to {} failed during recent-dir hydration: {}", host_name, error);
                    continue;
                }
            }

            let entries = remote::recent_directories(&self.ssh_manager, &host_name, limit).await;
            if !entries.is_empty() {
                self.recent_dirs.upsert_many(entries);
                updated = true;
            }
        }

        if updated {
            self.persist_recent_dirs();
        }

        updated
    }

    async fn create_session(
        &mut self,
        host: &str,
        directory: &str,
        name_hint: Option<String>,
    ) -> Result<AttachSpec> {
        let now_ms = current_unix_ms();
        let name = self.next_session_name(host, directory, name_hint).await?;

        if host == "local" {
            let status = Command::new("tmux")
                .args(["new-session", "-d", "-s", &name, "-c", directory, "opencode"])
                .status()
                .await
                .with_context(|| format!("Failed to start tmux session '{}': {}", name, directory))?;

            if !status.success() {
                anyhow::bail!("tmux new-session failed for '{}': {}", name, directory);
            }
        } else {
            self.ensure_remote_connection(host).await?;
            let command = format!(
                "tmux new-session -d -s '{}' -c '{}' opencode",
                shell_escape(&name),
                shell_escape(directory)
            );
            self.ssh_manager
                .exec(host, &command)
                .await
                .with_context(|| format!("Failed to create remote session '{}' on {}", name, host))?;
        }

        self.record_recent_dir(host, directory, now_ms);
        self.persist_recent_dirs();
        self.attach_spec_for_new_session(host, &name).await
    }

    async fn drop_in_attach_spec(&mut self, session_id: &str) -> Result<AttachSpec> {
        let Some(session) = self
            .sessions
            .values()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            anyhow::bail!("Session not found: {}", session_id);
        };

        if session.host == "local" {
            let window = session.tmux_window.clone();
            let pane = session.tmux_pane.clone();
            let session_name = session
                .tmux_session
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No tmux info — cannot drop in"))?;
            Ok(AttachSpec::LocalTmux {
                session: session_name,
                window,
                pane,
            })
        } else if let Some(tmux_session) = session.tmux_session.clone() {
            let target = if let (Some(window), Some(pane)) = (session.tmux_window.clone(), session.tmux_pane.clone()) {
                format!("{}:{}.{}", tmux_session, window, pane)
            } else {
                tmux_session.clone()
            };
            self.attach_spec_for_remote_tmux(&session.host, &target)
                .await
        } else {
            self.attach_spec_for_remote_shell(&session.host).await
        }
    }

    async fn attach_spec_for_new_session(&mut self, host: &str, session: &str) -> Result<AttachSpec> {
        if host == "local" {
            return Ok(AttachSpec::LocalTmux {
                session: session.to_string(),
                window: None,
                pane: None,
            });
        }

        self.attach_spec_for_remote_tmux(host, session).await
    }

    async fn attach_spec_for_remote_tmux(&mut self, host: &str, target: &str) -> Result<AttachSpec> {
        self.ensure_remote_connection(host).await?;
        let remote_command = format!("tmux attach -t '{}'", shell_escape(target));
        let (program, args) = self
            .ssh_manager
            .build_command_args(host, true, Some(&remote_command))?;
        Ok(AttachSpec::Exec {
            program,
            args,
            tmux_window_name: Some(format!("{}:{}", host, display_name(target))),
        })
    }

    async fn attach_spec_for_remote_shell(&mut self, host: &str) -> Result<AttachSpec> {
        self.ensure_remote_connection(host).await?;
        let (program, args) = self.ssh_manager.build_command_args(host, true, None)?;
        Ok(AttachSpec::Exec {
            program,
            args,
            tmux_window_name: Some(host.to_string()),
        })
    }

    async fn ensure_remote_connection(&mut self, host: &str) -> Result<()> {
        if self.ssh_manager.is_connected(host).await {
            return Ok(());
        }

        let host_config = self
            .config
            .hosts
            .iter()
            .find(|cfg| cfg.name == host)
            .ok_or_else(|| anyhow::anyhow!("Host '{}' not found in config", host))?;

        self.ssh_manager.connect(host_config).await
    }

    async fn next_session_name(
        &mut self,
        host: &str,
        directory: &str,
        name_hint: Option<String>,
    ) -> Result<String> {
        let base_name = sanitize_session_name(
            name_hint
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| directory_basename(directory)),
        );

        let mut candidate = base_name.clone();
        let mut suffix = 2;
        while self.session_name_exists(host, &candidate).await? {
            candidate = format!("{}-{}", base_name, suffix);
            suffix += 1;
        }
        Ok(candidate)
    }

    async fn session_name_exists(&mut self, host: &str, name: &str) -> Result<bool> {
        if host == "local" {
            let status = Command::new("tmux")
                .args(["has-session", "-t", name])
                .status()
                .await
                .with_context(|| format!("Failed to check local tmux session '{}'", name))?;
            return Ok(status.success());
        }

        self.ensure_remote_connection(host).await?;
        let command = format!(
            "tmux has-session -t '{}' >/dev/null 2>&1 && printf yes || true",
            shell_escape(name)
        );
        let output = self.ssh_manager.exec(host, &command).await?;
        Ok(output.trim() == "yes")
    }

    /// Handle a single client connection (simplified: synchronous read-then-close).
    /// For v1: client sends a message, daemon responds, then client may stay subscribed.
    async fn handle_client(
        &mut self,
        stream: tokio::net::UnixStream,
        scan_cmd_tx: &UnboundedSender<ScanWorkerCommand>,
    ) {
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
                let (refresh_tx, refresh_rx) = oneshot::channel();
                self.schedule_refresh(scan_cmd_tx, refresh_tx);
                tokio::spawn(async move {
                    let response = match tokio::time::timeout(Duration::from_secs(30), refresh_rx).await {
                        Ok(Ok(snapshot)) => snapshot,
                        Ok(Err(_)) => DaemonMessage::Error {
                            message: "Refresh failed before a fresh snapshot was available".to_string(),
                        },
                        Err(_) => DaemonMessage::Error {
                            message: "Refresh timed out waiting for a fresh snapshot".to_string(),
                        },
                    };

                    let _ = ipc::send_message(&mut write_half, &response).await;
                });
                return;
            }
            ClientMessage::GetRecentDirs { limit } => {
                let missing_hosts = self.has_missing_recent_dir_hosts();
                let initial = self.recent_dirs_response(limit as usize, !missing_hosts);
                let _ = ipc::send_message(&mut write_half, &initial).await;

                if missing_hosts {
                    let _ = self.hydrate_missing_recent_dirs(limit as usize).await;
                    let final_update = self.recent_dirs_response(limit as usize, true);
                    let _ = ipc::send_message(&mut write_half, &final_update).await;
                }
            }
            ClientMessage::CreateSession {
                host,
                directory,
                name_hint,
            } => {
                let response = match self.create_session(&host, &directory, name_hint).await {
                    Ok(attach) => DaemonMessage::AttachReady { attach },
                    Err(error) => DaemonMessage::Error {
                        message: error.to_string(),
                    },
                };
                let _ = ipc::send_message(&mut write_half, &response).await;
            }
            ClientMessage::DropIn { session_id } => {
                let response = match self.drop_in_attach_spec(&session_id).await {
                    Ok(attach) => DaemonMessage::AttachReady { attach },
                    Err(error) => DaemonMessage::Error {
                        message: error.to_string(),
                    },
                };
                let _ = ipc::send_message(&mut write_half, &response).await;
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
        }
    }
}

async fn run_scan_worker(
    config: Config,
    mut command_rx: UnboundedReceiver<ScanWorkerCommand>,
    event_tx: UnboundedSender<ScanWorkerEvent>,
) {
    let mut ssh_manager = SshManager::new();

    while let Some(command) = command_rx.recv().await {
        match command {
            ScanWorkerCommand::StartCycle => {
                if event_tx
                    .send(ScanWorkerEvent::HostScanned(scan_local_host().await))
                    .is_err()
                {
                    break;
                }

                for host_name in config
                    .hosts
                    .iter()
                    .filter(|host| host.ssh_target.is_some())
                    .map(|host| host.name.clone())
                {
                    if event_tx
                        .send(ScanWorkerEvent::HostScanned(
                            scan_remote_host(&config, &mut ssh_manager, &host_name).await,
                        ))
                        .is_err()
                    {
                        return;
                    }
                }

                if event_tx.send(ScanWorkerEvent::CycleComplete).is_err() {
                    break;
                }
            }
        }
    }

    ssh_manager.disconnect_all().await;
}

fn spawn_scan_worker(
    config: Config,
) -> (
    UnboundedSender<ScanWorkerCommand>,
    UnboundedReceiver<ScanWorkerEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (scan_cmd_tx, scan_cmd_rx) = mpsc::unbounded_channel();
    let (scan_event_tx, scan_event_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        run_scan_worker(config, scan_cmd_rx, scan_event_tx).await;
    });

    (scan_cmd_tx, scan_event_rx, handle)
}

async fn scan_local_host() -> ResolvedHostScan {
    let scan = local::scan_local().await;
    resolve_host_scan("local", &scan, true, None).await
}

async fn scan_remote_host(
    config: &Config,
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> ResolvedHostScan {
    let Some(host_config) = config.hosts.iter().find(|host| host.name == host_name) else {
        return ResolvedHostScan {
            host: host_name.to_string(),
            connected: false,
            error: Some(format!("Host '{}' not found in config", host_name)),
            completed_unix_ms: current_unix_ms(),
            sessions: vec![],
        };
    };

    if !ssh_manager.is_connected(host_name).await {
        if let Err(error) = ssh_manager.connect(host_config).await {
            tracing::warn!("SSH connect to {} failed: {}", host_name, error);
            return ResolvedHostScan {
                host: host_name.to_string(),
                connected: false,
                error: Some(error.to_string()),
                completed_unix_ms: current_unix_ms(),
                sessions: vec![],
            };
        }
    }

    let scan = remote::scan_remote_v2(ssh_manager, host_name).await;
    resolve_host_scan(host_name, &scan, true, None).await
}

async fn resolve_host_scan(
    host: &str,
    scan: &ScanResult,
    connected: bool,
    error: Option<String>,
) -> ResolvedHostScan {
    let statuses = if let Some(port) = scan.server_port {
        let base_url = format!("http://localhost:{}", port);
        match OcClient::new(&base_url) {
            Ok(client) => client.get_session_statuses().await.ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    let sessions = scan
        .active_sessions
        .iter()
        .map(|active| resolve_active_session(host, active, scan.server_port, statuses.as_ref()))
        .collect();

    ResolvedHostScan {
        host: host.to_string(),
        connected,
        error,
        completed_unix_ms: current_unix_ms(),
        sessions,
    }
}

fn resolve_active_session(
    host: &str,
    active: &ActiveSession,
    server_port: Option<u16>,
    statuses: Option<&HashMap<String, crate::opencode::client::OcSessionStatus>>,
) -> ResolvedSession {
    let inferred_state = active.inferred_state.clone();
    let state = match statuses {
        Some(statuses) => {
            let api_state = statuses
                .get(&active.session_id)
                .and_then(|status| status.status.as_deref())
                .map(SessionState::from_oc_str);

            match api_state {
                Some(SessionState::Unknown) | None => {
                    inferred_state.unwrap_or(SessionState::Unknown)
                }
                Some(state) => state,
            }
        }
        None => inferred_state.unwrap_or(SessionState::Unknown),
    };

    let oc_port = server_port.unwrap_or(0);
    let oc_base_url = format!("http://localhost:{}", oc_port);
    let activity_age_secs = current_unix_ms().saturating_sub(active.time_updated_ms) / 1000;

    ResolvedSession {
        info: SessionInfo {
            id: active.session_id.clone(),
            host: host.to_string(),
            state,
            parent_id: active.parent_id.clone(),
            title: active.title.clone(),
            working_dir: active.directory.clone(),
            activity_age_secs,
            oc_port,
            tmux_session: active.tmux_session.clone(),
            tmux_window: active.tmux_window.clone(),
            tmux_pane: active.tmux_pane_index.map(|index| index.to_string()),
        },
        oc_base_url,
        last_seen_unix_ms: active.time_updated_ms,
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn directory_basename(directory: &str) -> &str {
    directory
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("opencode")
}

fn sanitize_session_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "opencode".to_string()
    } else {
        sanitized
    }
}

fn shell_escape(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn display_name(target: &str) -> String {
    target.split(':').next().unwrap_or(target).to_string()
}
