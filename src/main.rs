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
        Some(Commands::Debug { action }) => match action {
            DebugCommands::ScanLocal => debug_scan_local().await,
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
    eprintln!("TUI not yet implemented — Task 11");
    Ok(())
}

async fn run_approve(session_id: &str) -> Result<()> {
    eprintln!("Approve not yet implemented — Task 14");
    Ok(())
}

async fn debug_scan_local() -> Result<()> {
    let instances = discovery::local::scan_local_tmux().await;
    println!("{}", serde_json::to_string_pretty(&instances)?);
    Ok(())
}

async fn debug_scan_remote(host: &str) -> Result<()> {
    eprintln!("scan-remote not yet implemented — Task 9");
    println!("[]");
    Ok(())
}

async fn debug_ssh_check(host: &str) -> Result<()> {
    eprintln!("ssh-check not yet implemented — Task 8");
    Ok(())
}

async fn debug_oc_client(url: &str) -> Result<()> {
    eprintln!("oc-client not yet implemented — Task 6");
    Ok(())
}

async fn debug_ipc_roundtrip() -> Result<()> {
    eprintln!("ipc-roundtrip not yet implemented — Task 5");
    Ok(())
}

async fn debug_inject_event(session_id: &str, state: &str) -> Result<()> {
    eprintln!("inject-event not yet implemented — Task 10");
    Ok(())
}
