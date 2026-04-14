#![allow(dead_code, unused_variables, unused_imports)]

mod config;
mod types;
mod agent_trait;
mod ipc;
mod daemon;
mod opencode;
mod discovery;
mod ssh;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(name = "ocwatch")]
#[command(about = "OpenCode session monitor TUI", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonCommands,
    },
    /// Quick-approve a pending permission request
    Approve {
        /// Session ID to approve
        session_id: String,
    },
    /// Create a new local tmux session in the current directory and launch opencode
    New,
    /// Debug utilities (for development and QA)
    Debug {
        #[command(subcommand)]
        action: DebugCommands,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the ocwatch daemon in the background
    Start,
    /// Stop the running daemon
    Stop,
    /// Print daemon status as JSON
    Status,
    /// Run daemon in foreground (internal use by 'daemon start')
    #[command(hide = true)]
    Run,
}

#[derive(Subcommand)]
enum DebugCommands {
    /// Scan local tmux panes for OpenCode processes
    ScanLocal,
    /// Scan local using new TUI-based active session detection
    ScanLocalV2,
    /// Scan a remote host for OpenCode processes
    ScanRemote {
        /// Host name from config
        host: String,
    },
    /// Test SSH ControlMaster connection to a host
    SshCheck {
        /// Host name from config
        host: String,
    },
    /// Query OpenCode HTTP API at URL
    OcClient {
        /// Base URL, e.g. http://localhost:4096
        url: String,
    },
    /// Connect to daemon socket, send GetStatus, print response
    IpcRoundtrip,
    /// Inject a synthetic state change for QA testing
    InjectEvent {
        /// Session ID
        session_id: String,
        /// State: idle, busy, error, waiting_for_permission, waiting_for_input
        state: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("OCWATCH_LOG")
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        None => run_tui().await,
        Some(Commands::Daemon { action }) => match action {
            DaemonCommands::Start => daemon::lifecycle::start_daemon().await,
            DaemonCommands::Stop => daemon::lifecycle::stop_daemon().await,
            DaemonCommands::Status => daemon::lifecycle::daemon_status().await,
            DaemonCommands::Run => daemon::lifecycle::run_daemon().await,
        },
        Some(Commands::Approve { session_id }) => run_approve(&session_id).await,
        Some(Commands::New) => run_new().await,
        Some(Commands::Debug { action }) => match action {
            DebugCommands::ScanLocal => debug_scan_local().await,
            DebugCommands::ScanLocalV2 => debug_scan_local_v2().await,
            DebugCommands::ScanRemote { host } => debug_scan_remote(&host).await,
            DebugCommands::SshCheck { host } => debug_ssh_check(&host).await,
            DebugCommands::OcClient { url } => debug_oc_client(&url).await,
            DebugCommands::IpcRoundtrip => debug_ipc_roundtrip().await,
            DebugCommands::InjectEvent { session_id, state } => {
                debug_inject_event(&session_id, &state).await
            }
        },
    }
}

async fn run_tui() -> Result<()> {
    tui::run_tui().await
}

async fn run_approve(session_id: &str) -> Result<()> {
    use anyhow::Context;
    use tokio::io::BufReader;

    let stream = ipc::connect_to_daemon()
        .await
        .context("Failed to connect to daemon")?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    ipc::send_message(
        &mut write_half,
        &ipc::ClientMessage::Approve {
            session_id: session_id.to_string(),
        },
    )
    .await
    .context("Failed to send approve message")?;

    match ipc::read_message::<ipc::DaemonMessage>(&mut reader).await {
        Ok(Some(ipc::DaemonMessage::SessionUpdated { session })) => {
            println!("Approved: {} → {}", session.id, session.state);
        }
        Ok(Some(ipc::DaemonMessage::Error { message })) => {
            anyhow::bail!("Daemon error: {}", message);
        }
        Ok(Some(other)) => {
            println!("{}", serde_json::to_string(&other)?);
        }
        Ok(None) | Err(_) => {
            println!("Approved session {}", session_id);
        }
    }

    Ok(())
}

async fn run_new() -> Result<()> {
    use anyhow::Context;
    use tokio::io::BufReader;

    let cwd = std::env::current_dir().context("Failed to resolve current directory")?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Current directory is not valid UTF-8"))?
        .to_string();
    let basename = infer_name_from_directory(&cwd);

    let stream = ipc::connect_to_daemon()
        .await
        .context("Failed to connect to daemon")?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    ipc::send_message(
        &mut write_half,
        &ipc::ClientMessage::CreateSession {
            host: "local".to_string(),
            directory: cwd,
            name_hint: Some(basename),
        },
    )
    .await
    .context("Failed to request session creation")?;

    match ipc::read_message::<ipc::DaemonMessage>(&mut reader).await {
        Ok(Some(ipc::DaemonMessage::AttachReady { attach })) => {
            if let Some(message) = tui::interaction::execute_attach(attach) {
                anyhow::bail!(message);
            }
            Ok(())
        }
        Ok(Some(ipc::DaemonMessage::Error { message })) => anyhow::bail!(message),
        Ok(Some(other)) => anyhow::bail!("Unexpected daemon response: {}", serde_json::to_string(&other)?),
        Ok(None) => anyhow::bail!("Daemon closed the connection before returning attach info"),
        Err(error) => Err(error).context("Failed to read daemon response"),
    }
}

fn infer_name_from_directory(directory: &str) -> String {
    Path::new(directory)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("opencode")
        .to_string()
}

async fn debug_scan_local() -> Result<()> {
    let instances = discovery::local::scan_local_tmux().await;
    println!("{}", serde_json::to_string_pretty(&instances)?);
    Ok(())
}

async fn debug_scan_local_v2() -> Result<()> {
    let result = discovery::local::scan_local().await;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn debug_scan_remote(host: &str) -> Result<()> {
    let config = config::Config::load()?;
    let host_config = config
        .hosts
        .iter()
        .find(|h| h.name == host)
        .ok_or_else(|| anyhow::anyhow!("Host '{}' not found in config", host))?;

    let mut ssh_manager = ssh::SshManager::new();
    ssh_manager.connect(host_config).await?;

    let instances = discovery::remote::scan_remote(&mut ssh_manager, host).await;
    println!("{}", serde_json::to_string_pretty(&instances)?);

    ssh_manager.disconnect_all().await;
    Ok(())
}

async fn debug_ssh_check(host: &str) -> Result<()> {
    let config = config::Config::load()?;
    ssh::manager::debug_ssh_check(host, &config).await
}

async fn debug_oc_client(url: &str) -> Result<()> {
    opencode::client::debug_query(url).await
}

async fn debug_ipc_roundtrip() -> Result<()> {
    use anyhow::Context;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use crate::ipc::{connect_to_daemon, ClientMessage};

    let stream = connect_to_daemon().await
        .context("Failed to connect to daemon")?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let msg = serde_json::to_string(&ClientMessage::GetStatus)? + "\n";
    write_half.write_all(msg.as_bytes()).await?;
    write_half.flush().await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;

    match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{}", line.trim()),
    }
    Ok(())
}

async fn debug_inject_event(session_id: &str, state: &str) -> Result<()> {
    use anyhow::Context;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use crate::ipc::{connect_to_daemon, ClientMessage};

    let stream = connect_to_daemon().await
        .context("Failed to connect to daemon")?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let msg = serde_json::to_string(&ClientMessage::InjectEvent {
        session_id: session_id.to_string(),
        state: state.to_string(),
    })? + "\n";
    write_half.write_all(msg.as_bytes()).await?;
    write_half.flush().await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if !line.trim().is_empty() {
        println!("{}", line.trim());
    }
    Ok(())
}
