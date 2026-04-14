# AGENTS.md — ocwatch

## What this is

Terminal monitor for OpenCode AI coding sessions. Rust daemon + TUI binary. Daemon polls local/remote hosts for OpenCode processes, TUI connects via Unix socket.

## Build & Run

```bash
cargo build --release          # binary: target/release/ocwatch
cargo check                    # fast type-check (no tests exist yet)
OCWATCH_LOG=debug cargo run    # run TUI with debug tracing
```

No tests, no CI, no linter/formatter config. `main.rs` has `#![allow(dead_code, unused_variables, unused_imports)]` — expect no compiler warnings even with unused code.

## Architecture

Single binary, two modes: **daemon** (background poller) and **TUI** (ratatui client). IPC over Unix domain socket using JSON Lines.

```
main.rs          CLI entrypoint (clap derive). No subcommand = TUI.
config.rs        TOML config from platform dirs (dirs crate)
types.rs         SessionState (9 variants), SessionInfo, HostStatus
agent_trait.rs   CodingAgent trait — async_trait, object-safe
ipc.rs           Unix socket JSON Lines protocol, DaemonMessage/ClientMessage enums
daemon/
  lifecycle.rs   start/stop/status/run — forks background process
  core.rs        Poll loop, state store (HashMap<key, SessionInfo>)
  bell.rs        tmux bell notifications (BEL char → display-message fallback)
discovery/
  local.rs       tmux list-panes → lsof → OC HTTP API
  remote.rs      SSH ps aux → lsof → port forward → OC HTTP API
opencode/
  client.rs      reqwest HTTP client for OpenCode API
  adapter.rs     CodingAgent impl for OpenCode
ssh/
  manager.rs     SSH ControlMaster lifecycle, port forwarding
tui/
  app.rs         Main TUI loop (crossterm + ratatui)
  session_list.rs, detail.rs, status_bar.rs, interaction.rs
```

## Constraints an agent must know

- **`SessionInfo` and `HostStatus` are `Serialize`/`Deserialize`.** Never add `std::time::Instant`, `Duration`, or other non-serializable types. Use `u64` for timestamps/durations. Code comment in `types.rs` enforces this.
- **`CodingAgent` trait must remain object-safe.** No generic methods, no `Self`-returning methods. Used as `Box<dyn CodingAgent>`.
- **reqwest uses `rustls-tls` with `default-features = false`.** Intentional — no OpenSSL dependency, enables easier cross-compile. Do not add `native-tls` or default features.
- **Platform: macOS + Linux.** Uses `nix` crate for signals, `lsof` for port discovery, `/proc/net/tcp` as Linux fallback. No Windows support.
- **SSH ControlMaster sockets** live at `/tmp/ocw-{host_name}` (host name truncated to 20 chars).
- **IPC protocol is JSON Lines** (newline-delimited JSON). Both `DaemonMessage` and `ClientMessage` use `#[serde(tag = "type", rename_all = "snake_case")]`.

## Config

Platform-specific path via `dirs` crate:
- macOS: `~/Library/Application Support/ocwatch/config.toml`
- Linux: `~/.config/ocwatch/config.toml`

Daemon socket/PID/log in `dirs::data_local_dir()/ocwatch/`.

## Environment

| Variable | Purpose |
|----------|---------|
| `OCWATCH_LOG` | `tracing` log level filter (`error`, `warn`, `info`, `debug`, `trace`) |

## Debug commands

`ocwatch debug scan-local`, `scan-remote <host>`, `ssh-check <host>`, `oc-client <url>`, `ipc-roundtrip`, `inject-event <id> <state>` — all output JSON. Useful for verifying changes without the full TUI.
