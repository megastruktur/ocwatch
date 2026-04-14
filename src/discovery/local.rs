use crate::discovery::{discover_port_from_lsof, parse_tmux_output, DiscoveredInstance};
use anyhow::Result;
use tokio::process::Command;

/// Returns empty Vec (not error) if tmux is not running or no opencode panes found.
pub async fn scan_local_tmux() -> Vec<DiscoveredInstance> {
    let instances = match try_scan_local_tmux().await {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!(
                "Local tmux scan failed (expected if tmux not running): {}",
                e
            );
            vec![]
        }
    };

    if instances.is_empty() {
        return scan_all_processes().await;
    }

    instances
}

async fn try_scan_local_tmux() -> Result<Vec<DiscoveredInstance>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_pid} #{session_name} #{window_name} #{window_index} #{pane_index} #{pane_current_command} #{pane_tty}",
        ])
        .output()
        .await?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let panes = parse_tmux_output(&raw);

    tracing::debug!("Found {} tmux panes", panes.len());

    let mut instances = Vec::new();

    for pane in panes {
        if !is_opencode_command(&pane.pane_current_command) {
            continue;
        }

        tracing::debug!(
            "Found opencode pane: session={} window={} pane={} cmd={}",
            pane.session_name,
            pane.window_name,
            pane.pane_index,
            pane.pane_current_command
        );

        // pane_pid is the shell; opencode is a child process
        let oc_pid = find_opencode_child_pid(pane.pane_pid)
            .await
            .unwrap_or(pane.pane_pid);

        let port = match discover_port_for_pid(oc_pid).await {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "Could not discover port for opencode PID {}, skipping",
                    oc_pid
                );
                continue;
            }
        };

        instances.push(DiscoveredInstance {
            pid: oc_pid,
            port,
            remote_port: None,
            tmux_session: Some(pane.session_name),
            tmux_window: Some(pane.window_name),
            tmux_window_index: Some(pane.window_index),
            tmux_pane_index: Some(pane.pane_index),
            tmux_pane_tty: Some(pane.pane_tty),
        });
    }

    Ok(instances)
}

async fn scan_all_processes() -> Vec<DiscoveredInstance> {
    let output = match Command::new("ps")
        .args(["aux"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut instances = Vec::new();

    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if !lower.contains("opencode") && !lower.contains(".opencode/") {
            continue;
        }
        if line.contains("grep") || line.contains("ps aux") || line.contains("ocwatch") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(port) = discover_port_for_pid(pid).await {
            instances.push(DiscoveredInstance {
                pid,
                port,
                remote_port: None,
                tmux_session: None,
                tmux_window: None,
                tmux_window_index: None,
                tmux_pane_index: None,
                tmux_pane_tty: None,
            });
        }
    }

    instances.sort_by_key(|i| i.port);
    instances.dedup_by_key(|i| i.port);
    instances
}

fn is_opencode_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    lower.contains("opencode") || lower == "oc"
}

async fn find_opencode_child_pid(shell_pid: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-axo", "pid,ppid,args"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let child_pids: Vec<u32> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return None;
            }
            let ppid: u32 = parts[1].parse().ok()?;
            if ppid != shell_pid {
                return None;
            }
            parts[0].parse().ok()
        })
        .collect();

    for child_pid in &child_pids {
        if is_opencode_pid(&stdout, *child_pid) {
            return Some(*child_pid);
        }

        if let Some(oc_pid) = find_opencode_child_recursive(&stdout, *child_pid) {
            return Some(oc_pid);
        }
    }

    None
}

fn find_opencode_child_recursive(ps_output: &str, parent_pid: u32) -> Option<u32> {
    let child_pids: Vec<u32> = ps_output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return None;
            }
            let ppid: u32 = parts[1].parse().ok()?;
            if ppid != parent_pid {
                return None;
            }
            parts[0].parse().ok()
        })
        .collect();

    for child_pid in &child_pids {
        if is_opencode_pid(ps_output, *child_pid) {
            return Some(*child_pid);
        }
        if let Some(oc_pid) = find_opencode_child_recursive(ps_output, *child_pid) {
            return Some(oc_pid);
        }
    }

    None
}

fn is_opencode_pid(ps_output: &str, pid: u32) -> bool {
    let pid_str = pid.to_string();
    ps_output.lines().any(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            return false;
        }
        if parts[0] != pid_str {
            return false;
        }
        let args = parts[2..].join(" ").to_lowercase();
        args.contains("opencode") || args.contains("/.opencode/")
    })
}

async fn discover_port_for_pid(pid: u32) -> Option<u16> {
    if let Ok(output) = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-i", "-P", "-n"])
        .output()
        .await
    {
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some(port) = discover_port_from_lsof(&text) {
            return Some(port);
        }
    }

    discover_port_from_cmdline(pid).await
}

async fn discover_port_from_cmdline(pid: u32) -> Option<u16> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .await
        .ok()?;

    let args = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = args.split_whitespace().collect();

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
