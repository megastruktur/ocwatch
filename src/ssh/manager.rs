//! SSH connection manager using system SSH binary with ControlMaster.
//!
//! Design:
//! - Uses system `ssh` binary (NOT libssh2/russh) for ControlMaster compatibility
//! - ControlMaster socket path: /tmp/ocwatch-ctrl-{host_name} (short path, <104 chars macOS limit)
//! - Port forwarding: ssh -L local_port:localhost:remote_port
//! - Persistent connection: ControlPersist=yes with ServerAliveInterval=30

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::process::Command;

use crate::config::HostConfig;

/// Manages SSH connections to multiple remote hosts.
pub struct SshManager {
    connections: HashMap<String, SshConnection>,
}

struct SshConnection {
    host_config: HostConfig,
    control_path: PathBuf,
    /// Maps remote_port → (local_port, ssh_process_id)
    forwarded_ports: HashMap<u16, (u16, Option<u32>)>,
}

impl SshManager {
    pub fn new() -> Self {
        SshManager {
            connections: HashMap::new(),
        }
    }

    /// Control socket path for a host — kept short for macOS 104-byte limit.
    fn control_path(host_name: &str) -> PathBuf {
        PathBuf::from(format!(
            "/tmp/ocw-{}",
            &host_name[..host_name.len().min(20)]
        ))
    }

    /// Establish SSH ControlMaster connection to a host.
    /// This spawns a background SSH process that holds the connection open.
    pub async fn connect(&mut self, host: &HostConfig) -> Result<()> {
        let control_path = Self::control_path(&host.name);

        if self.is_connected_internal(host, &control_path).await {
            // Existing ControlMaster from a previous process — register it so exec() works.
            self.connections
                .entry(host.name.clone())
                .or_insert_with(|| SshConnection {
                    host_config: host.clone(),
                    control_path: control_path.clone(),
                    forwarded_ports: HashMap::new(),
                });
            return Ok(());
        }

        let ssh_target = host
            .ssh_target
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Host {} has no ssh_target", host.name))?;

        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={}", control_path.display()),
            "-o",
            "ControlPersist=yes",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-N",
        ]);

        if let Some(port) = host.ssh_port {
            cmd.args(["-p", &port.to_string()]);
        }

        if let Some(identity) = &host.ssh_identity {
            cmd.args(["-i", &identity.display().to_string()]);
        }

        cmd.arg(ssh_target);

        cmd.spawn()
            .with_context(|| format!("Failed to spawn SSH to {}", ssh_target))?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if control_path.exists() {
                break;
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!(
                    "SSH ControlMaster to {} did not connect within 10s",
                    ssh_target
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        self.connections.insert(
            host.name.clone(),
            SshConnection {
                host_config: host.clone(),
                control_path: control_path.clone(),
                forwarded_ports: HashMap::new(),
            },
        );

        tracing::info!("SSH connected to {} ({})", host.name, ssh_target);
        Ok(())
    }

    /// Check if the ControlMaster connection is alive.
    pub async fn is_connected(&self, host_name: &str) -> bool {
        if let Some(conn) = self.connections.get(host_name) {
            self.is_connected_internal(&conn.host_config, &conn.control_path)
                .await
        } else {
            false
        }
    }

    async fn is_connected_internal(&self, host: &HostConfig, control_path: &PathBuf) -> bool {
        if !control_path.exists() {
            return false;
        }
        let ssh_target = match host.ssh_target.as_deref() {
            Some(target) => target,
            None => return false,
        };
        let result = Command::new("ssh")
            .args([
                "-o",
                &format!("ControlPath={}", control_path.display()),
                "-O",
                "check",
                ssh_target,
            ])
            .output()
            .await;

        matches!(result, Ok(out) if out.status.success())
    }

    pub fn build_command_args(
        &self,
        host_name: &str,
        interactive: bool,
        remote_command: Option<&str>,
    ) -> Result<(String, Vec<String>)> {
        let conn = self
            .connections
            .get(host_name)
            .ok_or_else(|| anyhow::anyhow!("Not connected to {}", host_name))?;

        let ssh_target = conn
            .host_config
            .ssh_target
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No ssh_target for {}", host_name))?;

        let mut args = vec![
            "-o".to_string(),
            format!("ControlPath={}", conn.control_path.display()),
            "-o".to_string(),
            "ControlMaster=no".to_string(),
        ];

        if interactive {
            args.push("-t".to_string());
        }

        if let Some(port) = conn.host_config.ssh_port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }

        if let Some(identity) = &conn.host_config.ssh_identity {
            args.push("-i".to_string());
            args.push(identity.display().to_string());
        }

        args.push(ssh_target.to_string());

        if let Some(remote_command) = remote_command {
            args.push(remote_command.to_string());
        }

        Ok(("ssh".to_string(), args))
    }

    /// Execute a command on a remote host via existing ControlMaster connection.
    pub async fn exec(&self, host_name: &str, command: &str) -> Result<String> {
        let (program, args) = self.build_command_args(host_name, false, Some(command))?;

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            Command::new(&program).args(&args).output(),
        )
        .await
        .context("SSH exec timed out")?
        .with_context(|| format!("SSH exec failed on {}", host_name))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("SSH command failed on {}: {}", host_name, stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Set up SSH local port forwarding: local_port → remote:remote_port.
    /// Returns the local port to connect to.
    pub async fn forward_port(&mut self, host_name: &str, remote_port: u16) -> Result<u16> {
        let conn = self
            .connections
            .get_mut(host_name)
            .ok_or_else(|| anyhow::anyhow!("Not connected to {}", host_name))?;

        if let Some(&(local_port, _)) = conn.forwarded_ports.get(&remote_port) {
            return Ok(local_port);
        }

        let local_port = {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .context("Failed to bind to get free port")?;
            listener.local_addr()?.port()
        };

        let ssh_target = conn
            .host_config
            .ssh_target
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No ssh_target for {}", host_name))?;

        let child = Command::new("ssh")
            .args([
                "-o",
                &format!("ControlPath={}", conn.control_path.display()),
                "-o",
                "ControlMaster=no",
                "-L",
                &format!("{}:localhost:{}", local_port, remote_port),
                "-N",
                ssh_target,
            ])
            .spawn()
            .with_context(|| format!("Failed to spawn SSH port forward for {}", host_name))?;

        let child_pid = child.id();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        conn.forwarded_ports
            .insert(remote_port, (local_port, child_pid));

        tracing::info!(
            "Port forward established: localhost:{} → {}:{}",
            local_port,
            host_name,
            remote_port
        );

        Ok(local_port)
    }

    /// List remote ports currently forwarded for a host.
    pub fn forwarded_remote_ports(&self, host_name: &str) -> Vec<u16> {
        self.connections
            .get(host_name)
            .map(|conn| conn.forwarded_ports.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Remove an SSH port forward.
    pub async fn unforward_port(&mut self, host_name: &str, remote_port: u16) {
        if let Some(conn) = self.connections.get_mut(host_name) {
            if let Some((_, pid)) = conn.forwarded_ports.remove(&remote_port) {
                if let Some(pid) = pid {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                    tracing::debug!("Killed port forward SSH process {}", pid);
                }
            }
        }
    }

    /// Disconnect from a host, cleaning up all port forwards and the ControlMaster.
    pub async fn disconnect(&mut self, host_name: &str) {
        if let Some(conn) = self.connections.remove(host_name) {
            for (_, (_, pid)) in conn.forwarded_ports {
                if let Some(pid) = pid {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }

            if let Some(ssh_target) = conn.host_config.ssh_target.as_deref() {
                let _ = Command::new("ssh")
                    .args([
                        "-o",
                        &format!("ControlPath={}", conn.control_path.display()),
                        "-O",
                        "exit",
                        ssh_target,
                    ])
                    .output()
                    .await;
            }

            let _ = std::fs::remove_file(&conn.control_path);
            tracing::info!("Disconnected from {}", host_name);
        }
    }

    /// Disconnect from all hosts.
    pub async fn disconnect_all(&mut self) {
        let host_names: Vec<String> = self.connections.keys().cloned().collect();
        for host_name in host_names {
            self.disconnect(&host_name).await;
        }
    }
}

impl Default for SshManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Debug Helper ─────────────────────────────────────────────────────────────

/// Check SSH connection to a host and print status (for debug ssh-check command).
pub async fn debug_ssh_check(host_name: &str, config: &crate::config::Config) -> Result<()> {
    let host = config
        .hosts
        .iter()
        .find(|h| h.name == host_name)
        .ok_or_else(|| anyhow::anyhow!("Host '{}' not found in config", host_name))?;

    if host.ssh_target.is_none() {
        println!(r#"{{"connected": true, "reason": "local host (no SSH needed)"}}"#);
        return Ok(());
    }

    let mut manager = SshManager::new();

    println!(
        "Connecting to {} ({})...",
        host.name,
        host.ssh_target.as_deref().unwrap_or("?")
    );

    match manager.connect(host).await {
        Ok(()) => match manager.exec(host_name, "echo ok").await {
            Ok(output) => {
                println!(
                    r#"{{"connected": true, "exec_test": "{}", "control_path": "/tmp/ocw-{}"}}"#,
                    output.trim(),
                    &host.name[..host.name.len().min(20)]
                );
            }
            Err(err) => {
                println!(r#"{{"connected": false, "error": "exec failed: {}"}}"#, err);
            }
        },
        Err(err) => {
            println!(r#"{{"connected": false, "error": "{}"}}"#, err);
        }
    }

    manager.disconnect_all().await;
    Ok(())
}
