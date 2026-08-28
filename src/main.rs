use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use rdp_desktop_host::{admin, config, host, service};

#[derive(Debug, Parser)]
#[command(
    name = "rdp-desktop-host",
    version,
    about = "Share the local desktop through the RDP transport"
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
    /// Open the local administration window.
    Admin,
    /// Run as a Windows service.
    Service,
    /// Print the path of the active configuration file.
    ConfigPath,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Run { config: path } => {
            let path = path.unwrap_or_else(config::config_path);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(host::run_server(&path))
        }
        Command::Admin => admin::run(),
        Command::Service => service::run(),
        Command::ConfigPath => {
            println!("{}", config::config_path().display());
            Ok(())
        }
    }
}
