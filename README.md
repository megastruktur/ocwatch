# ocwatch

Terminal monitor for OpenCode AI coding sessions — local and remote.

## Overview

ocwatch is a daemon + TUI application written in Rust that auto-discovers OpenCode sessions running locally (in tmux panes) and on remote hosts (via SSH), displays them in a live split-panel terminal interface, and notifies via tmux bell when a session needs attention. The daemon runs in the background, polling all configured hosts on a configurable interval. A TUI client connects to the daemon via a Unix socket and renders live session state, allowing you to approve permissions or drop into a session with a single keystroke.

## Features

- Auto-discovers OpenCode processes in local tmux panes and via `ps aux` fallback
- Monitors remote hosts via SSH — no agent needed on the remote side
- SSH ControlMaster for fast, connection-reusing remote polling (5x speedup)
- Real-time session state tracking: 9 states (Busy, Idle, Waiting, Error, etc.)
- tmux bell notifications when a session transitions to an actionable state
- Quick-approve pending permission requests from the TUI or CLI
- Drop-in to any session: local tmux pane switch or remote SSH attach
- Split-panel TUI: session list (grouped by host) on the left, session detail on the right
- Session detail: state, working directory, tmux coordinates, and recent activity age
- Extensible `CodingAgent` trait — OpenCode adapter built-in, ready for future agents

## Prerequisites

- **Rust** 2021 edition (Cargo) — for building from source
- **OpenCode** running on the monitored machine (`opencode serve` or `opencode --port 0`)
- **tmux** — required for local process discovery and bell notifications; optional for remote hosts
- **SSH** with key-based auth — required for remote host monitoring
- macOS or Linux (uses `nix` for signals, `lsof`/`/proc/net/tcp` for port discovery)

## Build & Install

```bash
cd ocwatch
cargo build --release
# Binary: target/release/ocwatch

# Optional: install to PATH
cp target/release/ocwatch ~/.local/bin/
```

## Quick Start

1. Start the daemon: `ocwatch daemon start`
2. Launch the TUI: `ocwatch` (requires daemon to be running)
3. Navigate sessions with `j`/`k`, press `a` to approve, `Enter` to drop in, `q` to quit

## CLI Reference

### Default (TUI)

```bash
ocwatch
```
Opens the split-panel TUI. Requires the daemon to be running. Connects via Unix socket.

### Daemon Management

```bash
ocwatch daemon start    # Start daemon in background
ocwatch daemon stop     # Send SIGTERM to daemon; SIGKILL after 5s if unresponsive
ocwatch daemon status   # Print daemon state (PID, uptime, sessions, hosts) as JSON
```

### Approve

```bash
ocwatch approve <session_id>
```
Approve a pending permission request for `<session_id>` without opening the TUI. Connects to the daemon, sends the approve request, and prints the result.

### Debug Utilities

```bash
ocwatch debug scan-local               # Scan local tmux panes for OpenCode processes (JSON)
ocwatch debug scan-remote <host>       # Scan a remote host for OpenCode processes (JSON)
ocwatch debug ssh-check <host>         # Test SSH ControlMaster connection to <host> (JSON)
ocwatch debug oc-client <url>          # Query OpenCode HTTP API at <url> (e.g. http://localhost:4096)
ocwatch debug ipc-roundtrip            # Send GetStatus to daemon socket and print response
ocwatch debug inject-event <session_id> <state>  # Inject synthetic state change for QA
                                       # States: idle, busy, error, waiting_for_permission, waiting_for_input
```

## TUI Keybindings

| Key | Action |
|-----|--------|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `a` | Approve pending permission for selected session |
| `Enter` | Drop into selected session (tmux switch or SSH attach) |
| `r` | Force re-scan all hosts |
| `?` | Show keybinding hint in status bar |
| `q` / `Ctrl-C` | Quit |

## Configuration

Config file location:

| Platform | Path |
|----------|------|
| Linux | `~/.config/ocwatch/config.toml` |
| macOS | `~/Library/Application Support/ocwatch/config.toml` |

If no config file exists, ocwatch uses defaults (local-only monitoring, 5-second poll interval).

### Example Configuration

```toml
# ocwatch configuration
# Place this file at ~/.config/ocwatch/config.toml

# How often to poll for session updates (seconds, minimum 1)
poll_interval_secs = 5

# Local machine is always monitored automatically.
# Add remote hosts here:

[[hosts]]
name = "megaserver"          # Display name in TUI
ssh_target = "megaserver"    # SSH destination (matches ~/.ssh/config Host entry or user@host)
# ssh_identity = "/Users/you/.ssh/id_ed25519"  # Optional: path to SSH private key
# ssh_port = 22              # Optional: SSH port (default 22)
```

### Field Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `poll_interval_secs` | integer | `5` | Seconds between host scans. Minimum: `1`. |
| `[[hosts]].name` | string | — | Display name for the host in the TUI. Also used in `scan-remote` and `ssh-check` commands. |
| `[[hosts]].ssh_target` | string | — | SSH connection target. Matches a `Host` entry in `~/.ssh/config` or `user@host`. Required for remote hosts. |
| `[[hosts]].ssh_identity` | path | — | Optional path to SSH private key (`-i` flag). |
| `[[hosts]].ssh_port` | integer | `22` | Optional SSH port override (`-p` flag). |

## Session States

ocwatch tracks 9 session states reported by the OpenCode HTTP API.

| State | Icon | Color | Description | Bell |
|-------|------|-------|-------------|------|
| `Busy` | ◐ | Yellow | Agent is actively working | — |
| `Idle` | ● | Green | Agent finished; awaiting next prompt | ✓ |
| `WaitingForPermission` | ◉ | Red | Agent needs tool permission approval | ✓ |
| `WaitingForInput` | ? | Yellow | Agent needs a user message to continue | ✓ |
| `Error` | ✗ | Red | Agent encountered an error | ✓ |
| `Compacting` | ⟳ | Blue | Context window compaction in progress | — |
| `Completed` | ✓ | Green | Session has finished | — |
| `Disconnected` | ○ | Gray | OpenCode instance unreachable | — |
| `Unknown` | · | Gray | Unrecognized status string | — |

Bell cooldown: 30 seconds per session (prevents notification spam on rapid state changes).

## Architecture

### Daemon–Client Model

ocwatch uses a daemon/client split. The daemon process runs in the background and owns all polling, discovery, and SSH connections. TUI instances (and the `approve` CLI command) connect to the daemon via a Unix domain socket and subscribe to state updates.

```
┌─────────────────────────────────────────────────────────┐
│                     ocwatch daemon                        │
│                                                           │
│  Discovery Loop (every poll_interval_secs)               │
│  ├── Local: tmux list-panes → lsof → OC HTTP API        │
│  └── Remote: SSH exec ps aux → lsof → port forward → OC │
│                                                           │
│  State Store: sessions HashMap<key, SessionInfo>         │
│  Bell Notifier: fires BEL to tmux pane TTY on transition │
│                                                           │
│  IPC: Unix socket (JSON Lines) ─────────────────────────┤
└─────────────────────────────────────────────────────────┘
            │
            │ subscribe / updates
            ▼
┌─────────────────────┐     ┌────────────────────┐
│     ocwatch TUI      │     │  ocwatch approve   │
│  (ratatui + tokio)  │     │  (one-shot CLI)    │
└─────────────────────┘     └────────────────────┘
```

### Discovery

Local discovery scans tmux panes for processes named `opencode` or `oc`, resolves the OpenCode child PID, then uses `lsof` to find its listening port. If tmux is not running, falls back to `ps aux` process scan.

Remote discovery SSHes into each configured host, runs `ps aux` to find OpenCode processes, uses `lsof` (or `/proc/net/tcp` as fallback) for port detection, then sets up an SSH local port forward so the daemon can reach the remote OpenCode HTTP API as if it were local.

### SSH ControlMaster

All SSH operations for a host share one persistent ControlMaster connection (socket at `/tmp/ocw-{host_name}`). Port-forward processes multiplex over this connection. ControlMaster reduces per-command SSH latency from ~400ms to ~80ms.

### Bell Notifications

When a session transitions into an actionable state (Idle, WaitingForPermission, WaitingForInput, Error), the daemon fires a tmux bell using three fallback strategies:

1. Write BEL character (`\x07`) directly to the pane's TTY device
2. `tmux display-message` on the pane's window (5-second popup)
3. Broadcast `tmux display-message` to all panes (if no tmux info available)

## File Paths

Paths are resolved by the `dirs` crate and vary by platform:

| File | Linux | macOS |
|------|-------|-------|
| Config | `~/.config/ocwatch/config.toml` | `~/Library/Application Support/ocwatch/config.toml` |
| Daemon socket | `~/.local/share/ocwatch/ocwatch.sock` | `~/Library/Application Support/ocwatch/ocwatch.sock` |
| PID file | `~/.local/share/ocwatch/ocwatch.pid` | `~/Library/Application Support/ocwatch/ocwatch.pid` |
| Daemon log | `~/.local/share/ocwatch/daemon.log` | `~/Library/Application Support/ocwatch/daemon.log` |
| SSH ControlMaster | `/tmp/ocw-{host_name}` (host name truncated to 20 chars) | same |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `OCWATCH_LOG` | Log level filter for daemon and TUI output. Accepts standard `tracing` directives: `error`, `warn`, `info`, `debug`, `trace`. Example: `OCWATCH_LOG=debug ocwatch daemon start` |

## Troubleshooting

**"Daemon is not running (socket not found)"**

Run `ocwatch daemon start` first. The TUI and `approve` command both require a running daemon.

**No sessions appear in the TUI**

Ensure OpenCode is running: `opencode serve` or `opencode --port 0`. For remote hosts, verify SSH connectivity with `ocwatch debug ssh-check <host>`.

**Remote host shows as disconnected**

Check your SSH config matches the `ssh_target` value. Test with `ocwatch debug ssh-check <host>`. Ensure key-based auth works without a passphrase prompt.

**Bell notifications not firing**

ocwatch must itself be running inside a tmux session to fire bells. The local OpenCode process must have been discovered inside a tmux pane (confirmed by `ocwatch debug scan-local` showing `tmux_session` fields in the output).

**Debug logging**

```bash
OCWATCH_LOG=debug ocwatch daemon start  # daemon with debug logs to daemon.log
OCWATCH_LOG=debug ocwatch             # TUI with debug output
```
