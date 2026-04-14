//! Remote host scanner for OpenCode processes.
//!
//! IMPORTANT: tmux is NOT available on the remote megaserver.
//! Discovery uses: ps aux + lsof (NOT tmux list-panes).
//! SSH port-forwarding is set up for each discovered OC instance.

use anyhow::Result;
use crate::discovery::{DiscoveredInstance, discover_port_from_lsof, discover_port_from_proc_net_tcp};
use crate::ssh::SshManager;
use std::collections::HashSet;

/// Represents a raw process entry from `ps aux` on remote
#[derive(Debug)]
struct RemoteProcess {
    pid: u32,
    cmdline: String,
}

/// Scan a remote host for OpenCode processes.
/// Uses ps aux + lsof over SSH (NOT tmux, which isn't available on all hosts).
/// Sets up SSH port forwards for each discovered OC instance.
/// Returns discovered instances with LOCAL forwarded ports.
pub async fn scan_remote(
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> Vec<DiscoveredInstance> {
    match try_scan_remote(ssh_manager, host_name).await {
        Ok(instances) => instances,
        Err(e) => {
            tracing::warn!("Remote scan failed for {}: {}", host_name, e);
            vec![]
        }
    }
}

async fn try_scan_remote(
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> Result<Vec<DiscoveredInstance>> {
    let previous_remote_ports = ssh_manager.forwarded_remote_ports(host_name);

    // Find OpenCode processes on remote using ps aux
    let ps_output = ssh_manager.exec(host_name, "ps aux").await?;

    let oc_processes: Vec<RemoteProcess> = ps_output
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("opencode") || lower.contains(".opencode/bin")
        })
        .filter(|line| !line.contains("grep") && !line.contains("ps aux"))
        .filter_map(parse_ps_line)
        .collect();

    tracing::debug!("Found {} opencode processes on {}", oc_processes.len(), host_name);

    let mut instances = Vec::new();
    let mut current_remote_ports = HashSet::new();

    for proc in oc_processes {
        // Try to discover port via lsof
        let lsof_cmd = format!("lsof -p {} -i -P -n 2>/dev/null || true", proc.pid);
        let lsof_output = ssh_manager.exec(host_name, &lsof_cmd).await.unwrap_or_default();

        let remote_port = if let Some(port) = discover_port_from_lsof(&lsof_output) {
            port
        } else if let Some(port) = extract_port_from_cmdline(&proc.cmdline) {
            port
        } else {
            // Fallback: /proc/net/tcp (Linux only)
            let proc_tcp = ssh_manager
                .exec(
                    host_name,
                    &format!(
                        "cat /proc/{}/net/tcp 2>/dev/null || cat /proc/net/tcp 2>/dev/null || true",
                        proc.pid
                    ),
                )
                .await
                .unwrap_or_default();

            if let Some(port) = discover_port_from_proc_net_tcp(&proc_tcp, proc.pid) {
                port
            } else {
                tracing::warn!(
                    "Could not discover port for remote OC PID {}, skipping",
                    proc.pid
                );
                continue;
            }
        };

        current_remote_ports.insert(remote_port);

        // Set up SSH port forward: local_port → remote_host:remote_port
        let local_port = match ssh_manager.forward_port(host_name, remote_port).await {
            Ok(port) => port,
            Err(e) => {
                tracing::warn!("Failed to forward port {} for remote OC: {}", remote_port, e);
                continue;
            }
        };

        tracing::info!(
            "Remote OC on {}: PID={} remote_port={} → local_port={}",
            host_name,
            proc.pid,
            remote_port,
            local_port
        );

        instances.push(DiscoveredInstance {
            pid: proc.pid,
            port: local_port,
            remote_port: Some(remote_port),
            tmux_session: None,
            tmux_window: None,
            tmux_window_index: None,
            tmux_pane_index: None,
            tmux_pane_tty: None,
        });
    }

    let current_ports: Vec<u16> = current_remote_ports.into_iter().collect();
    reconcile_port_forwards(ssh_manager, host_name, &current_ports, &previous_remote_ports).await;

    Ok(instances)
}

/// Parse a `ps aux` output line to extract PID and cmdline.
/// Format: USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND
fn parse_ps_line(line: &str) -> Option<RemoteProcess> {
    let parts: Vec<&str> = line.split_whitespace().collect();

    if parts.first() == Some(&"USER") {
        return None;
    }

    if parts.len() < 11 {
        return None;
    }

    let pid: u32 = parts.get(1)?.parse().ok()?;
    let cmdline = parts[10..].join(" ");

    Some(RemoteProcess { pid, cmdline })
}

/// Extract --port N or --port=N from a command line string.
fn extract_port_from_cmdline(cmdline: &str) -> Option<u16> {
    let parts: Vec<&str> = cmdline.split_whitespace().collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "--port" {
            return parts.get(i + 1)?.parse().ok();
        }
        if let Some(port_str) = part.strip_prefix("--port=") {
            return port_str.parse().ok();
        }
    }
    None
}

/// Reconcile port forwards: unforward ports for instances that no longer exist.
/// `current`: newly discovered instance ports
/// `previous_ports`: remote ports that were previously forwarded
pub async fn reconcile_port_forwards(
    ssh_manager: &mut SshManager,
    host_name: &str,
    current_remote_ports: &[u16],
    previous_remote_ports: &[u16],
) {
    for &old_port in previous_remote_ports {
        if !current_remote_ports.contains(&old_port) {
            tracing::info!("Cleaning up stale port forward {} for {}", old_port, host_name);
            ssh_manager.unforward_port(host_name, old_port).await;
        }
    }
}
