use anyhow::Result;
use crate::discovery::{ActiveSession, DiscoveredInstance, ScanResult, TmuxPane, discover_port_from_lsof};
use crate::ssh::SshManager;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
struct RemoteProcess {
    pid: u32,
    cmdline: String,
}

/// Scan a remote host for active OpenCode sessions using the new TUI-based detection.
/// Finds TUI processes → CWD → git project_id → sqlite3 query.
/// Also discovers server process port and sets up SSH port forward.
pub async fn scan_remote_v2(
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> ScanResult {
    match try_scan_remote_v2(ssh_manager, host_name).await {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("Remote scan failed for {}: {}", host_name, e);
            ScanResult {
                server_port: None,
                server_remote_port: None,
                active_sessions: vec![],
            }
        }
    }
}

async fn try_scan_remote_v2(
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> Result<ScanResult> {
    let previous_remote_ports = ssh_manager.forwarded_remote_ports(host_name);
    let tmux_panes_by_tty = fetch_remote_tmux_panes(ssh_manager, host_name)
        .await
        .into_iter()
        .map(|pane| (pane.pane_tty.clone(), pane))
        .collect::<HashMap<_, _>>();

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

    let mut server_port: Option<u16> = None;
    let mut server_remote_port: Option<u16> = None;
    let mut tui_pids: Vec<u32> = Vec::new();
    let mut current_remote_ports = HashSet::new();

    for proc in &oc_processes {
        let lsof_cmd = format!("lsof -a -p {} -i -P -n 2>/dev/null || true", proc.pid);
        let lsof_output = ssh_manager.exec(host_name, &lsof_cmd).await.unwrap_or_default();

        let remote_port = discover_port_from_lsof(&lsof_output)
            .or_else(|| extract_port_from_cmdline(&proc.cmdline))
            .or_else(|| {
                // /proc/net/tcp fallback handled below
                None
            });

        if let Some(rport) = remote_port {
            current_remote_ports.insert(rport);
            match ssh_manager.forward_port(host_name, rport).await {
                Ok(local_port) => {
                    server_port = Some(local_port);
                    server_remote_port = Some(rport);
                    tracing::info!(
                        "Remote OC server on {}: PID={} remote_port={} → local_port={}",
                        host_name, proc.pid, rport, local_port
                    );
                }
                Err(e) => {
                    tracing::warn!("Failed to forward port {} for {}: {}", rport, host_name, e);
                }
            }
        } else {
            // No reliable port signal → treat as a TUI process.
            // /proc/<pid>/net/tcp is network-namespace scoped on Linux, not process scoped,
            // so using it here misclassifies unrelated listeners as belonging to this PID.
            tui_pids.push(proc.pid);
        }
    }

    let mut active_sessions = Vec::new();
    let mut seen_session_ids = HashSet::new();

    for pid in &tui_pids {
        let cwd_cmd = format!(
            "readlink /proc/{}/cwd 2>/dev/null || lsof -a -p {} -d cwd -F n 2>/dev/null | grep '^n' | sed 's/^n//'",
            pid, pid
        );
        let cwd = ssh_manager.exec(host_name, &cwd_cmd).await.unwrap_or_default();
        let cwd = cwd.trim().to_string();
        if cwd.is_empty() || !cwd.starts_with('/') {
            continue;
        }

        let git_cmd = format!(
            "git -C '{}' rev-list --max-parents=0 HEAD 2>/dev/null | sort | head -1",
            cwd.replace('\'', "'\\''")
        );
        let project_id = ssh_manager.exec(host_name, &git_cmd).await.unwrap_or_default();
        let project_id = project_id.trim().to_string();
        if project_id.is_empty() {
            continue;
        }

        let db_query = format!(
            "sqlite3 -separator '|' ~/.local/share/opencode/opencode.db \
             \"SELECT id, title, directory, time_updated \
              FROM session \
              WHERE project_id = '{}' AND parent_id IS NULL \
              ORDER BY time_updated DESC LIMIT 1\"",
            project_id.replace('\'', "''")
        );
        let db_output = ssh_manager.exec(host_name, &db_query).await.unwrap_or_default();
        let db_output = db_output.trim();
        if db_output.is_empty() {
            continue;
        }

        let parts: Vec<&str> = db_output.splitn(4, '|').collect();
        if parts.len() < 4 {
            continue;
        }

        let session_id = parts[0].to_string();
        if !seen_session_ids.insert(session_id.clone()) {
            continue;
        }

        let tmux_pane = get_remote_process_tty(ssh_manager, host_name, *pid)
            .await
            .and_then(|tty| tmux_panes_by_tty.get(&tty).cloned());

        active_sessions.push(ActiveSession {
            session_id,
            title: parts[1].to_string(),
            directory: parts[2].to_string(),
            project_id: project_id.clone(),
            time_updated_ms: parts[3].parse().unwrap_or(0),
            tui_pid: *pid,
            tmux_session: tmux_pane.as_ref().map(|pane| pane.session_name.clone()),
            tmux_window: tmux_pane.as_ref().map(|pane| pane.window_name.clone()),
            tmux_window_index: tmux_pane.as_ref().map(|pane| pane.window_index),
            tmux_pane_index: tmux_pane.as_ref().map(|pane| pane.pane_index),
            tmux_pane_tty: tmux_pane.as_ref().map(|pane| pane.pane_tty.clone()),
        });
    }

    let current_ports: Vec<u16> = current_remote_ports.into_iter().collect();
    reconcile_port_forwards(ssh_manager, host_name, &current_ports, &previous_remote_ports).await;

    Ok(ScanResult {
        server_port,
        server_remote_port,
        active_sessions,
    })
}

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

async fn fetch_remote_tmux_panes(ssh_manager: &SshManager, host_name: &str) -> Vec<TmuxPane> {
    let tmux_cmd = "tmux list-panes -a -F '#{pane_pid}\t#{session_name}\t#{window_name}\t#{window_index}\t#{pane_index}\t#{pane_current_command}\t#{pane_tty}' 2>/dev/null || true";
    let output = ssh_manager.exec(host_name, tmux_cmd).await.unwrap_or_default();
    parse_remote_tmux_output(&output)
}

fn parse_remote_tmux_output(raw: &str) -> Vec<TmuxPane> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(7, '\t').collect();
            if parts.len() < 7 {
                return None;
            }

            Some(TmuxPane {
                pane_pid: parts[0].parse().ok()?,
                session_name: parts[1].to_string(),
                window_name: parts[2].to_string(),
                window_index: parts[3].parse().ok()?,
                pane_index: parts[4].parse().ok()?,
                pane_current_command: parts[5].to_string(),
                pane_tty: parts[6].trim().to_string(),
            })
        })
        .collect()
}

async fn get_remote_process_tty(
    ssh_manager: &SshManager,
    host_name: &str,
    pid: u32,
) -> Option<String> {
    let tty_cmd = format!("ps -p {} -o tty= 2>/dev/null | tr -d ' '", pid);
    let tty = ssh_manager.exec(host_name, &tty_cmd).await.ok()?;
    let tty = tty.trim();

    if tty.is_empty() || tty == "?" {
        return None;
    }

    if tty.starts_with("/dev/") {
        Some(tty.to_string())
    } else {
        Some(format!("/dev/{}", tty))
    }
}

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

// ─── Legacy API (kept for debug commands) ─────────────────────────────────────

pub async fn scan_remote(
    ssh_manager: &mut SshManager,
    host_name: &str,
) -> Vec<DiscoveredInstance> {
    let result = scan_remote_v2(ssh_manager, host_name).await;
    let mut instances = Vec::new();

    for s in result.active_sessions {
        instances.push(DiscoveredInstance {
            pid: s.tui_pid,
            port: result.server_port.unwrap_or(0),
            remote_port: result.server_remote_port,
            tmux_session: s.tmux_session,
            tmux_window: s.tmux_window,
            tmux_window_index: s.tmux_window_index,
            tmux_pane_index: s.tmux_pane_index,
            tmux_pane_tty: s.tmux_pane_tty,
        });
    }

    instances
}
