//! qfand — QNAP EC fan control daemon (§3, D2).
//! One binary, multiple subcommands: daemon / tui / status / export.

#![allow(clippy::todo)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "qfand",
    about = "QNAP EC fan control daemon for Unraid",
    version
)]
struct Cli {
    /// Path to the config TOML file.
    #[arg(
        short,
        long,
        default_value = "/boot/config/plugins/qnap-fan/config.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the fan control daemon. This process is the sole EC PWM writer (§3, D3).
    Daemon {
        /// Log intended PWM writes but do not write to the EC (§10, dry-run validation).
        #[arg(long)]
        dry_run: bool,
    },
    /// Open the interactive ratatui TUI (connects to a running daemon via socket).
    Tui,
    /// Print current daemon status and exit (reads from the daemon socket).
    Status,
    /// Export history data to stdout.
    Export {
        #[arg(long)]
        series: String,
        #[arg(long, value_parser = ["raw", "minute", "hour"])]
        tier: String,
        /// Unix timestamp for the start of the export window.
        #[arg(long)]
        from: u64,
        /// Unix timestamp for the end of the export window.
        #[arg(long)]
        to: u64,
        #[arg(long, default_value = "csv", value_parser = ["csv"])]
        format: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon { dry_run } => run_daemon(&cli.config, dry_run).await,
        Command::Tui => run_tui(&cli.config).await,
        Command::Status => run_status(&cli.config).await,
        Command::Export {
            series,
            tier,
            from,
            to,
            format,
        } => run_export(&cli.config, &series, &tier, from, to, &format).await,
    }
}

async fn run_daemon(_config: &std::path::Path, _dry_run: bool) -> Result<()> {
    todo!("T3: load config, discover hwmon, start control loop, listen on socket")
}

async fn run_tui(_config: &std::path::Path) -> Result<()> {
    todo!("T6: connect to daemon socket, render ratatui interface")
}

async fn run_status(_config: &std::path::Path) -> Result<()> {
    todo!("T5: connect to daemon socket, print StatusResponse")
}

async fn run_export(
    _config: &std::path::Path,
    _series: &str,
    _tier: &str,
    _from: u64,
    _to: u64,
    _format: &str,
) -> Result<()> {
    todo!("T4: open tier file, iterate slots in range, write CSV to stdout")
}
