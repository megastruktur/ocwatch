use serde::{Deserialize, Serialize};

pub mod local;
pub mod remote;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredInstance {
    pub pid: u32,
    pub port: u16,
    pub remote_port: Option<u16>,
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub tmux_window_index: Option<u32>,
    pub tmux_pane_index: Option<u32>,
    /// TTY path (e.g. /dev/ttys012) — used for bell delivery
    pub tmux_pane_tty: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub pane_pid: u32,
    pub session_name: String,
    pub window_name: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub pane_current_command: String,
    pub pane_tty: String,
}

/// Parse raw `tmux list-panes -a -F '...'` output into TmuxPane structs.
/// Format: "#{pane_pid} #{session_name} #{window_name} #{window_index} #{pane_index} #{pane_current_command} #{pane_tty}"
pub fn parse_tmux_output(raw: &str) -> Vec<TmuxPane> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(7, ' ').collect();
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

/// Parse lsof LISTEN output to extract port.
/// Input line example: "opencode 1234 user 23u IPv4 0x... 0t0 TCP localhost:4096 (LISTEN)"
pub fn discover_port_from_lsof(lsof_output: &str) -> Option<u16> {
    for line in lsof_output.lines() {
        if !line.contains("LISTEN") {
            continue;
        }
        // Look for ":<port>" pattern before "(LISTEN)"
        if let Some(addr_part) = line.split_whitespace().rev().nth(1) {
            if let Some(colon_pos) = addr_part.rfind(':') {
                let port_str = &addr_part[colon_pos + 1..];
                if let Ok(port) = port_str.parse::<u16>() {
                    if port > 1024 {
                        return Some(port);
                    }
                }
            }
        }
    }
    None
}

/// Linux-specific fallback: parse /proc/net/tcp to find listening port for a PID.
pub fn discover_port_from_proc_net_tcp(proc_net_tcp: &str, _pid: u32) -> Option<u16> {
    // Format: sl  local_address rem_address   st ...
    // local_address is "XXXX:YYYY" where YYYY is hex port
    // State 0A = LISTEN
    for line in proc_net_tcp.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        if parts.get(3) == Some(&"0A") {
            if let Some(addr) = parts.get(1) {
                if let Some(colon_pos) = addr.rfind(':') {
                    let hex_port = &addr[colon_pos + 1..];
                    if let Ok(port) = u16::from_str_radix(hex_port, 16) {
                        if port > 1024 {
                            return Some(port);
                        }
                    }
                }
            }
        }
    }
    None
}
