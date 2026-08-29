use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use sun_remote_desktop::{admin, bridge, config, host, service};

#[derive(Debug, Parser)]
#[command(
    name = "sun-remote-desktop",
    version,
    about = "Share the local desktop through SunRDP"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the host in the current interactive user session.
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the desktop capture and input agent in the current Windows session.
    Agent {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the service-managed LocalSystem helper on a physical console desktop.
    #[command(hide = true)]
    ConsoleAgent {
        #[arg(long)]
        desktop: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Open the local administration window.
    Admin,
    /// Run as a Windows service.
    Service,
    /// Print the path of the active configuration file.
    ConfigPath,
}

fn main() -> Result<()> {
    let command = Cli::parse().command;
    init_tracing(matches!(
        &command,
        Command::Service | Command::ConsoleAgent { .. }
    ))?;

    match command {
        Command::Run { config: path } => {
            let path = path.unwrap_or_else(config::config_path);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(host::run_server(&path))
        }
        Command::Agent { config: path } => {
            let path = path.unwrap_or_else(config::config_path);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            run_agent(&runtime, &path)
        }
        Command::ConsoleAgent {
            desktop,
            config: path,
        } => {
            let desktop = bridge::windows::ConsoleDesktop::parse(&desktop)?;
            let path = path.unwrap_or_else(config::config_path);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(bridge::windows::run_console_agent(&path, desktop))
        }
        Command::Admin => admin::run(),
        Command::Service => service::run(),
        Command::ConfigPath => {
            println!("{}", config::config_path().display());
            Ok(())
        }
    }
}

fn init_tracing(service_mode: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if service_mode {
        let log_path = config::data_dir().join("sunrdp-service.log");
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create service log directory {}", parent.display()))?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open service log {}", log_path.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(Mutex::new(log))
            .try_init()
            .map_err(|error| anyhow::anyhow!("initialize service logging: {error}"))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init()
            .map_err(|error| anyhow::anyhow!("initialize console logging: {error}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn run_agent(runtime: &tokio::runtime::Runtime, path: &std::path::Path) -> Result<()> {
    runtime.block_on(bridge::windows::run_agent(path))
}

#[cfg(not(windows))]
fn run_agent(_runtime: &tokio::runtime::Runtime, _path: &std::path::Path) -> Result<()> {
    anyhow::bail!("当前会话代理只在 Windows 构建中可用")
}
