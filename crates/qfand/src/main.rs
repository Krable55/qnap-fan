//! qfand — QNAP EC fan control daemon (§3, D2).
//! One binary, multiple subcommands: daemon / tui / status / export.

#![allow(clippy::todo)]
#![allow(clippy::redundant_closure)]
// Dead code is expected while T3/T4/T6 are split across parallel tasks.
// state.rs and ipc.rs items become live once run_daemon is implemented.
#![allow(dead_code)]

mod control;
mod ipc;
mod state;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qfan_hist::{ExportFormat, FileRingBuffer, Tier, Unit};
use qfan_proto::{Ack, Request, Response};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(
    name = "qfand",
    about = "QNAP EC fan control daemon for Unraid",
    version
)]
struct Cli {
    /// Path to the config TOML file.
    ///
    /// `global = true` so it is accepted both before and after the subcommand.
    /// `qfand daemon --config X` is the order an operator reaches for, and
    /// rc.qfand shipped that order in its start line — clap rejected it with
    /// "unexpected argument '--config' found" and the daemon never ran.
    #[arg(
        short,
        long,
        global = true,
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

async fn run_daemon(config_path: &Path, dry_run: bool) -> Result<()> {
    use qfan_hal::{HwmonPaths, SysfsBackend};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let config = match qfan_core::Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[qfand] config load failed ({e}), using defaults");
            qfan_core::Config::default()
        }
    };

    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let shared = state::Shared::new(dry_run, started_at);

    let paths = HwmonPaths::discover().context("hwmon discovery failed")?;
    let backend: Arc<dyn qfan_hal::EcBackend> = Arc::new(SysfsBackend::new(paths));

    // stop flag: set true to terminate the control loop
    let stop = Arc::new(AtomicBool::new(false));

    // Synchronous control loop on a blocking thread (R1: sole EC writer)
    let backend2 = Arc::clone(&backend);
    let shared2 = shared.clone();
    let config2 = config.clone();
    let stop2 = Arc::clone(&stop);
    let loop_handle = tokio::task::spawn_blocking(move || {
        control::run_loop(backend2, config2, shared2, dry_run, stop2);
    });

    // Socket server (pure reader — never writes PWM, never calls decide)
    let hist_base = PathBuf::from("/var/lib/qnap-fan/history");
    let sock_ctx = ipc::ServerCtx {
        shared: shared.clone(),
        config: Arc::new(config),
        hist_base,
    };
    tokio::spawn(async move {
        if let Err(e) = ipc::serve(sock_ctx, std::path::Path::new(ipc::SOCK_PATH)).await {
            eprintln!("[qfand] socket server error: {e:#}");
        }
    });

    // Wait for shutdown signal
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).context("SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("SIGINT handler")?;
        tokio::select! {
            _ = sigterm.recv() => eprintln!("[qfand] received SIGTERM"),
            _ = sigint.recv() => eprintln!("[qfand] received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.context("ctrl_c")?;

    // Signal control loop to stop, then wait for it (FailsafeGuard fires on exit)
    stop.store(true, Ordering::Relaxed);
    let _ = loop_handle.await;

    Ok(())
}

async fn run_tui(config: &Path) -> Result<()> {
    tui::run_tui(config).await
}

/// Connect to the daemon socket, send Status, and pretty-print the response.
///
/// Exits cleanly (no stack trace) when the daemon is not running — that is the
/// normal state before the daemon starts, not an error worth alarming over.
async fn run_status(_config: &Path) -> Result<()> {
    let path = ipc::SOCK_PATH;

    let stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            eprintln!("qfand is not running (no socket at {path})");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("connect {path}")),
    };

    let (r, mut w) = stream.into_split();
    let req = serde_json::to_string(&Request::Status).context("serialise request")?;
    w.write_all(req.as_bytes()).await.context("write request")?;
    w.write_all(b"\n").await.context("write newline")?;

    let mut reader = BufReader::new(r);
    let mut line = String::new();
    reader.read_line(&mut line).await.context("read response")?;

    if line.is_empty() {
        eprintln!("qfand returned an empty response");
        return Ok(());
    }

    let response: Response = serde_json::from_str(&line).context("parse response")?;

    match response {
        Response::Status(s) => {
            println!("uptime: {}s   mode: {}", s.uptime_s, s.mode);
            if let Some(pkg) = s.temps.cpu_package_c {
                println!("  package:  {:.1}°C", pkg);
            }
            let cores: Vec<String> = s
                .temps
                .cpu_cores_c
                .iter()
                .enumerate()
                .filter_map(|(i, v)| v.map(|c| format!("core{i}={:.1}°C", c)))
                .collect();
            if !cores.is_empty() {
                println!("  cores:    {}", cores.join("  "));
            }
            for (name, temp) in &s.temps.disk_temps_c {
                if let Some(t) = temp {
                    println!("  disk {name}: {:.1}°C", t);
                }
            }
            println!();
            for ch in &s.channels {
                print!(
                    "  {}  pwm={}  mode={}",
                    ch.channel, ch.intended_pwm, ch.mode
                );
                if let Some(rb) = ch.readback_pwm {
                    print!("  readback={rb}");
                }
                if let Some(rpm) = ch.rpm {
                    print!("  rpm={rpm}");
                }
                if let Some(exp) = ch.override_expires_at {
                    print!("  expires_at={exp}");
                }
                println!();
            }
        }
        Response::Ack(Ack::Error { message }) => {
            eprintln!("daemon error: {message}");
        }
        _ => {
            eprintln!("unexpected response type from daemon");
        }
    }

    Ok(())
}

/// Open the requested history tier file and write CSV to stdout.
///
/// Gap rows (ring wrapped) and no-data rows (count=0) have empty min/avg/max/count
/// fields — gaps are never rendered as 0.  See §6.3 and qfan_hist::export.
async fn run_export(
    config: &Path,
    series: &str,
    tier: &str,
    from: u64,
    to: u64,
    format: &str,
) -> Result<()> {
    if format != "csv" {
        anyhow::bail!("unsupported format {format:?}; only csv is supported");
    }

    let tier_enum = match tier {
        "hour" => Tier::Hour,
        "minute" => Tier::Minute,
        "raw" => {
            anyhow::bail!("tier 'raw' is in-process RAM only and cannot be exported from disk")
        }
        _ => anyhow::bail!("unknown tier {tier:?}; use minute or hour"),
    };

    // T2 (hour) lives under the same directory as the config file (flash).
    // T1 (minute) lives in appdata (§6.2).
    let base: PathBuf = match tier_enum {
        Tier::Hour => config
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
            .join("history/hour"),
        Tier::Minute => PathBuf::from("/mnt/user/appdata/qnap-fan/history/minute"),
        Tier::Raw => unreachable!(),
    };

    let path = base.join(format!("{series}.qfrb"));

    if !path.exists() {
        anyhow::bail!("history file not found: {}", path.display());
    }

    let mut rb = FileRingBuffer::open_or_create(
        &path,
        tier_enum,
        &qfan_core::Series::CpuPackage, // only used when creating; file exists
        Unit::DeciCelsius,
        0,
    )
    .with_context(|| format!("opening {}", path.display()))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    qfan_hist::export(&mut rb, from, to, ExportFormat::Csv, &mut out).context("export")
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    /// The shipped rc.qfand start line must parse. F15: the plugin shipped
    /// `qfand daemon --config X`, clap rejected it with "unexpected argument
    /// '--config' found", the daemon never started, and rc.qfand reported
    /// success anyway. Both orders must work, because both are things a human
    /// or a script will actually type.
    #[test]
    fn config_flag_accepted_before_and_after_subcommand() {
        let before = Cli::try_parse_from(["qfand", "--config", "/tmp/a.toml", "daemon"])
            .expect("--config before the subcommand must parse");
        assert_eq!(before.config, PathBuf::from("/tmp/a.toml"));

        let after = Cli::try_parse_from(["qfand", "daemon", "--config", "/tmp/b.toml"])
            .expect("--config after the subcommand must parse (the order rc.qfand shipped)");
        assert_eq!(after.config, PathBuf::from("/tmp/b.toml"));
    }

    /// The exact command line the shipped rc.qfand uses, including --dry-run.
    #[test]
    fn shipped_rc_start_lines_parse() {
        for args in [
            vec![
                "qfand",
                "--config",
                "/boot/config/plugins/qnap-fan/config.toml",
                "daemon",
            ],
            vec![
                "qfand",
                "daemon",
                "--config",
                "/boot/config/plugins/qnap-fan/config.toml",
            ],
            vec!["qfand", "daemon", "--dry-run"],
            vec!["qfand", "--config", "/tmp/c.toml", "daemon", "--dry-run"],
            vec!["qfand", "status"],
            vec!["qfand", "tui"],
        ] {
            Cli::try_parse_from(args.clone())
                .unwrap_or_else(|e| panic!("{args:?} must parse, got: {e}"));
        }
    }

    /// rc.qfand greps its own start line out of this file in the packaging test
    /// below; keep the daemon subcommand name stable.
    #[test]
    fn daemon_subcommand_is_named_daemon() {
        let cli = Cli::try_parse_from(["qfand", "daemon"]).expect("must parse");
        assert!(matches!(cli.command, Command::Daemon { .. }));
    }
}
