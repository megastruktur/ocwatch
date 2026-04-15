use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::daemon::lifecycle::data_dir;

const STARTUP_LOCK_WAIT: Duration = Duration::from_millis(50);
const STARTUP_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const TUI_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TuiInstanceRecord {
    pid: u32,
    started_at_unix_ms: u64,
    exe_path: String,
    tmux_socket: Option<String>,
    tmux_session: Option<String>,
    tmux_window: Option<String>,
    tmux_pane: Option<String>,
}

impl TuiInstanceRecord {
    fn current() -> Result<Self> {
        let tmux_target = current_tmux_target();
        let exe_path = std::env::current_exe()
            .context("Failed to resolve current executable path")?
            .to_string_lossy()
            .into_owned();

        Ok(Self {
            pid: std::process::id(),
            started_at_unix_ms: unix_time_ms(),
            exe_path,
            tmux_socket: tmux_target.as_ref().map(|target| target.socket_path.clone()),
            tmux_session: tmux_target.as_ref().map(|target| target.session.clone()),
            tmux_window: tmux_target.as_ref().map(|target| target.window.clone()),
            tmux_pane: tmux_target.as_ref().map(|target| target.pane.clone()),
        })
    }

    fn is_attachable(&self) -> bool {
        self.tmux_session.is_some()
    }
}

#[derive(Debug, Clone)]
struct TmuxTarget {
    socket_path: String,
    session: String,
    window: String,
    pane: String,
}

pub async fn run_tui_singleton() -> Result<()> {
    match prepare_tui_launch().await? {
        LaunchOutcome::Attached => Ok(()),
        LaunchOutcome::Start(guard) => {
            let _guard = guard;
            crate::tui::app::run_tui().await
        }
    }
}

enum LaunchOutcome {
    Attached,
    Start(TuiInstanceGuard),
}

struct StartupLock {
    _file: File,
}

struct TuiInstanceGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for TuiInstanceGuard {
    fn drop(&mut self) {
        let Ok(Some(record)) = read_record(&self.path) else {
            return;
        };

        if record.pid == self.pid {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl TuiInstanceGuard {
    fn register() -> Result<Self> {
        ensure_data_dir()?;
        let path = record_path();
        let record = TuiInstanceRecord::current()?;
        write_record(&path, &record)?;
        Ok(Self {
            path,
            pid: record.pid,
        })
    }
}

async fn prepare_tui_launch() -> Result<LaunchOutcome> {
    ensure_data_dir()?;
    let _startup_lock = acquire_startup_lock().await?;
    let path = record_path();

    if let Some(record) = read_record(&path)? {
        if record.pid != std::process::id() && is_same_tui_process(&record) {
            if record.is_attachable()
                && crate::tui::interaction::execute_tmux_attach(
                    record.tmux_socket.as_deref(),
                    record.tmux_session.as_deref().unwrap_or_default(),
                    record.tmux_window.as_deref(),
                    record.tmux_pane.as_deref(),
                )
                .is_none()
            {
                return Ok(LaunchOutcome::Attached);
            }

            terminate_process(record.pid).await?;
        }

        let _ = std::fs::remove_file(&path);
    }

    Ok(LaunchOutcome::Start(TuiInstanceGuard::register()?))
}

async fn acquire_startup_lock() -> Result<StartupLock> {
    use nix::errno::Errno;
    use nix::libc::{flock, LOCK_EX, LOCK_NB};

    ensure_data_dir()?;
    let path = startup_lock_path();
    let deadline = std::time::Instant::now() + STARTUP_LOCK_TIMEOUT;

    loop {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open startup lock: {:?}", path))?;

        let lock_result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };

        match Errno::result(lock_result).map(drop) {
            Ok(()) => return Ok(StartupLock { _file: file }),
            Err(Errno::EWOULDBLOCK) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("Timed out waiting for ocwatch TUI startup lock");
                }

                tokio::time::sleep(STARTUP_LOCK_WAIT).await;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error))
                    .context("Failed to acquire ocwatch TUI startup lock");
            }
        }
    }
}

fn ensure_data_dir() -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create ocwatch data directory: {:?}", dir))
}

fn record_path() -> PathBuf {
    data_dir().join("tui.json")
}

fn startup_lock_path() -> PathBuf {
    data_dir().join("tui.lock")
}

fn read_record(path: &Path) -> Result<Option<TuiInstanceRecord>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read TUI singleton record: {:?}", path))?;

    match serde_json::from_str(&contents) {
        Ok(record) => Ok(Some(record)),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            Ok(None)
        }
    }
}

fn write_record(path: &Path, record: &TuiInstanceRecord) -> Result<()> {
    let temp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let contents = serde_json::to_vec(record)?;
    std::fs::write(&temp_path, contents)
        .with_context(|| format!("Failed to write temporary TUI record: {:?}", temp_path))?;
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("Failed to publish TUI singleton record: {:?}", path))
}

fn is_pid_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), None).is_ok()
}

fn is_same_tui_process(record: &TuiInstanceRecord) -> bool {
    if !is_pid_alive(record.pid) {
        return false;
    }

    let output = match std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &record.pid.to_string()])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let executable = command.split_whitespace().next().unwrap_or_default();
    let exe_path = Path::new(&record.exe_path);
    let exe_name = exe_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    executable == record.exe_path
        || executable == exe_name
        || executable.ends_with(&format!("/{}", exe_name))
}

async fn terminate_process(pid: u32) -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let pid = Pid::from_raw(pid as i32);
    let _ = kill(pid, Signal::SIGTERM);

    let deadline = std::time::Instant::now() + TUI_SHUTDOWN_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if kill(pid, None).is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = kill(pid, Signal::SIGKILL);

    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        if kill(pid, None).is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    anyhow::bail!("Failed to stop existing ocwatch TUI process {}", pid.as_raw())
}

fn current_tmux_target() -> Option<TmuxTarget> {
    let tmux_env = std::env::var("TMUX").ok()?;
    let socket_path = tmux_env.split(',').next()?.to_string();

    let output = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "#{session_name}\t#{window_id}\t#{pane_id}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut parts = stdout.trim().split('\t');

    Some(TmuxTarget {
        socket_path,
        session: parts.next()?.to_string(),
        window: parts.next()?.to_string(),
        pane: parts.next()?.to_string(),
    })
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
