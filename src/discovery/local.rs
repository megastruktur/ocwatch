use crate::discovery::{
    decode_sqlite_hex_payload, discover_port_from_lsof, infer_session_state_from_part,
    parse_tmux_output, ActiveSession, DiscoveredInstance, ScanResult,
};
use crate::ipc::RecentDirEntry;
use anyhow::Result;
use tokio::process::Command;

/// Scan local machine for active OpenCode sessions.
/// Finds TUI processes → resolves their CWD → git project_id → queries SQLite DB.
/// Also discovers the server process port for status/action queries.
pub async fn scan_local() -> ScanResult {
    match try_scan_local().await {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!("Local scan failed: {}", e);
            ScanResult {
                server_port: None,
                server_remote_port: None,
                active_sessions: vec![],
            }
        }
    }
}

async fn try_scan_local() -> Result<ScanResult> {
    let all_procs = find_all_opencode_processes().await;
    if all_procs.is_empty() {
        return Ok(ScanResult {
            server_port: None,
            server_remote_port: None,
            active_sessions: vec![],
        });
    }

    // Separate server (has LISTEN port) from TUI (no LISTEN port) processes
    let mut server_port: Option<u16> = None;
    let mut tui_procs: Vec<OcProcess> = Vec::new();

    for proc in all_procs {
        if let Some(port) = discover_port_for_pid(proc.pid).await {
            server_port = Some(port);
        } else {
            tui_procs.push(proc);
        }
    }

    // Resolve each TUI to an active session
    let db_path = opencode_db_path();
    let mut active_sessions = Vec::new();
    let mut seen_session_ids = std::collections::HashSet::new();

    for tui in &tui_procs {
        let cwd = match get_process_cwd(tui.pid).await {
            Some(c) => c,
            None => continue,
        };

        let project_id = match resolve_project_id(&cwd).await {
            Some(id) => id,
            None => continue,
        };

        if let Some(root_session) = query_latest_session(&db_path, &project_id).await {
            let sessions = query_session_tree(&db_path, &root_session.id)
                .await
                .filter(|sessions| !sessions.is_empty())
                .unwrap_or_else(|| vec![root_session.clone()]);

            for session in sessions {
                if !seen_session_ids.insert(session.id.clone()) {
                    continue;
                }

                let inferred_state = query_inferred_state(&db_path, &session.id).await;
                let is_root_session = session.id == root_session.id;

                active_sessions.push(ActiveSession {
                    session_id: session.id,
                    parent_id: session.parent_id,
                    title: session.title,
                    directory: session.directory,
                    project_id: project_id.clone(),
                    inferred_state,
                    time_updated_ms: session.time_updated_ms,
                    tui_pid: tui.pid,
                    tmux_session: is_root_session.then(|| tui.tmux_session.clone()).flatten(),
                    tmux_window: is_root_session.then(|| tui.tmux_window.clone()).flatten(),
                    tmux_window_index: is_root_session.then_some(tui.tmux_window_index).flatten(),
                    tmux_pane_index: is_root_session.then_some(tui.tmux_pane_index).flatten(),
                    tmux_pane_tty: is_root_session.then(|| tui.tmux_pane_tty.clone()).flatten(),
                });
            }
        }
    }

    Ok(ScanResult {
        server_port,
        server_remote_port: None,
        active_sessions,
    })
}

// ─── Process Discovery ────────────────────────────────────────────────────────

struct OcProcess {
    pid: u32,
    tmux_session: Option<String>,
    tmux_window: Option<String>,
    tmux_window_index: Option<u32>,
    tmux_pane_index: Option<u32>,
    tmux_pane_tty: Option<String>,
}

/// Find all OpenCode processes — first via tmux, then fallback to ps aux.
async fn find_all_opencode_processes() -> Vec<OcProcess> {
    let procs = find_via_tmux().await;
    if !procs.is_empty() {
        return procs;
    }
    find_via_ps().await
}

async fn find_via_tmux() -> Vec<OcProcess> {
    let output = match Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_pid} #{session_name} #{window_name} #{window_index} #{pane_index} #{pane_current_command} #{pane_tty}",
        ])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let raw = String::from_utf8_lossy(&output.stdout);
    let panes = parse_tmux_output(&raw);
    let mut procs = Vec::new();

    for pane in panes {
        if !is_opencode_command(&pane.pane_current_command) {
            continue;
        }

        let oc_pid = find_opencode_child_pid(pane.pane_pid)
            .await
            .unwrap_or(pane.pane_pid);

        procs.push(OcProcess {
            pid: oc_pid,
            tmux_session: Some(pane.session_name),
            tmux_window: Some(pane.window_name),
            tmux_window_index: Some(pane.window_index),
            tmux_pane_index: Some(pane.pane_index),
            tmux_pane_tty: Some(pane.pane_tty),
        });
    }

    procs
}

async fn find_via_ps() -> Vec<OcProcess> {
    let output = match Command::new("ps").args(["aux"]).output().await {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut procs = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 11 {
            continue;
        }
        let cmd = parts[10..].join(" ");
        if !is_opencode_binary(&cmd.to_lowercase()) {
            continue;
        }
        if line.contains("grep") || line.contains("ps aux") || line.contains("ocwatch") {
            continue;
        }

        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        procs.push(OcProcess {
            pid,
            tmux_session: None,
            tmux_window: None,
            tmux_window_index: None,
            tmux_pane_index: None,
            tmux_pane_tty: None,
        });
    }

    procs.sort_by_key(|p| p.pid);
    procs.dedup_by_key(|p| p.pid);
    procs
}

fn is_opencode_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    if lower == "oc" || lower == "opencode" {
        return true;
    }
    false
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
        let cmd = parts[2..].join(" ").to_lowercase();
        is_opencode_binary(&cmd)
    })
}

fn is_opencode_binary(cmd: &str) -> bool {
    // Match "opencode" or paths ending in "/opencode" (e.g. "~/.opencode/bin/opencode")
    // Exclude language servers and other tools that live under .opencode/bin/
    if cmd.contains("terraform-ls")
        || cmd.contains("gopls")
        || cmd.contains("rust-analyzer")
        || cmd.contains("typescript-language-server")
        || cmd.contains("node_modules")
    {
        return false;
    }
    let first_token = cmd.split_whitespace().next().unwrap_or("");
    first_token == "opencode"
        || first_token == "oc"
        || first_token.ends_with("/opencode")
        || first_token.ends_with("/oc")
}

// ─── CWD + Project ID Resolution ─────────────────────────────────────────────

async fn get_process_cwd(pid: u32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-F", "n"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix('n') {
            if path.starts_with('/') {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Get the OpenCode project_id for a directory.
/// OpenCode uses the first root commit hash as the project ID.
async fn resolve_project_id(cwd: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", cwd, "rev-list", "--max-parents=0", "HEAD"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut roots: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    roots.sort();
    roots.first().map(|s| s.trim().to_string())
}

// ─── SQLite Session Query ─────────────────────────────────────────────────────

#[derive(Clone)]
struct DbSession {
    id: String,
    parent_id: Option<String>,
    title: String,
    directory: String,
    time_updated_ms: u64,
}

const LOCAL_INFERRED_STATE_LOOKBACK_ROWS: usize = 32;

fn opencode_db_path() -> String {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return format!("{}/opencode/opencode.db", xdg);
    }
    if let Some(home) = dirs::home_dir() {
        return format!("{}/.local/share/opencode/opencode.db", home.display());
    }
    "~/.local/share/opencode/opencode.db".to_string()
}

pub async fn recent_directories(limit: usize) -> Vec<RecentDirEntry> {
    let db_path = opencode_db_path();
    let query = format!(
        "SELECT directory, MAX(time_updated) \
         FROM session \
         WHERE parent_id IS NULL AND directory != '' \
         GROUP BY directory \
         ORDER BY MAX(time_updated) DESC LIMIT {}",
        limit
    );

    let output = match Command::new("sqlite3")
        .args(["-separator", "|", &db_path, &query])
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => return vec![],
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() < 2 || parts[0].trim().is_empty() {
                return None;
            }
            Some(RecentDirEntry {
                host: "local".to_string(),
                directory: parts[0].trim().to_string(),
                last_seen_unix_ms: parts[1].trim().parse().unwrap_or(0),
            })
        })
        .collect()
}

async fn query_latest_session(db_path: &str, project_id: &str) -> Option<DbSession> {
    let query = format!(
        "SELECT id, COALESCE(parent_id, ''), title, directory, time_updated \
         FROM session \
         WHERE project_id = '{}' AND parent_id IS NULL \
         ORDER BY time_updated DESC LIMIT 1",
        project_id.replace('\'', "''")
    );

    let output = Command::new("sqlite3")
        .args(["-separator", "|", db_path, &query])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        tracing::debug!(
            "sqlite3 query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let parts: Vec<&str> = line.splitn(5, '|').collect();
    if parts.len() < 5 {
        return None;
    }

    Some(DbSession {
        id: parts[0].to_string(),
        parent_id: (!parts[1].is_empty()).then(|| parts[1].to_string()),
        title: parts[2].to_string(),
        directory: parts[3].to_string(),
        time_updated_ms: parts[4].parse().unwrap_or(0),
    })
}

async fn query_session_tree(db_path: &str, root_session_id: &str) -> Option<Vec<DbSession>> {
    let query = format!(
        "WITH RECURSIVE session_tree(id, parent_id, title, directory, time_updated) AS ( \
             SELECT id, parent_id, title, directory, time_updated \
             FROM session \
             WHERE id = '{}' \
             UNION ALL \
             SELECT child.id, child.parent_id, child.title, child.directory, child.time_updated \
             FROM session child \
             JOIN session_tree parent ON child.parent_id = parent.id \
         ) \
         SELECT id, COALESCE(parent_id, ''), title, directory, time_updated \
         FROM session_tree \
         ORDER BY CASE WHEN parent_id IS NULL THEN 0 ELSE 1 END, time_updated DESC",
        root_session_id.replace('\'', "''")
    );

    let output = Command::new("sqlite3")
        .args(["-separator", "|", db_path, &query])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        tracing::debug!(
            "sqlite3 session tree query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(5, '|').collect();
                if parts.len() < 5 {
                    return None;
                }

                Some(DbSession {
                    id: parts[0].to_string(),
                    parent_id: (!parts[1].is_empty()).then(|| parts[1].to_string()),
                    title: parts[2].to_string(),
                    directory: parts[3].to_string(),
                    time_updated_ms: parts[4].parse().unwrap_or(0),
                })
            })
            .collect(),
    )
}

async fn query_inferred_state(db_path: &str, session_id: &str) -> Option<crate::types::SessionState> {
    let query = format!(
        "SELECT hex(data) FROM part WHERE session_id = '{}' ORDER BY time_updated DESC LIMIT {}",
        session_id.replace('\'', "''"),
        LOCAL_INFERRED_STATE_LOOKBACK_ROWS,
    );

    let output = Command::new("sqlite3")
        .args([db_path, &query])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(decode_sqlite_hex_payload)
        .find_map(|part| infer_session_state_from_part(&part))
}

// ─── Port Discovery (for server processes) ────────────────────────────────────

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
            let port = parts.get(i + 1)?.parse().ok()?;
            return (port > 1024).then_some(port);
        }
        if let Some(port_str) = part.strip_prefix("--port=") {
            let port = port_str.parse().ok()?;
            return (port > 1024).then_some(port);
        }
    }

    None
}

// ─── Legacy API (kept for debug commands) ─────────────────────────────────────

pub async fn scan_local_tmux() -> Vec<DiscoveredInstance> {
    let result = scan_local().await;
    let mut instances = Vec::new();

    if let Some(port) = result.server_port {
        instances.push(DiscoveredInstance {
            pid: 0,
            port,
            remote_port: None,
            tmux_session: None,
            tmux_window: None,
            tmux_window_index: None,
            tmux_pane_index: None,
            tmux_pane_tty: None,
        });
    }

    for s in result.active_sessions {
        instances.push(DiscoveredInstance {
            pid: s.tui_pid,
            port: result.server_port.unwrap_or(0),
            remote_port: None,
            tmux_session: s.tmux_session,
            tmux_window: s.tmux_window,
            tmux_window_index: s.tmux_window_index,
            tmux_pane_index: s.tmux_pane_index,
            tmux_pane_tty: s.tmux_pane_tty,
        });
    }

    instances
}
