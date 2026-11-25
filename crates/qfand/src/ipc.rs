//! Socket API server (§5, T6).
//!
//! This module is a **pure reader** of daemon state. It never writes PWM,
//! never calls `qfan_core::decide`, and never touches sysfs. Mutations happen
//! by queuing a `state::Command` for the control loop to apply on its next tick.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use qfan_core::{Config, ControlMode, Override};
use qfan_hist::{FileRingBuffer, Tier, Unit, NO_DATA};
use qfan_proto::{
    Ack, ChannelStatus, HistoryResponse, HistorySlot, Request, Response, StatusResponse,
    TempSnapshot,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{timeout, Duration};

use crate::state::{Command, Shared};

pub const SOCK_PATH: &str = "/var/run/qfand.sock";
const READ_TIMEOUT_S: u64 = 30;
const MAX_LINE_BYTES: usize = 65_536;

/// Context the socket server needs to serve all request types.
#[derive(Clone)]
pub struct ServerCtx {
    pub shared: Shared,
    /// The daemon's live config: what `{"op":"config"}` reports, and the base
    /// a `set_curve` is validated against. Updated only when a curve is
    /// accepted, so clients (the TUI curve editor) can load the running curve.
    pub config: Arc<Mutex<Config>>,
    /// Where accepted curves are saved so they survive a restart. `None` in
    /// dry-run and tests: the curve still applies, nothing is written.
    pub config_path: Option<PathBuf>,
    /// Root directory for the HOUR tier (flash).
    /// Structure: `<hist_hour_base>/hour/<series>.qfrb`.
    pub hist_hour_base: PathBuf,
    /// Root directory for the MINUTE tier (appdata), or `None` when appdata is
    /// not mounted. The two tiers have DIFFERENT roots by design (D2), so a
    /// single `hist_base` would resolve `tier=minute` under the flash root and
    /// return empty forever — the F17 failure mode, reintroduced one layer up.
    pub hist_minute_base: Option<PathBuf>,
}

impl ServerCtx {
    /// Resolve the directory holding a tier's files, or `None` if that tier is
    /// not currently stored anywhere.
    fn tier_base(&self, tier: Tier) -> Option<&Path> {
        match tier {
            Tier::Hour => Some(self.hist_hour_base.as_path()),
            Tier::Minute => self.hist_minute_base.as_deref(),
            // T0 is an in-process RAM ring; it has no files.
            Tier::Raw => None,
        }
    }
}

/// Bind the socket, set permissions 0660, and accept clients in a loop.
///
/// Unlinks any stale socket file before binding (§5) so a crashed daemon
/// never permanently blocks its own restart.  chmod 0660 keeps the socket
/// accessible to root and the webGUI group without being world-writable —
/// Override is a thermally dangerous operation.
pub async fn serve(ctx: ServerCtx, sock_path: &Path) -> Result<()> {
    // Remove stale socket; ignore errors (file may not exist).
    let _ = std::fs::remove_file(sock_path);

    let listener =
        UnixListener::bind(sock_path).with_context(|| format!("bind {}", sock_path.display()))?;

    std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("chmod 0660 {}", sock_path.display()))?;

    loop {
        let (stream, _) = listener.accept().await.context("accept")?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, ctx).await {
                eprintln!("ipc: client error: {e:#}");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, ctx: ServerCtx) -> Result<()> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let result = timeout(
            Duration::from_secs(READ_TIMEOUT_S),
            read_limited_line(&mut reader, MAX_LINE_BYTES),
        )
        .await;

        match result {
            Err(_elapsed) => {
                // Client went silent: drop it without stalling the daemon.
                break;
            }
            Ok(Err(e)) => {
                // Over-long line or I/O error: send error, drop connection.
                let resp = Response::Ack(Ack::Error {
                    message: e.to_string(),
                });
                send_response(&mut writer, &resp).await.ok();
                break;
            }
            Ok(Ok(None)) => break, // EOF
            Ok(Ok(Some(bytes))) => {
                let line = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.trim_end_matches(['\n', '\r']),
                    Err(_) => {
                        let resp = Response::Ack(Ack::Error {
                            message: "request must be UTF-8".into(),
                        });
                        send_response(&mut writer, &resp).await.ok();
                        continue;
                    }
                };

                let resp = dispatch(line, &ctx).await;
                if let Err(e) = send_response(&mut writer, &resp).await {
                    eprintln!("ipc: write error: {e:#}");
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn send_response<W: AsyncWriteExt + Unpin>(writer: &mut W, resp: &Response) -> Result<()> {
    let json = serde_json::to_string(resp).context("serialise response")?;
    writer
        .write_all(json.as_bytes())
        .await
        .context("write json")?;
    writer.write_all(b"\n").await.context("write newline")?;
    Ok(())
}

/// Read one newline-terminated line, capped at `limit` bytes.
///
/// Returns `None` on EOF with no data, `Some(bytes)` on success (including
/// the newline), and `Err` when the line exceeds `limit`.  Never allocates
/// more than `limit` bytes regardless of what the client sends.
async fn read_limited_line<R>(reader: &mut BufReader<R>, limit: usize) -> Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(256);

    loop {
        // Keep the fill_buf borrow inside its own block so it is dropped
        // before we call consume(), which also needs &mut reader.
        let (n, found, over_limit) = {
            let chunk = reader.fill_buf().await.context("fill_buf")?;
            if chunk.is_empty() {
                return Ok(if buf.is_empty() { None } else { Some(buf) });
            }
            let pos = chunk.iter().position(|&b| b == b'\n');
            let n = pos.map_or(chunk.len(), |p| p + 1);
            let over = buf.len() + n > limit;
            if !over {
                buf.extend_from_slice(&chunk[..n]);
            }
            (n, pos.is_some(), over)
        };
        // fill_buf borrow dropped here.
        reader.consume(n);

        if over_limit {
            return Err(anyhow::anyhow!(
                "request line exceeds {limit}-byte limit; connection dropped"
            ));
        }
        if found {
            return Ok(Some(buf));
        }
    }
}

async fn dispatch(line: &str, ctx: &ServerCtx) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Response::Ack(Ack::Error {
                message: format!("malformed request: {e}"),
            })
        }
    };

    match req {
        Request::Status => handle_status(ctx),
        Request::Config => handle_config(ctx),
        Request::History {
            series,
            tier,
            from,
            to,
        } => handle_history(ctx, series, tier, from, to).await,
        Request::SetCurve { channel, points } => handle_set_curve(ctx, channel, points),
        Request::Override {
            channel,
            pwm,
            ttl_s,
        } => handle_override(ctx, channel, pwm, ttl_s),
        Request::ClearOverride { channel } => handle_clear_override(ctx, channel),
        Request::Events { since } => handle_events(since),
    }
}

fn handle_status(ctx: &ServerCtx) -> Response {
    let p = ctx.shared.snapshot();
    let now = unix_now();

    let channels = p
        .channels
        .iter()
        .map(|ch| {
            let (mode_str, exp) = match &ch.mode {
                ControlMode::Curve => ("curve".into(), None),
                ControlMode::Override { expires_at_secs } => {
                    ("override".into(), Some(*expires_at_secs))
                }
                ControlMode::Emergency => ("emergency".into(), None),
                ControlMode::FailSafe => ("failsafe".into(), None),
            };
            ChannelStatus {
                channel: ch.channel.to_string(),
                intended_pwm: ch.intended_pwm,
                readback_pwm: ch.readback_pwm,
                rpm: ch.rpm,
                mode: mode_str,
                override_expires_at: exp,
            }
        })
        .collect();

    let temps = TempSnapshot {
        cpu_package_c: p.temps.cpu_package_c.map(|v| v as f32 / 10.0),
        cpu_cores_c: p
            .temps
            .cpu_cores_c
            .iter()
            .map(|v| v.map(|x| x as f32 / 10.0))
            .collect(),
        disk_temps_c: p
            .temps
            .disk_temps_c
            .iter()
            .map(|(n, v)| (n.clone(), v.map(|x| x as f32 / 10.0)))
            .collect(),
    };

    Response::Status(StatusResponse {
        channels,
        temps,
        uptime_s: now.saturating_sub(p.started_at_secs),
        mode: if p.emergency {
            "emergency".into()
        } else {
            "normal".into()
        },
    })
}

fn handle_config(ctx: &ServerCtx) -> Response {
    let live = ctx.config.lock().unwrap_or_else(PoisonError::into_inner);
    match serde_json::to_value(&*live) {
        Ok(v) => Response::Config(v),
        Err(e) => Response::Ack(Ack::Error {
            message: format!("config serialisation: {e}"),
        }),
    }
}

async fn handle_history(
    ctx: &ServerCtx,
    series: String,
    tier: String,
    from: u64,
    to: u64,
) -> Response {
    let tier_enum = match parse_tier(&tier) {
        Some(t) => t,
        None => {
            return Response::Ack(Ack::Error {
                message: format!("unknown tier {tier:?}; use raw, minute, or hour"),
            })
        }
    };

    let Some(base) = ctx.tier_base(tier_enum) else {
        // Not an error: the raw tier is RAM-only, and the minute tier is absent
        // until appdata mounts. An empty series is the honest answer, and the
        // gap sentinel keeps a client from drawing it as 0.
        return Response::History(HistoryResponse {
            series,
            tier,
            slots: vec![],
        });
    };
    let path = base
        .join(crate::history::tier_dir(tier_enum))
        .join(format!("{series}.qfrb"));
    let series_ret = series.clone();
    let tier_ret = tier.clone();

    let result = tokio::task::spawn_blocking(move || -> Result<Vec<HistorySlot>> {
        // Return empty rather than creating a file for a series with no history.
        if !path.exists() {
            return Ok(vec![]);
        }

        let mut rb = FileRingBuffer::open_or_create(
            &path,
            tier_enum,
            &qfan_core::Series::CpuPackage, // only used when creating; file exists
            Unit::DeciCelsius,
            0,
        )?;

        let interval = tier_enum.interval_s() as u64;
        let from_bucket = (from / interval) as u32;
        let to_bucket = (to / interval) as u32;

        let mut slots = Vec::new();
        for b in from_bucket..=to_bucket {
            let slot = match rb.read_slot(b)? {
                // Gap: ring wrapped past this bucket.  Use NO_DATA sentinel
                // (i16::MIN = -32768) so callers can distinguish downtime from
                // a genuine zero reading — a chart rendering gaps as 0°C is lying.
                None => HistorySlot {
                    bucket: b,
                    min: NO_DATA,
                    avg: NO_DATA,
                    max: NO_DATA,
                    count: 0,
                },
                Some(s) => HistorySlot {
                    bucket: s.bucket,
                    min: s.min,
                    avg: s.avg,
                    max: s.max,
                    count: s.count,
                },
            };
            slots.push(slot);
        }
        Ok(slots)
    })
    .await;

    match result {
        Ok(Ok(slots)) => Response::History(HistoryResponse {
            series: series_ret,
            tier: tier_ret,
            slots,
        }),
        Ok(Err(e)) => Response::Ack(Ack::Error {
            message: format!("history: {e:#}"),
        }),
        Err(e) => Response::Ack(Ack::Error {
            message: format!("history task panic: {e}"),
        }),
    }
}

/// Validate, save, then apply a curve, in that order, and answer only once
/// the outcome is known.
///
/// Validation used to happen later, in the control loop, after this handler had
/// already answered `ok`: a rejected curve looked accepted to every client and
/// the reason went only to the daemon log. The control loop still re-validates
/// (defence in depth); with identical inputs it reaches the same verdict.
///
/// Saving comes before applying so the running curve and the file the next
/// start loads never disagree. A curve that cannot be saved is not applied.
fn handle_set_curve(
    ctx: &ServerCtx,
    channel: String,
    points: Vec<qfan_core::CurvePoint>,
) -> Response {
    // Held across validate -> save -> apply so two clients cannot interleave
    // and leave the file and the running curve describing different curves.
    let mut live = ctx.config.lock().unwrap_or_else(PoisonError::into_inner);

    // Preserve source/slew/hysteresis; only the breakpoints change.
    let mut candidate = live.clone();
    let slot = match channel.as_str() {
        "pwm7" => &mut candidate.pwm7,
        "pwm1" => &mut candidate.pwm1,
        _ => {
            return Response::Ack(Ack::Error {
                message: format!("unknown channel {channel:?}; use pwm1 or pwm7"),
            })
        }
    };
    slot.breakpoints = points;
    let new_cfg = slot.clone();

    if let Err(e) = candidate.validate() {
        return Response::Ack(Ack::Error {
            message: format!("curve rejected: {e}"),
        });
    }

    match &ctx.config_path {
        Some(path) => {
            if let Err(e) = crate::config_file::save_curve(path, &channel, &new_cfg) {
                return Response::Ack(Ack::Error {
                    message: format!("curve not applied: could not save it: {e:#}"),
                });
            }
        }
        None => eprintln!("[qfand] {channel} curve applied in memory only (dry-run: not saved)"),
    }

    *live = candidate;
    ctx.shared.submit(Command::SetCurve {
        channel,
        config: new_cfg,
    });
    Response::Ack(Ack::Ok)
}

/// Reject any channel the daemon does not drive.
///
/// `handle_set_curve` always validated this; the override handlers did not, so a
/// client could POST `channel: "pwm21"` and receive `Ack::Ok` for a request the
/// control loop then silently dropped. No write ever reached the EC — qfan-hal's
/// enums cannot name pwm21 (R6) — but reporting success for a no-op is the wrong
/// failure mode on a thermally dangerous operation.
fn check_channel(channel: &str) -> Option<Response> {
    match channel {
        "pwm1" | "pwm7" => None,
        _ => Some(Response::Ack(Ack::Error {
            message: format!("unknown channel {channel:?}; use pwm1 or pwm7"),
        })),
    }
}

fn handle_override(ctx: &ServerCtx, channel: String, pwm: u8, ttl_s: u32) -> Response {
    if let Some(err) = check_channel(&channel) {
        return err;
    }
    // Override::new enforces 1..=3600 TTL and the 35% duty floor.
    // Surface its error verbatim — the API contract says so, and the error
    // message was written by the safety author, not invented here.
    match Override::new(pwm, ttl_s, unix_now()) {
        Ok(ov) => {
            ctx.shared.submit(Command::SetOverride { channel, ov });
            Response::Ack(Ack::Ok)
        }
        Err(e) => Response::Ack(Ack::Error {
            message: e.to_string(),
        }),
    }
}

fn handle_clear_override(ctx: &ServerCtx, channel: String) -> Response {
    if let Some(err) = check_channel(&channel) {
        return err;
    }
    ctx.shared.submit(Command::ClearOverride { channel });
    Response::Ack(Ack::Ok)
}

fn handle_events(_since: u64) -> Response {
    // Event log is populated by T4 (control loop). Until state::Published
    // includes an event buffer this returns an empty list; callers must
    // tolerate it rather than treating it as an error.
    Response::Config(serde_json::json!({ "events": [] }))
}

fn parse_tier(s: &str) -> Option<Tier> {
    match s {
        "raw" => Some(Tier::Raw),
        "minute" => Some(Tier::Minute),
        "hour" => Some(Tier::Hour),
        _ => None,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use qfan_core::Config;
    use qfan_hist::{FileRingBuffer, Slot, Tier, Unit};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    fn test_ctx(dir: &TempDir) -> ServerCtx {
        ServerCtx {
            shared: Shared::new(false, 0),
            config: Arc::new(Mutex::new(Config::default())),
            config_path: None,
            hist_hour_base: dir.path().to_path_buf(),
            hist_minute_base: Some(dir.path().to_path_buf()),
        }
    }

    /// Start a server in the background and wait for the socket to appear.
    async fn start_server(dir: &TempDir) -> (PathBuf, ServerCtx) {
        let sock = dir.path().join("test.sock");
        let ctx = test_ctx(dir);
        let sock2 = sock.clone();
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            serve(ctx2, &sock2).await.ok();
        });
        // Poll until the socket file exists (avoids a fixed sleep).
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        (sock, ctx)
    }

    /// F17 end-to-end: the control loop's history WRITER and the socket
    /// READER must agree on paths. Both sides passed their own tests while the
    /// daemon recorded nothing for a year of intended retention, because
    /// nothing exercised the seam. This test owns that seam: it records through
    /// the real `HistoryWriter`, then queries over the real socket.
    #[tokio::test]
    async fn history_written_by_the_writer_is_served_by_the_socket() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        // Write via the production writer, pointed at the server's roots.
        let paths = crate::history::HistoryPaths {
            hour_base: dir.path().to_path_buf(),
            minute_base: Some(dir.path().to_path_buf()),
        };
        let (w, handle) = crate::history::spawn(paths);
        let t = 1_700_000_000u64;
        w.record(qfan_core::Series::CpuPackage, Unit::DeciCelsius, 571, t);
        drop(w);
        // join() is blocking; on the single-threaded test runtime calling it
        // directly deadlocks the executor. Hand it to the blocking pool.
        tokio::task::spawn_blocking(move || handle.join().unwrap())
            .await
            .unwrap();

        // Read back over the socket, for both file-backed tiers. The range must
        // be realistic: handle_history visits every bucket in it, and 0..10^10
        // is ~166 million minute buckets, which spun this test for 10+ minutes.
        let (from, to) = (t - 7200, t + 7200);
        for tier in ["hour", "minute"] {
            let req = format!(
                r#"{{"op":"history","series":"cpu.package","tier":"{tier}","from":{from},"to":{to}}}"#
            );
            let resp = send_recv(&sock, &req).await;
            assert!(
                resp.contains("571"),
                "tier {tier}: writer stored 571 but the socket returned: {resp}"
            );
            assert!(
                !resp.contains(r#""slots":[]"#),
                "tier {tier}: socket returned an empty series — the F17 bug, \
                 where the API answers correctly and the data is elsewhere: {resp}"
            );
        }
    }

    /// The raw tier is a RAM ring with no files. It must answer empty rather
    /// than erroring or inventing a path.
    #[tokio::test]
    async fn raw_tier_has_no_files_and_answers_empty() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let resp = send_recv(
            &sock,
            r#"{"op":"history","series":"cpu.package","tier":"raw","from":0,"to":9999999999}"#,
        )
        .await;
        assert!(
            resp.contains(r#""slots":[]"#),
            "raw tier must be empty: {resp}"
        );
        assert!(!resp.contains("error"), "raw tier is not an error: {resp}");
    }

    /// Connect, send one request, return the raw response line.
    async fn send_recv(sock: &Path, req: &str) -> String {
        let stream = UnixStream::connect(sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(req.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    // ── Round-trips ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn round_trip_status() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let resp = send_recv(&sock, r#"{"op":"status"}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v.get("channels").is_some(), "status must have 'channels'");
        assert!(v.get("uptime_s").is_some(), "status must have 'uptime_s'");
        assert!(v.get("temps").is_some(), "status must have 'temps'");
        assert!(v.get("mode").is_some(), "status must have 'mode'");
    }

    #[tokio::test]
    async fn round_trip_config() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let resp = send_recv(&sock, r#"{"op":"config"}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        // Config is a JSON object (not an Ack error).
        assert!(v.is_object(), "config response must be a JSON object");
        assert!(v.get("pwm7").is_some(), "config must have 'pwm7'");
    }

    #[tokio::test]
    async fn round_trip_history_no_file() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({
            "op": "history",
            "series": "cpu.package",
            "tier": "minute",
            "from": 0_u64,
            "to": 3600_u64
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(
            v.get("slots").is_some(),
            "history response must have 'slots'"
        );
        assert_eq!(
            v["slots"].as_array().unwrap().len(),
            0,
            "empty history for absent file"
        );
    }

    #[tokio::test]
    async fn round_trip_set_curve() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({
            "op": "set_curve",
            "channel": "pwm7",
            "points": [{"temp_c": 45, "pwm": 90}, {"temp_c": 70, "pwm": 255}]
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], "ok");
    }

    fn set_curve_req(channel: &str, points: serde_json::Value) -> String {
        serde_json::json!({"op": "set_curve", "channel": channel, "points": points}).to_string()
    }

    /// An unsafe curve must be refused in the reply itself. The handler used to
    /// answer `ok` and leave validation to the control loop, so the TUI showed
    /// success while the daemon logged the rejection and kept the old curve.
    #[tokio::test]
    async fn unsafe_curve_is_rejected_in_the_reply() {
        let dir = TempDir::new().unwrap();
        let (sock, ctx) = start_server(&dir).await;
        // 60 PWM at 80 °C is below the 35% delivered-duty floor.
        let req = set_curve_req(
            "pwm7",
            serde_json::json!([{"temp_c": 45, "pwm": 60}, {"temp_c": 80, "pwm": 60}]),
        );

        let v: serde_json::Value = serde_json::from_str(&send_recv(&sock, &req).await).unwrap();

        assert_eq!(v["result"], "error", "{v}");
        assert!(
            v["message"].as_str().unwrap().contains("35%"),
            "reason reaches the client: {v}"
        );
        let live = ctx.config.lock().unwrap();
        assert_eq!(
            live.pwm7.breakpoints,
            Config::default().pwm7.breakpoints,
            "running curve untouched"
        );
        assert!(
            ctx.shared.drain().is_empty(),
            "nothing queued for the control loop"
        );
    }

    #[tokio::test]
    async fn out_of_order_points_are_rejected_in_the_reply() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = set_curve_req(
            "pwm7",
            serde_json::json!([{"temp_c": 70, "pwm": 255}, {"temp_c": 45, "pwm": 90}]),
        );
        let v: serde_json::Value = serde_json::from_str(&send_recv(&sock, &req).await).unwrap();
        assert_eq!(v["result"], "error", "{v}");
        assert!(v["message"].as_str().unwrap().contains("monotonic"), "{v}");
    }

    /// The TUI editor loads the running curve from `{"op":"config"}`. That op
    /// used to return the startup snapshot forever, so it must reflect an
    /// accepted curve immediately.
    #[tokio::test]
    async fn config_op_reports_the_accepted_curve() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let pts = serde_json::json!([{"temp_c": 50, "pwm": 100}, {"temp_c": 72, "pwm": 255}]);
        let ack = send_recv(&sock, &set_curve_req("pwm7", pts.clone())).await;
        assert!(ack.contains(r#""result":"ok""#), "{ack}");

        let v: serde_json::Value =
            serde_json::from_str(&send_recv(&sock, r#"{"op":"config"}"#).await).unwrap();

        assert_eq!(v["pwm7"]["breakpoints"], pts);
        assert_eq!(
            v["pwm1"]["breakpoints"],
            serde_json::to_value(Config::default().pwm1.breakpoints).unwrap()
        );
    }

    /// An accepted curve is written to the config file, so a restart keeps it.
    /// A rejected one leaves the file alone.
    #[tokio::test]
    async fn accepted_curve_is_saved_and_rejected_one_is_not() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, include_str!("../../../plugin/config.default.toml")).unwrap();
        let sock = dir.path().join("save.sock");
        let ctx = ServerCtx {
            config_path: Some(path.clone()),
            ..test_ctx(&dir)
        };
        let ctx2 = ctx.clone();
        let sock2 = sock.clone();
        tokio::spawn(async move { serve(ctx2, &sock2).await.ok() });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let good = serde_json::json!([{"temp_c": 30, "pwm": 90}, {"temp_c": 55, "pwm": 200}]);
        let ack = send_recv(&sock, &set_curve_req("pwm1", good)).await;
        assert!(ack.contains(r#""result":"ok""#), "{ack}");
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.pwm1.breakpoints,
            ctx.config.lock().unwrap().pwm1.breakpoints
        );
        assert_eq!(saved.pwm1.breakpoints[1].pwm, 200);

        let before = std::fs::read_to_string(&path).unwrap();
        let bad = serde_json::json!([{"temp_c": 30, "pwm": 10}, {"temp_c": 90, "pwm": 10}]);
        let ack = send_recv(&sock, &set_curve_req("pwm1", bad)).await;
        assert!(ack.contains(r#""result":"error""#), "{ack}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "rejected curve not saved"
        );
    }

    #[tokio::test]
    async fn round_trip_override_valid() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({
            "op": "override",
            "channel": "pwm7",
            "pwm": 200,
            "ttl_s": 600
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], "ok");
    }

    #[tokio::test]
    async fn round_trip_clear_override() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({"op": "clear_override", "channel": "pwm7"}).to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], "ok");
    }

    #[tokio::test]
    async fn round_trip_events() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({"op": "events", "since": 0_u64}).to_string();
        let resp = send_recv(&sock, &req).await;
        // Must be valid JSON (not a parse error).
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        // Events returns either a JSON object (with "events" key) or Ack.
        assert!(v.is_object(), "events response must be a JSON object");
    }

    // ── Socket hygiene ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stale_socket_cleaned_up() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("stale.sock");
        // Leave a stale regular file at the socket path.
        std::fs::write(&sock, b"stale data").unwrap();

        let ctx = test_ctx(&dir);
        let sock2 = sock.clone();
        tokio::spawn(async move {
            serve(ctx, &sock2).await.ok();
        });

        // Don't just check path existence — the stale file was already there.
        // Retry the connection until it succeeds (proves the stale file was replaced
        // by an actual listening socket).
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if UnixStream::connect(&sock).await.is_ok() {
                return; // successfully connected — stale socket was cleaned up
            }
        }
        panic!("server did not become reachable after stale socket cleanup");
    }

    #[tokio::test]
    async fn socket_permissions_0660() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let meta = std::fs::metadata(&sock).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "socket must be mode 0660, got {:o}", mode);
    }

    // ── Error handling ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn malformed_json_yields_error_then_server_continues() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // Send malformed JSON.
        w.write_all(b"not valid json\n").await.unwrap();
        let mut err_line = String::new();
        reader.read_line(&mut err_line).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&err_line).unwrap();
        assert_eq!(v["result"], "error", "malformed JSON must yield Ack::Error");

        // Same connection: send a valid request and get a valid response.
        w.write_all(b"{\"op\":\"status\"}\n").await.unwrap();
        let mut ok_line = String::new();
        reader.read_line(&mut ok_line).await.unwrap();
        let v2: serde_json::Value = serde_json::from_str(&ok_line).unwrap();
        assert!(
            v2.get("channels").is_some(),
            "valid request after malformed must succeed on same connection"
        );
    }

    #[tokio::test]
    async fn overlength_line_rejected_and_connection_dropped() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // Send a line longer than MAX_LINE_BYTES.
        let long_line: Vec<u8> = vec![b'x'; MAX_LINE_BYTES + 1];
        w.write_all(&long_line).await.unwrap();
        w.write_all(b"\n").await.unwrap();

        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(v["result"], "error", "over-long line must yield Ack::Error");

        // Connection must be dropped: further read returns EOF (0 bytes).
        let mut extra = String::new();
        let n = reader.read_line(&mut extra).await.unwrap();
        assert_eq!(n, 0, "connection must be dropped after over-long line");
    }

    #[tokio::test]
    async fn silent_client_does_not_block_other_clients() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        // Connect a client that sends nothing.
        let _silent = UnixStream::connect(&sock).await.unwrap();

        // A second client must still get a response immediately (each client
        // runs in its own task; the silent one is independently timed out).
        let resp = send_recv(&sock, r#"{"op":"status"}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(
            v.get("channels").is_some(),
            "second client must not be blocked by a silent client"
        );
    }

    #[tokio::test]
    async fn concurrent_clients_all_get_correct_responses() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let sock = sock.clone();
                tokio::spawn(async move { send_recv(&sock, r#"{"op":"status"}"#).await })
            })
            .collect();

        for task in tasks {
            let resp = task.await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
            assert!(
                v.get("channels").is_some(),
                "each concurrent client must get a correct status response"
            );
        }
    }

    // ── Override validation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn override_ttl_zero_rejected_with_core_error_text() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({
            "op": "override", "channel": "pwm7", "pwm": 200, "ttl_s": 0
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], "error", "ttl_s=0 must be rejected");
        // Error text must come from qfan_core::OverrideError, not invented here.
        let msg = v["message"].as_str().unwrap();
        assert!(
            msg.contains("out of range"),
            "error text must be from qfan_core: got {msg:?}"
        );
    }

    #[tokio::test]
    async fn override_ttl_exceeds_cap_rejected_with_core_error_text() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;
        let req = serde_json::json!({
            "op": "override", "channel": "pwm7", "pwm": 200, "ttl_s": 3601
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], "error", "ttl_s=3601 must be rejected");
        let msg = v["message"].as_str().unwrap();
        assert!(
            msg.contains("out of range"),
            "error text must be from qfan_core: got {msg:?}"
        );
    }

    // ── History gap sentinel ──────────────────────────────────────────────────

    /// F11: the override handlers accepted ANY channel string and returned
    /// Ack::Ok while the control loop silently dropped the command. No write
    /// reached the EC, but reporting success for a no-op on a thermally
    /// dangerous operation is the wrong failure mode.
    #[tokio::test]
    async fn f11_override_rejects_unknown_channel() {
        let dir = TempDir::new().unwrap();
        let (sock, _ctx) = start_server(&dir).await;

        for (op, body) in [
            (
                "override",
                r#"{"op":"override","channel":"pwm21","pwm":200,"ttl_s":60}"#,
            ),
            (
                "override",
                r#"{"op":"override","channel":"","pwm":200,"ttl_s":60}"#,
            ),
            (
                "clear_override",
                r#"{"op":"clear_override","channel":"pwm21"}"#,
            ),
            (
                "clear_override",
                r#"{"op":"clear_override","channel":"fan22"}"#,
            ),
        ] {
            let resp = send_recv(&sock, body).await;
            assert!(
                resp.contains("unknown channel"),
                "{op} must reject an unknown channel, got: {resp}"
            );
        }

        // The valid channels must still work, or the guard is too strict.
        for body in [
            r#"{"op":"override","channel":"pwm7","pwm":200,"ttl_s":60}"#,
            r#"{"op":"override","channel":"pwm1","pwm":200,"ttl_s":60}"#,
            r#"{"op":"clear_override","channel":"pwm7"}"#,
        ] {
            let resp = send_recv(&sock, body).await;
            assert!(
                !resp.contains("unknown channel"),
                "a valid channel must not be rejected, got: {resp}"
            );
        }
    }

    #[tokio::test]
    async fn history_gap_serialises_distinct_from_zero() {
        let dir = TempDir::new().unwrap();

        // Build a minute-tier file with buckets 0 and 2 written; bucket 1 is a gap.
        let tier_dir = dir.path().join("minute");
        std::fs::create_dir_all(&tier_dir).unwrap();
        let path = tier_dir.join("cpu.package.qfrb");
        let interval = Tier::Minute.interval_s() as u64;

        {
            let mut rb = FileRingBuffer::open_or_create(
                &path,
                Tier::Minute,
                &qfan_core::Series::CpuPackage,
                Unit::DeciCelsius,
                0,
            )
            .unwrap();
            // bucket 0
            rb.write_slot(Slot {
                bucket: 0,
                min: 300,
                avg: 320,
                max: 350,
                count: 5,
            })
            .unwrap();
            // bucket 2 (bucket 1 is intentionally unwritten → gap)
            rb.write_slot(Slot {
                bucket: 2,
                min: 400,
                avg: 410,
                max: 420,
                count: 5,
            })
            .unwrap();
        }

        let (sock, _ctx) = start_server(&dir).await;

        let req = serde_json::json!({
            "op": "history",
            "series": "cpu.package",
            "tier": "minute",
            "from": 0_u64,
            "to": 2 * interval
        })
        .to_string();
        let resp = send_recv(&sock, &req).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();

        let slots = v["slots"].as_array().expect("must have slots array");
        assert_eq!(slots.len(), 3, "three buckets: 0, 1, 2");

        // Bucket 1 is the gap.
        let gap = slots
            .iter()
            .find(|s| s["bucket"] == 1)
            .expect("gap bucket must be present");
        let gap_min = gap["min"].as_i64().expect("gap min must be an integer");

        assert_ne!(
            gap_min, 0,
            "gap min must not be 0 — a chart rendering gaps as 0°C is lying"
        );
        assert_eq!(
            gap_min,
            i16::MIN as i64,
            "gap min must be the NO_DATA sentinel (i16::MIN = {})",
            i16::MIN
        );
    }
}
