use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixListener;

/// Returns the path to the daemon's Unix socket.
pub fn socket_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ocwatch")
        .join("ocwatch.sock")
}

/// Returns the path to the daemon's PID file.
pub fn pid_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ocwatch")
        .join("ocwatch.pid")
}

/// Returns the path to the daemon's log file.
pub fn log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ocwatch")
        .join("daemon.log")
}

/// Ensure the ocwatch data directory exists.
fn ensure_data_dir() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ocwatch");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create data directory: {:?}", dir))?;
    Ok(dir)
}

/// Check if a PID is alive (sends signal 0).
fn is_pid_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// Start the daemon as a detached background process.
/// Uses Command::spawn (NOT fork) to avoid unsafety with tokio runtime.
pub async fn start_daemon() -> Result<()> {
    ensure_data_dir()?;

    let pid_file = pid_path();
    let socket_file = socket_path();

    if pid_file.exists() {
        let pid_str = std::fs::read_to_string(&pid_file)
            .context("Failed to read PID file")?;
        let pid: u32 = pid_str
            .trim()
            .parse()
            .context("PID file contains invalid PID")?;

        if is_pid_alive(pid) {
            eprintln!("Daemon already running (PID: {})", pid);
            std::process::exit(1);
        } else {
            tracing::info!("Cleaning up stale PID file (PID {} is dead)", pid);
            let _ = std::fs::remove_file(&pid_file);
            let _ = std::fs::remove_file(&socket_file);
        }
    }

    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    let log_file = log_path();
    let stdout_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("Failed to open log file: {:?}", log_file))?;
    let stderr_log = stdout_log
        .try_clone()
        .context("Failed to clone log file handle")?;

    let child = std::process::Command::new(&exe)
        .arg("daemon")
        .arg("run")
        .stdout(stdout_log)
        .stderr(stderr_log)
        .spawn()
        .context("Failed to spawn daemon process")?;

    let child_pid = child.id();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if socket_file.exists() && pid_file.exists() {
            break;
        }
        if std::time::Instant::now() > deadline {
            eprintln!(
                "Daemon failed to start within 5 seconds. Check log: {:?}",
                log_path()
            );
            std::process::exit(1);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!("Daemon started (PID: {})", child_pid);
    Ok(())
}

/// Stop the running daemon by sending SIGTERM.
pub async fn stop_daemon() -> Result<()> {
    let pid_file = pid_path();

    if !pid_file.exists() {
        eprintln!("No daemon running (no PID file found)");
        std::process::exit(1);
    }

    let pid_str = std::fs::read_to_string(&pid_file).context("Failed to read PID file")?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .context("PID file contains invalid PID")?;

    if !is_pid_alive(pid) {
        eprintln!(
            "Daemon PID {} is not running. Cleaning up stale files.",
            pid
        );
        let _ = std::fs::remove_file(&pid_file);
        let _ = std::fs::remove_file(socket_path());
        return Ok(());
    }

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .context("Failed to send SIGTERM to daemon")?;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !is_pid_alive(pid) {
            println!("Daemon stopped.");
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            println!("Daemon force-killed.");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Print daemon status as JSON.
pub async fn daemon_status() -> Result<()> {
    let socket_file = socket_path();
    let pid_file = pid_path();

    if !socket_file.exists() {
        println!(r#"{{"running": false, "reason": "socket not found"}}"#);
        std::process::exit(1);
    }

    if pid_file.exists() {
        let pid_str = std::fs::read_to_string(&pid_file).unwrap_or_default();
        let pid: u32 = pid_str.trim().parse().unwrap_or(0);
        if is_pid_alive(pid) {
            // TODO: connect to socket and get full status (Task 5 IPC)
            println!(
                r#"{{"running": true, "pid": {}, "socket": "{}"}}"#,
                pid,
                socket_file.display()
            );
            return Ok(());
        }
    }

    println!(r#"{{"running": false}}"#);
    std::process::exit(1);
}

/// Run the daemon in foreground (called by 'daemon start' as a child process).
/// This function does NOT return until the daemon is signaled to stop.
///
/// Note: tracing is already initialized by main(). Do NOT re-init here.
pub async fn run_daemon() -> Result<()> {
    tracing::info!("ocwatch daemon starting (PID: {})", std::process::id());

    ensure_data_dir()?;

    let pid_file = pid_path();
    std::fs::write(&pid_file, std::process::id().to_string())
        .with_context(|| format!("Failed to write PID file: {:?}", pid_file))?;
    tracing::info!("PID file written: {:?}", pid_file);

    let socket_file = socket_path();
    let _ = std::fs::remove_file(&socket_file);

    let listener = UnixListener::bind(&socket_file)
        .with_context(|| format!("Failed to bind Unix socket: {:?}", socket_file))?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket_file, std::fs::Permissions::from_mode(0o600))
        .context("Failed to set socket permissions")?;

    tracing::info!("Unix socket created: {:?}", socket_file);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("Failed to register SIGTERM handler")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("Failed to register SIGINT handler")?;

    tracing::info!("Daemon ready. Waiting for connections and signals.");

    // TODO: Task 10 replaces this with the full event loop
    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        tracing::debug!("Client connected");
                        // TODO: handle client in Task 10
                        drop(stream);
                    }
                    Err(e) => {
                        tracing::error!("Failed to accept connection: {}", e);
                    }
                }
            }
        }
    }

    tracing::info!("Cleaning up...");
    let _ = std::fs::remove_file(&socket_file);
    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("Daemon stopped.");

    Ok(())
}
