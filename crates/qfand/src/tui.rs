//! ratatui TUI for qfand — §7, M6.
//!
//! Client only: reads daemon state over the unix socket.
//! Never writes PWM, never calls decide, never touches sysfs.

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use qfan_core::CurvePoint;
use qfan_hist::NO_DATA;
use qfan_proto::{HistorySlot, Request, Response};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::ipc::SOCK_PATH;

// ── Constants ─────────────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RENDER_INTERVAL: Duration = Duration::from_millis(100);
const SOCK_TIMEOUT: Duration = Duration::from_secs(3);

/// (label, tier string, window duration in seconds)
const HIST_WINDOWS: &[(&str, &str, u64)] = &[
    ("1h", "raw", 3600),
    ("24h", "minute", 86_400),
    ("14d", "minute", 14 * 86_400),
    ("1y", "hour", 365 * 86_400),
];

const HIST_SERIES: &[&str] = &[
    "cpu.package",
    "cpu.core0",
    "cpu.core1",
    "cpu.core2",
    "cpu.core3",
    "cpu.core4",
    "cpu.core5",
    "pwm.1",
    "pwm.7",
    "fan.1",
    "fan.2",
    "fan.3",
    "fan.4",
    "fan.7",
    "fan.8",
];

// ── Terminal restore ───────────────────────────────────────────────────────────

/// Restore the terminal to its pre-TUI state.
/// Safe to call multiple times and from panic hooks.
pub fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

fn install_panic_hook() {
    let old = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        old(info);
    }));
}

/// RAII guard: restores the terminal on drop.
/// Covers normal exit, `?` early return, and panic unwind.
pub struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Dashboard = 0,
    History = 1,
    Curves = 2,
    Override = 3,
}

impl Tab {
    fn from_index(i: usize) -> Self {
        match i {
            1 => Tab::History,
            2 => Tab::Curves,
            3 => Tab::Override,
            _ => Tab::Dashboard,
        }
    }
    fn index(self) -> usize {
        self as usize
    }
    fn titles() -> &'static [&'static str] {
        &["[1] Dashboard", "[2] History", "[3] Curves", "[4] Override"]
    }
}

pub(crate) struct AppState {
    pub tab: Tab,
    pub daemon_online: bool,
    pub daemon_error: String,
    pub status: Option<qfan_proto::StatusResponse>,

    // History tab
    pub hist_win: usize,
    pub hist_ser: usize,
    pub hist_data: Vec<HistorySlot>,

    // Curves tab
    pub crv_channel: usize, // 0=pwm1, 1=pwm7
    pub crv_points: Vec<CurvePoint>,
    pub crv_sel: usize,
    pub crv_field: u8, // 0=temp_c, 1=pwm
    pub crv_editing: bool,
    pub crv_buf: String,
    pub crv_error: Option<String>,
    pub crv_ok: Option<String>,
    pub crv_submit: bool,
    /// The points on screen came from the daemon's running config. Until they
    /// have, editing and submit are disabled: the editor used to open on
    /// hardcoded pwm7 defaults, and submitting those on pwm1 replaced the real
    /// pwm1 curve with pwm7's.
    pub crv_loaded: bool,
    /// Fetch the running curve for `crv_channel` on the next loop iteration.
    pub crv_reload: bool,
    pub crv_load_error: Option<String>,

    // Override tab
    pub ov_channel: usize, // 0=pwm1, 1=pwm7
    pub ov_pwm_buf: String,
    pub ov_ttl_buf: String,
    pub ov_field: u8, // 0=pwm input, 1=ttl input
    pub ov_submit: bool,
    pub ov_clear: bool,
    pub ov_error: Option<String>,
    pub ov_ok: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            tab: Tab::Dashboard,
            daemon_online: false,
            daemon_error: "connecting…".into(),
            status: None,

            hist_win: 1, // default 24h (minute tier has real data)
            hist_ser: 0,
            hist_data: vec![],

            crv_channel: 1,
            crv_points: vec![],
            crv_sel: 0,
            crv_field: 0,
            crv_editing: false,
            crv_buf: String::new(),
            crv_error: None,
            crv_ok: None,
            crv_submit: false,
            crv_loaded: false,
            crv_reload: true,
            crv_load_error: None,

            ov_channel: 1,
            ov_pwm_buf: String::new(),
            ov_ttl_buf: "600".into(),
            ov_field: 0,
            ov_submit: false,
            ov_clear: false,
            ov_error: None,
            ov_ok: None,
        }
    }
}

impl AppState {
    /// Drop local edits and fetch `crv_channel`'s running curve.
    fn crv_request_reload(&mut self) {
        self.crv_points.clear();
        self.crv_sel = 0;
        self.crv_loaded = false;
        self.crv_reload = true;
        self.crv_error = None;
        self.crv_ok = None;
    }

    /// Install a curve fetched from the daemon.
    fn crv_apply_loaded(&mut self, points: Vec<CurvePoint>) {
        self.crv_sel = self.crv_sel.min(points.len().saturating_sub(1));
        self.crv_points = points;
        self.crv_loaded = !self.crv_points.is_empty();
        self.crv_load_error = None;
    }

    fn crv_channel_name(&self) -> &'static str {
        if self.crv_channel == 0 {
            "pwm1"
        } else {
            "pwm7"
        }
    }
    fn ov_channel_name(&self) -> &'static str {
        if self.ov_channel == 0 {
            "pwm1"
        } else {
            "pwm7"
        }
    }
}

// ── Gap logic ─────────────────────────────────────────────────────────────────

/// A slot is a gap when the daemon was not running (count==0) or no data exists (sentinel).
/// CRITICAL: gaps must NEVER be rendered as 0°C.
pub fn is_gap(slot: &HistorySlot) -> bool {
    slot.count == 0 || slot.avg == NO_DATA || slot.min == NO_DATA || slot.max == NO_DATA
}

/// Split slots into continuous segments of (bucket_x, avg_celsius) pairs, skipping gaps.
/// Each Vec is one unbroken run. Gaps produce separate vecs (visual breaks in the chart).
pub fn history_to_segments(slots: &[HistorySlot]) -> Vec<Vec<(f64, f64)>> {
    let mut result: Vec<Vec<(f64, f64)>> = vec![];
    let mut current: Vec<(f64, f64)> = vec![];
    for slot in slots {
        if is_gap(slot) {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push((slot.bucket as f64, slot.avg as f64 / 10.0));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// Same but returns max values (safety-relevant).
pub fn history_to_segments_max(slots: &[HistorySlot]) -> Vec<Vec<(f64, f64)>> {
    let mut result: Vec<Vec<(f64, f64)>> = vec![];
    let mut current: Vec<(f64, f64)> = vec![];
    for slot in slots {
        if is_gap(slot) {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push((slot.bucket as f64, slot.max as f64 / 10.0));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

// ── Socket client ─────────────────────────────────────────────────────────────

async fn daemon_request(req: &Request) -> Result<Response, String> {
    let stream = tokio::time::timeout(SOCK_TIMEOUT, UnixStream::connect(SOCK_PATH))
        .await
        .map_err(|_| "daemon not responding (connect timeout)".to_string())?
        .map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                "daemon not running".to_string()
            } else {
                format!("connect: {e}")
            }
        })?;

    let (r, mut w) = stream.into_split();
    let json = serde_json::to_string(req).map_err(|e| e.to_string())?;

    tokio::time::timeout(SOCK_TIMEOUT, async {
        w.write_all(json.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        w.write_all(b"\n")
            .await
            .map_err(|e| format!("write newline: {e}"))?;
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if line.is_empty() {
            return Err("daemon returned empty response".to_string());
        }
        serde_json::from_str::<Response>(&line).map_err(|e| format!("parse: {e}"))
    })
    .await
    .map_err(|_| "daemon not responding (read timeout)".to_string())?
}

// ── Style helpers ─────────────────────────────────────────────────────────────

fn mode_style(mode: &str) -> Style {
    match mode {
        "emergency" => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD | Modifier::RAPID_BLINK),
        "failsafe" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        "override" => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Green),
    }
}

fn temp_color(t: f32) -> Style {
    if t >= 90.0 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if t >= 75.0 {
        Style::default().fg(Color::Yellow)
    } else if t >= 60.0 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else {
        format!("{m}m {s}s")
    }
}

fn fmt_bucket_time(bucket: u64, window_s: u64) -> String {
    let interval_s: u64 = match window_s {
        s if s <= 3600 => 2,
        s if s <= 14 * 86_400 => 60,
        _ => 3600,
    };
    let ts = bucket * interval_s;
    if window_s <= 86_400 {
        let h = (ts % 86_400) / 3600;
        let m = (ts % 3600) / 60;
        format!("{h:02}:{m:02}")
    } else {
        let days = ts / 86_400;
        let m = (days % 365) / 30 + 1;
        let d = days % 30 + 1;
        format!("{m:02}/{d:02}")
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub(crate) fn draw(f: &mut Frame, app: &AppState) {
    let size = f.area();

    if size.width < 20 || size.height < 5 {
        f.render_widget(
            Paragraph::new("Terminal too small — resize").style(Style::default().fg(Color::Yellow)),
            size,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(size);

    draw_tabs(f, chunks[0], app);
    match app.tab {
        Tab::Dashboard => draw_dashboard(f, chunks[1], app),
        Tab::History => draw_history(f, chunks[1], app),
        Tab::Curves => draw_curves(f, chunks[1], app),
        Tab::Override => draw_override(f, chunks[1], app),
    }
    draw_help(f, chunks[2], app);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &AppState) {
    let titles: Vec<Line> = Tab::titles().iter().map(|t| Line::from(*t)).collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .divider("|");
    f.render_widget(tabs, area);
}

fn draw_help(f: &mut Frame, area: Rect, app: &AppState) {
    let txt = match app.tab {
        Tab::Dashboard => "[1-4/Tab] switch  [q/Esc] quit",
        Tab::History => "[h/l] window  [j/k] series  [q] quit",
        Tab::Curves => {
            "[j/k] row  [Tab/h/l] column  [Enter] edit  [a] add  [d] del  [s] apply  [r] reload  [c] chan  [1-4] tabs"
        }
        Tab::Override => "[Tab] field  [s] set  [x] clear  [c] channel  [1-4] tabs  [q] quit",
    };
    f.render_widget(
        Paragraph::new(txt).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

fn draw_dashboard(f: &mut Frame, area: Rect, app: &AppState) {
    if !app.daemon_online {
        f.render_widget(
            Paragraph::new(format!("Daemon offline: {}", app.daemon_error))
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::ALL).title("Dashboard")),
            area,
        );
        return;
    }

    let status = match &app.status {
        Some(s) => s,
        None => {
            f.render_widget(
                Paragraph::new("Waiting for daemon response…")
                    .block(Block::default().borders(Borders::ALL).title("Dashboard")),
                area,
            );
            return;
        }
    };

    // Show emergency/failsafe banner at the top; it must be impossible to miss.
    let is_critical = status.mode == "emergency" || status.mode == "failsafe";
    let chunks = if is_critical {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1)])
            .split(area)
    };

    let content_area = if is_critical {
        let banner_text = format!(
            "⚠  {} ⚠  — fans forced to 255 (100%)",
            status.mode.to_uppercase()
        );
        f.render_widget(
            Paragraph::new(banner_text)
                .style(mode_style(&status.mode))
                .block(Block::default().borders(Borders::ALL)),
            chunks[0],
        );
        chunks[1]
    } else {
        chunks[0]
    };

    let mut lines: Vec<Line> = vec![];

    lines.push(Line::from(vec![
        Span::raw(format!("uptime: {}   mode: ", fmt_uptime(status.uptime_s))),
        Span::styled(status.mode.to_uppercase(), mode_style(&status.mode)),
    ]));
    lines.push(Line::from(""));

    if let Some(pkg) = status.temps.cpu_package_c {
        lines.push(Line::from(vec![
            Span::raw("CPU package: "),
            Span::styled(format!("{pkg:.1}°C"), temp_color(pkg)),
        ]));
    }

    if !status.temps.cpu_cores_c.is_empty() {
        let core_str: String = status
            .temps
            .cpu_cores_c
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.map(|c| format!("c{i}:{c:.0}"))
                    .unwrap_or_else(|| format!("c{i}:?"))
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(format!("Cores: {core_str}")));
    }

    let disk_max = status
        .temps
        .disk_temps_c
        .iter()
        .filter_map(|(_, t)| *t)
        .fold(f32::NEG_INFINITY, f32::max);
    if disk_max.is_finite() {
        lines.push(Line::from(vec![
            Span::raw("Disk max: "),
            Span::styled(format!("{disk_max:.0}°C"), temp_color(disk_max)),
        ]));
    }

    lines.push(Line::from(""));

    // Channel rows — only pwm1 and pwm7 (never pwm21 or fan22-35).
    for ch in status
        .channels
        .iter()
        .filter(|c| c.channel == "pwm1" || c.channel == "pwm7")
    {
        let pct = ch.intended_pwm as f32 / 255.0 * 100.0;
        let mut spans = vec![
            Span::raw(format!("{:<5} ", ch.channel)),
            Span::raw(format!("pwm={} ({:.0}%)", ch.intended_pwm, pct)),
        ];
        if let Some(rb) = ch.readback_pwm {
            spans.push(Span::raw(format!("  rb={rb}")));
        }
        if let Some(rpm) = ch.rpm {
            spans.push(Span::raw(format!("  rpm={rpm}")));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(ch.mode.to_uppercase(), mode_style(&ch.mode)));
        if ch.mode == "override" {
            if let Some(exp) = ch.override_expires_at {
                let left = exp.saturating_sub(unix_now());
                spans.push(Span::styled(
                    format!(" (expires in {left}s)"),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Dashboard"))
            .wrap(Wrap { trim: false }),
        content_area,
    );
}

// ── History ───────────────────────────────────────────────────────────────────

fn draw_history(f: &mut Frame, area: Rect, app: &AppState) {
    let (win_label, _, _) = HIST_WINDOWS[app.hist_win];
    let series = HIST_SERIES[app.hist_ser];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    f.render_widget(
        Paragraph::new(format!(
            "Series: {series}  Window: {win_label}  (gaps = daemon offline)"
        ))
        .style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );

    if !app.daemon_online {
        f.render_widget(
            Paragraph::new(format!("Daemon offline: {}", app.daemon_error))
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::ALL).title("History")),
            chunks[1],
        );
        return;
    }

    if app.hist_data.is_empty() {
        f.render_widget(
            Paragraph::new("No history data for this series/window.")
                .block(Block::default().borders(Borders::ALL).title("History")),
            chunks[1],
        );
        return;
    }

    let avg_segs = history_to_segments(&app.hist_data);
    let max_segs = history_to_segments_max(&app.hist_data);

    let non_gap: Vec<&HistorySlot> = app.hist_data.iter().filter(|s| !is_gap(s)).collect();
    if non_gap.is_empty() {
        f.render_widget(
            Paragraph::new("All buckets are gaps — daemon was not running during this window.")
                .block(Block::default().borders(Borders::ALL).title("History")),
            chunks[1],
        );
        return;
    }

    let x_min = non_gap
        .iter()
        .map(|s| s.bucket as f64)
        .fold(f64::INFINITY, f64::min);
    let x_max = non_gap
        .iter()
        .map(|s| s.bucket as f64)
        .fold(f64::NEG_INFINITY, f64::max);
    let y_min = (non_gap
        .iter()
        .map(|s| s.min as f64 / 10.0)
        .fold(f64::INFINITY, f64::min)
        - 5.0)
        .max(0.0);
    let y_max = non_gap
        .iter()
        .map(|s| s.max as f64 / 10.0)
        .fold(f64::NEG_INFINITY, f64::max)
        + 5.0;

    let (_, _, dur_s) = HIST_WINDOWS[app.hist_win];
    let x_mid = (x_min + x_max) / 2.0;
    let x_labels = vec![
        Span::raw(fmt_bucket_time(x_min as u64, dur_s)),
        Span::raw(fmt_bucket_time(x_mid as u64, dur_s)),
        Span::raw(fmt_bucket_time(x_max as u64, dur_s)),
    ];
    let y_mid = (y_min + y_max) / 2.0;
    let y_labels = vec![
        Span::raw(format!("{y_min:.0}")),
        Span::raw(format!("{y_mid:.0}")),
        Span::raw(format!("{y_max:.0}")),
    ];

    // Build datasets.  avg = cyan, max = red.  One Dataset per continuous segment.
    let mut datasets: Vec<Dataset> = vec![];
    for (i, seg) in avg_segs.iter().enumerate() {
        datasets.push(
            Dataset::default()
                .name(if i == 0 { "avg°C" } else { "" })
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(seg),
        );
    }
    for (i, seg) in max_segs.iter().enumerate() {
        datasets.push(
            Dataset::default()
                .name(if i == 0 { "max°C" } else { "" })
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Red))
                .data(seg),
        );
    }

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("History — {series} / {win_label}")),
        )
        .x_axis(
            Axis::default()
                .title("time")
                .bounds([x_min, x_max.max(x_min + 1.0)])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("°C")
                .bounds([y_min, y_max.max(y_min + 1.0)])
                .labels(y_labels),
        );

    f.render_widget(chart, chunks[1]);
}

// ── Curves ────────────────────────────────────────────────────────────────────

fn draw_curves(f: &mut Frame, area: Rect, app: &AppState) {
    let channel = app.crv_channel_name();
    let title = format!("Curve Editor — {channel}");

    let mut lines: Vec<Line> = vec![];
    if !app.crv_loaded {
        let msg = match &app.crv_load_error {
            Some(e) => format!("Cannot load the {channel} curve from the daemon: {e}"),
            None => format!("Loading the {channel} curve from the daemon…"),
        };
        lines.push(Line::from(Span::styled(
            msg,
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Temp (°C)",
            Style::default().add_modifier(Modifier::UNDERLINED),
        ),
        Span::raw("    "),
        Span::styled(
            "PWM (0-255)",
            Style::default().add_modifier(Modifier::UNDERLINED),
        ),
        Span::raw("    "),
        Span::styled("Duty%", Style::default().add_modifier(Modifier::UNDERLINED)),
    ]));

    for (i, pt) in app.crv_points.iter().enumerate() {
        let sel = i == app.crv_sel;
        let arrow = if sel { "►" } else { " " };
        let pct = pt.pwm as f32 / 255.0 * 100.0;

        let temp_str = if sel && app.crv_editing && app.crv_field == 0 {
            format!("[{}]", app.crv_buf)
        } else {
            format!("{:>7}", pt.temp_c)
        };
        let pwm_str = if sel && app.crv_editing && app.crv_field == 1 {
            format!("[{}]", app.crv_buf)
        } else {
            format!("{:>10}", pt.pwm)
        };

        // The selected row is bold; within it, the column Enter will edit is
        // reversed. Nothing marked the active column before, so the only way
        // to learn it was to press Enter and read the prompt.
        let row = if sel {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let cell = |field: u8| {
            if sel && app.crv_field == field {
                row.add_modifier(Modifier::REVERSED)
            } else {
                row
            }
        };
        lines.push(Line::from(vec![
            Span::styled(arrow.to_string(), row),
            Span::styled(temp_str, cell(0)),
            Span::raw("    "),
            Span::styled(pwm_str, cell(1)),
            Span::raw(format!("    {pct:>5.0}%")),
        ]));
    }

    if let Some(err) = &app.crv_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Not applied: {err}"),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(Span::styled(
            "Your edits are still on screen; the fan keeps its previous curve. Fix and [s], or [r] to reload.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    if let Some(ok) = &app.crv_ok {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            ok.clone(),
            Style::default().fg(Color::Green),
        )));
    }

    if app.crv_editing {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "Editing {} — digits + Enter to confirm, Esc to cancel",
                if app.crv_field == 0 { "temp_c" } else { "pwm" }
            ),
            Style::default().fg(Color::Yellow),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ── Override ──────────────────────────────────────────────────────────────────

fn draw_override(f: &mut Frame, area: Rect, app: &AppState) {
    let channel = app.ov_channel_name();

    let active_override = app.status.as_ref().and_then(|s| {
        s.channels
            .iter()
            .find(|c| c.channel == channel && c.mode == "override")
            .and_then(|c| c.override_expires_at.map(|exp| (c.intended_pwm, exp)))
    });

    let mut lines: Vec<Line> = vec![];

    match active_override {
        Some((pwm, exp)) => {
            let left = exp.saturating_sub(unix_now());
            lines.push(Line::from(Span::styled(
                format!(
                    "⚠  OVERRIDE ACTIVE on {channel}: PWM {pwm} ({:.0}%)  expires in {left}s",
                    pwm as f32 / 255.0 * 100.0
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        None => {
            lines.push(Line::from(format!(
                "{channel}: no active override (curve control running)"
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from("Set Override:"));

    let pwm_hl = app.ov_field == 0;
    let ttl_hl = app.ov_field == 1;

    lines.push(Line::from(vec![
        Span::raw("  PWM (0-255):       "),
        Span::styled(
            format!("[{}]", app.ov_pwm_buf),
            if pwm_hl {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  TTL seconds (1-3600): "),
        Span::styled(
            format!("[{}]", app.ov_ttl_buf),
            if ttl_hl {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
    ]));

    if let Some(err) = &app.ov_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    if let Some(ok) = &app.ov_ok {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            ok.clone(),
            Style::default().fg(Color::Green),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Override — {channel}")),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ── Key handling ──────────────────────────────────────────────────────────────

/// Returns true if the TUI should exit.
fn handle_key(app: &mut AppState, key: crossterm::event::KeyEvent) -> bool {
    // Curve editor intercepts input when editing a field value.
    if app.tab == Tab::Curves && app.crv_editing {
        return handle_key_curve_editing(app, key);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('1') => app.tab = Tab::Dashboard,
        KeyCode::Char('2') => {
            app.tab = Tab::History;
        }
        KeyCode::Char('3') => {
            app.tab = Tab::Curves;
        }
        KeyCode::Char('4') => {
            app.tab = Tab::Override;
        }
        // Curves and Override use Tab to move between their input fields, so
        // it must reach them. Catching it here first made those fields
        // unreachable (the PWM column could never be edited). Number keys
        // still switch tabs from anywhere.
        KeyCode::Tab if !matches!(app.tab, Tab::Curves | Tab::Override) => {
            app.tab = Tab::from_index((app.tab.index() + 1) % 4);
        }
        _ => {
            return match app.tab {
                Tab::Dashboard => false,
                Tab::History => {
                    handle_key_history(app, key);
                    false
                }
                Tab::Curves => {
                    handle_key_curves(app, key);
                    false
                }
                Tab::Override => handle_key_override(app, key),
            };
        }
    }
    false
}

fn handle_key_history(app: &mut AppState, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('h') | KeyCode::Left => {
            if app.hist_win > 0 {
                app.hist_win -= 1;
                app.hist_data.clear();
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if app.hist_win + 1 < HIST_WINDOWS.len() {
                app.hist_win += 1;
                app.hist_data.clear();
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if app.hist_ser + 1 < HIST_SERIES.len() {
                app.hist_ser += 1;
                app.hist_data.clear();
            }
        }
        KeyCode::Char('k') | KeyCode::Up if app.hist_ser > 0 => {
            app.hist_ser -= 1;
            app.hist_data.clear();
        }
        _ => {}
    }
}

fn handle_key_curves(app: &mut AppState, key: crossterm::event::KeyEvent) {
    match key.code {
        KeyCode::Char('c') => {
            app.crv_channel = 1 - app.crv_channel;
            app.crv_request_reload();
            return;
        }
        KeyCode::Char('r') => {
            app.crv_request_reload();
            return;
        }
        _ => {}
    }
    // Nothing below may run on points that did not come from the daemon.
    if !app.crv_loaded || app.crv_points.is_empty() {
        return;
    }
    if matches!(key.code, KeyCode::Char('a' | 'd') | KeyCode::Enter) {
        app.crv_ok = None;
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if app.crv_sel + 1 < app.crv_points.len() {
                app.crv_sel += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.crv_sel > 0 {
                app.crv_sel -= 1;
            }
        }
        KeyCode::Enter => {
            let pt = &app.crv_points[app.crv_sel];
            app.crv_buf = if app.crv_field == 0 {
                pt.temp_c.to_string()
            } else {
                pt.pwm.to_string()
            };
            app.crv_editing = true;
        }
        KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
            app.crv_field = 1 - app.crv_field;
        }
        KeyCode::Char('a') => {
            let insert_at = (app.crv_sel + 1).min(app.crv_points.len());
            app.crv_points.insert(
                insert_at,
                CurvePoint {
                    temp_c: 60,
                    pwm: 180,
                },
            );
            app.crv_sel = insert_at;
        }
        KeyCode::Char('d') => {
            if app.crv_points.len() > 1 {
                app.crv_points.remove(app.crv_sel);
                if app.crv_sel >= app.crv_points.len() {
                    app.crv_sel = app.crv_points.len() - 1;
                }
            }
        }
        KeyCode::Char('s') => {
            app.crv_submit = true;
        }
        _ => {}
    }
}

fn handle_key_curve_editing(app: &mut AppState, key: crossterm::event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            app.crv_editing = false;
            app.crv_buf.clear();
        }
        KeyCode::Enter => {
            if app.crv_field == 0 {
                if let Ok(v) = app.crv_buf.parse::<i16>() {
                    app.crv_points[app.crv_sel].temp_c = v;
                }
            } else if let Ok(v) = app.crv_buf.parse::<u8>() {
                app.crv_points[app.crv_sel].pwm = v;
            }
            app.crv_editing = false;
            app.crv_buf.clear();
        }
        KeyCode::Backspace => {
            app.crv_buf.pop();
        }
        KeyCode::Char(c)
            if (c.is_ascii_digit() || (c == '-' && app.crv_buf.is_empty()))
                && app.crv_buf.len() < 5 =>
        {
            app.crv_buf.push(c);
        }
        _ => {}
    }
    false
}

fn handle_key_override(app: &mut AppState, key: crossterm::event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab => {
            app.ov_field = (app.ov_field + 1) % 2;
        }
        KeyCode::Backspace => {
            if app.ov_field == 0 {
                app.ov_pwm_buf.pop();
            } else {
                app.ov_ttl_buf.pop();
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if app.ov_field == 0 && app.ov_pwm_buf.len() < 3 {
                app.ov_pwm_buf.push(c);
            } else if app.ov_field == 1 && app.ov_ttl_buf.len() < 4 {
                app.ov_ttl_buf.push(c);
            }
        }
        KeyCode::Char('s') | KeyCode::Enter => {
            app.ov_submit = true;
        }
        KeyCode::Char('x') => {
            app.ov_clear = true;
        }
        KeyCode::Char('c') => {
            app.ov_channel = 1 - app.ov_channel;
            app.ov_error = None;
            app.ov_ok = None;
        }
        _ => {}
    }
    false
}

// ── Main loop ─────────────────────────────────────────────────────────────────

pub async fn run_tui(_config: &Path) -> Result<()> {
    install_panic_hook();

    terminal::enable_raw_mode().context("enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).context("enter alt screen")?;
    let _guard = TermGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).context("create terminal")?;

    let mut app = AppState::default();
    let mut last_poll = std::time::Instant::now();
    let mut first_tick = true;

    loop {
        // Render.
        term.draw(|f| draw(f, &app)).context("draw")?;

        tokio::time::sleep(RENDER_INTERVAL).await;

        // Drain key events (non-blocking).
        while event::poll(Duration::ZERO).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if handle_key(&mut app, key) {
                        return Ok(());
                    }
                }
                Ok(Event::Resize(_, _)) => {} // next draw() picks up new size
                _ => {}
            }
        }

        // Async actions triggered by key handlers.
        if app.crv_submit {
            app.crv_submit = false;
            let req = Request::SetCurve {
                channel: app.crv_channel_name().to_string(),
                points: app.crv_points.clone(),
            };
            match daemon_request(&req).await {
                Ok(Response::Ack(qfan_proto::Ack::Ok)) => {
                    app.crv_error = None;
                    app.crv_ok = Some(format!(
                        "Applied and saved: {} now runs this curve.",
                        app.crv_channel_name()
                    ));
                }
                Ok(Response::Ack(qfan_proto::Ack::Error { message })) => {
                    app.crv_error = Some(message);
                    app.crv_ok = None;
                }
                Err(e) => {
                    app.crv_error = Some(e);
                    app.crv_ok = None;
                }
                _ => {}
            }
        }

        if app.crv_reload {
            // One attempt per request; the 1 Hz poll below re-arms it while
            // the daemon is unreachable, so an offline daemon is not hammered
            // at the render rate.
            app.crv_reload = false;
            match daemon_request(&Request::Config).await {
                Ok(Response::Config(v)) => match curve_from_config(&v, app.crv_channel_name()) {
                    Ok(points) => app.crv_apply_loaded(points),
                    Err(e) => app.crv_load_error = Some(e),
                },
                Ok(Response::Ack(qfan_proto::Ack::Error { message })) => {
                    app.crv_load_error = Some(message)
                }
                Ok(_) => app.crv_load_error = Some("unexpected response type".into()),
                Err(e) => app.crv_load_error = Some(e),
            }
        }

        if app.ov_submit {
            app.ov_submit = false;
            let pwm: Option<u8> = app.ov_pwm_buf.parse().ok();
            let ttl: Option<u32> = app.ov_ttl_buf.parse().ok();
            match (pwm, ttl) {
                (Some(pwm), Some(ttl_s)) => {
                    let req = Request::Override {
                        channel: app.ov_channel_name().to_string(),
                        pwm,
                        ttl_s,
                    };
                    match daemon_request(&req).await {
                        Ok(Response::Ack(qfan_proto::Ack::Ok)) => {
                            app.ov_error = None;
                            app.ov_ok = Some("Override set.".into());
                        }
                        Ok(Response::Ack(qfan_proto::Ack::Error { message })) => {
                            app.ov_error = Some(message);
                            app.ov_ok = None;
                        }
                        Err(e) => {
                            app.ov_error = Some(e);
                            app.ov_ok = None;
                        }
                        _ => {}
                    }
                }
                _ => {
                    app.ov_error = Some("Enter valid numbers for PWM and TTL.".into());
                }
            }
        }

        if app.ov_clear {
            app.ov_clear = false;
            let req = Request::ClearOverride {
                channel: app.ov_channel_name().to_string(),
            };
            match daemon_request(&req).await {
                Ok(Response::Ack(qfan_proto::Ack::Ok)) => {
                    app.ov_error = None;
                    app.ov_ok = Some("Override cleared.".into());
                }
                Ok(Response::Ack(qfan_proto::Ack::Error { message })) => {
                    app.ov_error = Some(message);
                    app.ov_ok = None;
                }
                Err(e) => {
                    app.ov_error = Some(e);
                    app.ov_ok = None;
                }
                _ => {}
            }
        }

        // Daemon poll — 1 Hz max (§2.2: EC access is expensive).
        if first_tick || last_poll.elapsed() >= POLL_INTERVAL {
            first_tick = false;
            last_poll = std::time::Instant::now();
            if !app.crv_loaded {
                app.crv_reload = true;
            }

            match daemon_request(&Request::Status).await {
                Ok(Response::Status(s)) => {
                    app.status = Some(s);
                    app.daemon_online = true;
                    app.daemon_error.clear();
                }
                Err(e) => {
                    app.daemon_online = false;
                    app.daemon_error = e;
                }
                _ => {
                    app.daemon_online = false;
                    app.daemon_error = "unexpected response type".into();
                }
            }

            // Refresh history when on that tab.
            if app.tab == Tab::History {
                let (_, tier, dur_s) = HIST_WINDOWS[app.hist_win];
                let now = unix_now();
                let req = Request::History {
                    series: HIST_SERIES[app.hist_ser].to_string(),
                    tier: tier.to_string(),
                    from: now.saturating_sub(dur_s),
                    to: now,
                };
                if let Ok(Response::History(h)) = daemon_request(&req).await {
                    app.hist_data = h.slots;
                }
            }
        }
    }
}

/// Pull one channel's breakpoints out of a `{"op":"config"}` reply.
fn curve_from_config(v: &serde_json::Value, channel: &str) -> Result<Vec<CurvePoint>, String> {
    let bp = v
        .get(channel)
        .and_then(|c| c.get("breakpoints"))
        .ok_or_else(|| format!("daemon config has no {channel}.breakpoints"))?;
    let points: Vec<CurvePoint> = serde_json::from_value(bp.clone())
        .map_err(|e| format!("bad {channel}.breakpoints: {e}"))?;
    if points.is_empty() {
        return Err(format!("daemon reports an empty {channel} curve"));
    }
    Ok(points)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use qfan_core::Config;
    use qfan_proto::{ChannelStatus, HistorySlot, StatusResponse, TempSnapshot};
    use ratatui::{backend::TestBackend, Terminal};

    fn valid_slot(bucket: u32, avg: i16) -> HistorySlot {
        HistorySlot {
            bucket,
            min: avg - 5,
            avg,
            max: avg + 5,
            count: 5,
        }
    }

    fn no_data_slot(bucket: u32) -> HistorySlot {
        HistorySlot {
            bucket,
            min: NO_DATA,
            avg: NO_DATA,
            max: NO_DATA,
            count: 0,
        }
    }

    /// Sentinel WITHOUT count==0. The daemon currently always writes both
    /// together, so no other fixture isolates the sentinel branch of `is_gap` —
    /// deleting that branch left the whole suite green. This fixture pins it,
    /// because `is_gap` promises to catch either signal independently and a
    /// future daemon change could legitimately send a nonzero count alongside
    /// the sentinel.
    fn sentinel_only_slot(bucket: u32) -> HistorySlot {
        HistorySlot {
            bucket,
            min: NO_DATA,
            avg: NO_DATA,
            max: NO_DATA,
            count: 7,
        }
    }

    fn count_zero_slot(bucket: u32) -> HistorySlot {
        // count=0 with plausible-looking values — still a gap per spec
        HistorySlot {
            bucket,
            min: 300,
            avg: 320,
            max: 350,
            count: 0,
        }
    }

    fn render_app_to_buffer(app: &AppState, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    /// The gap tests above all exercise `history_to_segments`, a pure function.
    /// That proves the segmentation logic but NOT that a gap stays off the
    /// screen — a renderer could plot the sentinel independently of the helper.
    /// This asserts on the actual rendered buffer AND on the exact points handed
    /// to the chart: real temperatures either side of a three-bucket outage.
    #[test]
    fn gaps_never_reach_the_rendered_buffer() {
        let slots = vec![
            valid_slot(0, 320),
            valid_slot(1, 340),
            no_data_slot(2),
            no_data_slot(3),
            count_zero_slot(4),
            valid_slot(5, 380),
            valid_slot(6, 400),
        ];

        let app = AppState {
            tab: Tab::History,
            daemon_online: true,
            hist_data: slots.clone(),
            ..AppState::default()
        };

        let rendered = render_app_to_buffer(&app, 120, 40);
        for forbidden in ["-32768", "-3276.8", "32768"] {
            assert!(
                !rendered.contains(forbidden),
                "NO_DATA sentinel {forbidden} leaked into the rendered buffer"
            );
        }

        // Axis labels are legitimate text, so assert on the plotted DATA too.
        let segs = history_to_segments(&slots);
        assert_eq!(segs.len(), 2, "one outage run must split the series in two");
        let plotted: Vec<(f64, f64)> = segs.iter().flatten().copied().collect();
        for &(x, v) in &plotted {
            assert!(
                !(2.0..=4.0).contains(&x),
                "gap bucket {x} was plotted; downtime must break the line"
            );
            assert!(
                v > 0.0,
                "plotted value {v} is not a real temperature; a gap became zero"
            );
        }
        assert_eq!(
            plotted.len(),
            4,
            "only the four real samples may be plotted"
        );
    }

    /// Each gap signal must work ALONE, not only in combination.
    #[test]
    fn sentinel_alone_is_a_gap_without_count_zero() {
        let slot = sentinel_only_slot(3);
        assert!(
            is_gap(&slot),
            "NO_DATA sentinel must be a gap even when count is nonzero"
        );

        let slots = vec![
            valid_slot(0, 320),
            sentinel_only_slot(1),
            valid_slot(2, 400),
        ];
        let segs = history_to_segments(&slots);
        assert_eq!(segs.len(), 2, "sentinel alone must break the line");
        for &(_, v) in segs.iter().flatten() {
            assert!(v > 0.0, "sentinel {v} was plotted as a temperature");
        }
    }

    // ── Gap logic ─────────────────────────────────────────────────────────────

    #[test]
    fn no_data_sentinel_is_gap() {
        assert!(is_gap(&no_data_slot(0)));
    }

    #[test]
    fn count_zero_is_gap_even_with_plausible_values() {
        assert!(is_gap(&count_zero_slot(0)));
    }

    #[test]
    fn valid_slot_not_gap() {
        assert!(!is_gap(&valid_slot(0, 320)));
    }

    #[test]
    fn no_data_gaps_not_included_in_segments() {
        let slots = vec![valid_slot(0, 320), no_data_slot(1), valid_slot(2, 400)];
        let segs = history_to_segments(&slots);
        // Gap splits into two segments.
        assert_eq!(segs.len(), 2, "NO_DATA gap must split into two segments");
        // Gap bucket (x=1.0) must not appear in any segment.
        for seg in &segs {
            for &(x, _) in seg {
                assert_ne!(x, 1.0, "gap bucket must not appear in segments");
            }
        }
        // NO_DATA value must not appear.
        for seg in &segs {
            for &(_, v) in seg {
                assert_ne!(
                    v,
                    NO_DATA as f64 / 10.0,
                    "NO_DATA sentinel value must not appear"
                );
            }
        }
    }

    #[test]
    fn count_zero_gaps_split_segments() {
        let slots = vec![valid_slot(0, 320), count_zero_slot(1), valid_slot(2, 400)];
        let segs = history_to_segments(&slots);
        assert_eq!(segs.len(), 2, "count=0 gap must split just like NO_DATA");
        for seg in &segs {
            for &(x, _) in seg {
                assert_ne!(x, 1.0, "count=0 bucket must not appear in segments");
            }
        }
    }

    #[test]
    fn all_gaps_produces_no_segments() {
        let slots = vec![no_data_slot(0), count_zero_slot(1), no_data_slot(2)];
        assert!(history_to_segments(&slots).is_empty());
    }

    #[test]
    fn contiguous_valid_data_is_one_segment() {
        let slots: Vec<_> = (0u32..10)
            .map(|i| valid_slot(i, 300 + i as i16 * 5))
            .collect();
        let segs = history_to_segments(&slots);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].len(), 10);
    }

    // ── Rendering — no panics ─────────────────────────────────────────────────

    #[test]
    fn all_screens_render_at_80x24() {
        for tab in [Tab::Dashboard, Tab::History, Tab::Curves, Tab::Override] {
            let app = AppState {
                tab,
                ..AppState::default()
            };
            render_app_to_buffer(&app, 80, 24);
        }
    }

    #[test]
    fn all_screens_render_at_20x5_no_panic() {
        for tab in [Tab::Dashboard, Tab::History, Tab::Curves, Tab::Override] {
            let app = AppState {
                tab,
                ..AppState::default()
            };
            render_app_to_buffer(&app, 20, 5);
        }
    }

    // ── Curve editor ──────────────────────────────────────────────────────────

    fn press(app: &mut AppState, code: KeyCode) -> bool {
        handle_key(
            app,
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        )
    }

    fn curves_tab() -> AppState {
        AppState {
            tab: Tab::Curves,
            daemon_online: true,
            ..AppState::default()
        }
    }

    fn pwm1_running() -> Vec<CurvePoint> {
        vec![
            CurvePoint {
                temp_c: 35,
                pwm: 85,
            },
            CurvePoint {
                temp_c: 60,
                pwm: 180,
            },
        ]
    }

    /// The editor used to open on hardcoded pwm7 values; pressing `s` on the
    /// pwm1 tab then replaced the real pwm1 curve with pwm7's defaults. Until
    /// the daemon's curve arrives there must be nothing to submit or edit.
    #[test]
    fn nothing_is_editable_or_submittable_before_the_curve_loads() {
        let mut app = curves_tab();
        assert!(app.crv_points.is_empty() && !app.crv_loaded);
        assert!(
            app.crv_reload,
            "a fresh editor asks the daemon for its curve"
        );

        for k in [
            KeyCode::Char('s'),
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char('d'),
        ] {
            press(&mut app, k);
        }

        assert!(
            !app.crv_submit,
            "no submit of points that never came from the daemon"
        );
        assert!(!app.crv_editing);
        assert!(app.crv_points.is_empty());
        assert!(render_app_to_buffer(&app, 100, 24).contains("Loading the pwm7 curve"));
    }

    #[test]
    fn switching_channel_drops_edits_and_reloads() {
        let mut app = curves_tab();
        app.crv_apply_loaded(Config::default().pwm7.breakpoints);
        app.crv_reload = false;

        press(&mut app, KeyCode::Char('c'));

        assert_eq!(app.crv_channel_name(), "pwm1");
        assert!(
            app.crv_points.is_empty(),
            "pwm7's points must not linger on pwm1"
        );
        assert!(app.crv_reload && !app.crv_loaded);

        app.crv_apply_loaded(pwm1_running());
        let screen = render_app_to_buffer(&app, 100, 24);
        assert!(
            screen.contains("pwm1") && screen.contains("180"),
            "{screen}"
        );
    }

    /// Tab was swallowed by the global tab switcher, so the PWM column could
    /// never be selected and only temperatures were editable.
    #[test]
    fn tab_moves_between_columns_and_stays_on_the_curves_tab() {
        let mut app = curves_tab();
        app.crv_apply_loaded(pwm1_running());
        assert_eq!(app.crv_field, 0);

        press(&mut app, KeyCode::Tab);

        assert!(app.tab == Tab::Curves, "Tab must not leave the editor");
        assert_eq!(app.crv_field, 1, "Tab selects the PWM column");

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        for c in "120".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.crv_points[0].pwm, 120);
        assert_eq!(app.crv_points[0].temp_c, 35, "temperature untouched");
    }

    #[test]
    fn tab_still_cycles_tabs_outside_the_editors() {
        let mut app = AppState::default();
        press(&mut app, KeyCode::Tab);
        assert!(app.tab == Tab::History);
    }

    #[test]
    fn rejection_keeps_edits_and_says_the_fan_was_not_changed() {
        let mut app = curves_tab();
        app.crv_apply_loaded(pwm1_running());
        app.crv_error = Some("curve rejected: below the 35% duty minimum".into());

        let screen = render_app_to_buffer(&app, 140, 24);

        assert!(screen.contains("Not applied: curve rejected"), "{screen}");
        assert!(screen.contains("keeps its previous curve"), "{screen}");
        assert_eq!(app.crv_points, pwm1_running(), "edits kept for fixing");
    }

    #[test]
    fn curve_is_read_from_the_config_reply() {
        let v = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(curve_from_config(&v, "pwm1").unwrap(), pwm1_running());
        assert_eq!(
            curve_from_config(&v, "pwm7").unwrap(),
            Config::default().pwm7.breakpoints
        );
        assert!(curve_from_config(&serde_json::json!({}), "pwm7").is_err());
        assert!(
            curve_from_config(&serde_json::json!({"pwm7": {"breakpoints": []}}), "pwm7").is_err()
        );
    }

    #[test]
    fn daemon_offline_renders_offline_message() {
        let app = AppState {
            daemon_online: false,
            daemon_error: "daemon not running".into(),
            ..AppState::default()
        };
        let content = render_app_to_buffer(&app, 80, 24);
        assert!(
            content.contains("offline")
                || content.contains("not running")
                || content.contains("connecting"),
            "offline state must render informative message"
        );
    }

    #[test]
    fn emergency_mode_renders_emergency_text() {
        let status = StatusResponse {
            channels: vec![ChannelStatus {
                channel: "pwm7".into(),
                intended_pwm: 255,
                readback_pwm: Some(253),
                rpm: Some(2000),
                mode: "emergency".into(),
                override_expires_at: None,
            }],
            temps: TempSnapshot {
                cpu_package_c: Some(95.0),
                cpu_cores_c: vec![Some(90.0)],
                disk_temps_c: vec![],
            },
            uptime_s: 100,
            mode: "emergency".into(),
        };
        let app = AppState {
            daemon_online: true,
            status: Some(status),
            ..AppState::default()
        };
        let content = render_app_to_buffer(&app, 80, 24);
        assert!(
            content.contains("EMERGENCY"),
            "emergency mode must render 'EMERGENCY' text"
        );
    }

    #[test]
    fn emergency_mode_visually_distinct_from_curve() {
        let em_status = StatusResponse {
            channels: vec![ChannelStatus {
                channel: "pwm7".into(),
                intended_pwm: 255,
                readback_pwm: Some(253),
                rpm: Some(2000),
                mode: "emergency".into(),
                override_expires_at: None,
            }],
            temps: TempSnapshot {
                cpu_package_c: Some(95.0),
                cpu_cores_c: vec![],
                disk_temps_c: vec![],
            },
            uptime_s: 100,
            mode: "emergency".into(),
        };
        let cv_status = StatusResponse {
            channels: vec![ChannelStatus {
                channel: "pwm7".into(),
                intended_pwm: 180,
                readback_pwm: Some(178),
                rpm: Some(1200),
                mode: "curve".into(),
                override_expires_at: None,
            }],
            temps: TempSnapshot {
                cpu_package_c: Some(55.0),
                cpu_cores_c: vec![],
                disk_temps_c: vec![],
            },
            uptime_s: 100,
            mode: "normal".into(),
        };

        let em_app = AppState {
            daemon_online: true,
            status: Some(em_status),
            ..AppState::default()
        };
        let cv_app = AppState {
            daemon_online: true,
            status: Some(cv_status),
            ..AppState::default()
        };

        let em_content = render_app_to_buffer(&em_app, 80, 24);
        let cv_content = render_app_to_buffer(&cv_app, 80, 24);

        assert!(em_content.contains("EMERGENCY"));
        assert_ne!(
            em_content, cv_content,
            "emergency and curve renders must differ"
        );
    }

    #[test]
    fn history_with_gaps_renders_without_panic() {
        let app = AppState {
            daemon_online: true,
            tab: Tab::History,
            hist_data: vec![
                valid_slot(0, 320),
                no_data_slot(1),
                valid_slot(2, 350),
                count_zero_slot(3),
                valid_slot(4, 300),
            ],
            ..AppState::default()
        };
        render_app_to_buffer(&app, 80, 24);
    }

    // ── Terminal restore ──────────────────────────────────────────────────────

    #[test]
    fn guard_drops_on_normal_exit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = Arc::clone(&dropped);

        struct MockGuard(Arc<AtomicBool>);
        impl Drop for MockGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        {
            let _g = MockGuard(d2);
            assert!(!dropped.load(Ordering::SeqCst), "must not be dropped yet");
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "Drop must fire on scope exit"
        );
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "panics abort on Darwin/Nix toolchain (no stack unwinding); runs in Linux CI"
    )]
    fn guard_drops_on_panic_unwind() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = Arc::clone(&dropped);

        struct MockGuard(Arc<AtomicBool>);
        impl Drop for MockGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = MockGuard(d2);
            panic!("test panic — verifying Drop fires during unwind");
        }));

        assert!(
            dropped.load(Ordering::SeqCst),
            "Drop must fire during panic unwind"
        );
    }
}
