//! Control loop (T4). The sole EC PWM writer (R1).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use qfan_core::{decide, ChannelConfig, ChannelState, Config, ControlMode, EmergencyLatch, Series};
use qfan_hal::{EcBackend, FanChannel, PwmChannel, TempSensor};
use qfan_hist::{RingBuffer, NO_DATA};

use crate::state::{ChannelSnapshot, Command, Shared, TempSnapshot};

/// Slots in the 30-second trailing window at 2-second resolution (30 / 2 = 15).
const TRAILING_MAX_SLOTS: usize = 15;

const DISKS_INI_PATH: &str = "/var/local/emhttp/disks.ini";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ── Failsafe guard (R2) ───────────────────────────────────────────────────────

/// Sets pwm1=pwm7=255 on drop (normal exit, early return, or panic unwind).
/// Must be constructed before any other loop work and held until the daemon exits.
pub struct FailsafeGuard {
    backend: Arc<dyn EcBackend>,
    dry_run: bool,
}

impl FailsafeGuard {
    pub fn new(backend: Arc<dyn EcBackend>, dry_run: bool) -> Self {
        Self { backend, dry_run }
    }
}

impl Drop for FailsafeGuard {
    fn drop(&mut self) {
        eprintln!("[qfand] FAILSAFE: setting pwm1=255 pwm7=255");
        if !self.dry_run {
            let _ = self.backend.write_pwm(PwmChannel::Pwm1, 255);
            let _ = self.backend.write_pwm(PwmChannel::Pwm7, 255);
        }
    }
}

// ── Disk INI parser ───────────────────────────────────────────────────────────

/// Parse /var/local/emhttp/disks.ini for disk temps.
/// Returns (unraid_name, temp_deci_c) pairs. Never calls smartctl (§2.4).
pub fn parse_disks_ini(path: &str) -> Vec<(String, Option<i16>)> {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    let mut results = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_temp: Option<i16> = None;

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim().to_owned();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if let Some(name) = current_name.take() {
                results.push((name, current_temp.take()));
            }
            current_name = Some(trimmed[1..trimmed.len() - 1].to_owned());
            current_temp = None;
        } else if let Some(rest) = trimmed.strip_prefix("temp=") {
            let val = rest.trim_matches('"');
            if let Ok(c) = val.parse::<i16>() {
                current_temp = Some(c * 10); // whole °C → deci-°C
            }
        }
    }
    if let Some(name) = current_name.take() {
        results.push((name, current_temp.take()));
    }
    results
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn notify_emergency() {
    let _ = std::process::Command::new("/usr/local/emhttp/webGui/scripts/notify")
        .args([
            "-e",
            "qfand",
            "-s",
            "Fan Emergency",
            "-d",
            "CPU package temp ≥90°C: all fans at 100%",
            "-i",
            "alert",
        ])
        .status();
}

fn mode_reason(mode: &ControlMode) -> String {
    match mode {
        ControlMode::Curve => "curve".into(),
        ControlMode::Override { expires_at_secs } => {
            format!("override(exp={})", expires_at_secs)
        }
        ControlMode::Emergency => "emergency".into(),
        ControlMode::FailSafe => "failsafe".into(),
    }
}

// ── Loop state ────────────────────────────────────────────────────────────────

/// Mutable state for one control loop iteration.
/// Extracted from `run_loop` so tests can call `tick()` directly with `FakeBackend`.
pub struct LoopState {
    // Channel configs — mutable for SetCurve commands
    pwm7_cfg: ChannelConfig,
    pwm1_cfg: ChannelConfig,
    emergency_temp_c: i16,
    emergency_release_c: i16,

    // Per-channel control state (managed by decide())
    state_pwm7: ChannelState,
    state_pwm1: ChannelState,
    emergency: EmergencyLatch,

    // Last PWM value written to EC — change detection compares against this, never read_pwm (F7)
    last_written_pwm7: u8,
    last_written_pwm1: u8,

    // CPU package temp ring buffer for 30 s trailing max (T0, deci-°C values)
    pkg_ring: RingBuffer,

    // Latest sensor readings
    disk_temps: Vec<(String, Option<i16>)>,
    fan_rpms: HashMap<FanChannel, Option<u32>>,

    // Tick rate divisors
    fan_every_ticks: u64,
    disk_every_ticks: u64,
    tick: u64,

    // Emergency notification: notify once per engagement (not every tick)
    emergency_notified: bool,

    pub dry_run: bool,
    pub disks_ini_path: String,
}

impl LoopState {
    pub fn new(config: &Config, dry_run: bool) -> Self {
        let fan_every = (config.poll_ec_fan_s / config.poll_coretemp_s).max(1) as u64;
        let disk_every = (config.poll_disks_s / config.poll_coretemp_s).max(1) as u64;
        Self {
            pwm7_cfg: config.pwm7.clone(),
            pwm1_cfg: config.pwm1.clone(),
            emergency_temp_c: config.emergency_temp_c,
            emergency_release_c: config.emergency_release_c,
            state_pwm7: ChannelState::new(255),
            state_pwm1: ChannelState::new(255),
            emergency: EmergencyLatch::new(),
            last_written_pwm7: 255,
            last_written_pwm1: 255,
            pkg_ring: RingBuffer::new(Series::CpuPackage, 1800),
            disk_temps: Vec::new(),
            fan_rpms: HashMap::new(),
            fan_every_ticks: fan_every,
            disk_every_ticks: disk_every,
            tick: 0,
            emergency_notified: false,
            dry_run,
            disks_ini_path: DISKS_INI_PATH.to_owned(),
        }
    }

    /// Process one control tick (called every poll_coretemp_s seconds).
    /// Does not sleep; caller is responsible for pacing.
    pub fn tick(&mut self, backend: &dyn EcBackend, shared: &Shared) {
        let now = now_secs();

        // ── Drain commands at top of tick ─────────────────────────────────────
        for cmd in shared.drain() {
            match cmd {
                Command::SetCurve {
                    channel,
                    config: new_cfg,
                } => {
                    // Build a candidate config to validate (enforces 35% floor, monotonicity, etc.)
                    // A rejected curve must not disturb the running curve.
                    let candidate = Config {
                        poll_coretemp_s: 2,
                        poll_ec_fan_s: 10,
                        poll_disks_s: 30,
                        emergency_temp_c: self.emergency_temp_c,
                        emergency_release_c: self.emergency_release_c,
                        pwm7: if channel == "pwm7" {
                            new_cfg.clone()
                        } else {
                            self.pwm7_cfg.clone()
                        },
                        pwm1: if channel == "pwm1" {
                            new_cfg.clone()
                        } else {
                            self.pwm1_cfg.clone()
                        },
                    };
                    match candidate.validate() {
                        Ok(()) => {
                            if channel == "pwm7" {
                                self.pwm7_cfg = new_cfg;
                            } else if channel == "pwm1" {
                                self.pwm1_cfg = new_cfg;
                            } else {
                                eprintln!("[qfand] SetCurve: unknown channel {channel}");
                            }
                            eprintln!("[qfand] SetCurve accepted for {channel}");
                        }
                        Err(e) => {
                            eprintln!("[qfand] SetCurve rejected for {channel}: {e}");
                        }
                    }
                }
                Command::SetOverride { channel, ov } => {
                    if channel == "pwm7" {
                        self.state_pwm7.set_override(ov);
                    } else if channel == "pwm1" {
                        self.state_pwm1.set_override(ov);
                    }
                }
                Command::ClearOverride { channel } => {
                    if channel == "pwm7" {
                        self.state_pwm7.clear_override();
                    } else if channel == "pwm1" {
                        self.state_pwm1.clear_override();
                    }
                }
            }
        }

        // ── Read coretemp (every tick, cheap ~0.5 ms) ─────────────────────────
        let pkg_deci = backend.read_temp(TempSensor::Package).ok().map(|d| d.0);
        // Push deci-°C into ring (NO_DATA sentinel on failure — ring ignores it in window_max)
        self.pkg_ring.push(pkg_deci.unwrap_or(NO_DATA));

        let core_temps: Vec<Option<i16>> = (0u8..6)
            .map(|i| backend.read_temp(TempSensor::Core(i)).ok().map(|d| d.0))
            .collect();

        // ── Read EC fan RPM (every fan_every_ticks, ~18 ms each — F6) ──────────
        // Lock is NOT held across EC reads (F6).
        if self.tick.is_multiple_of(self.fan_every_ticks) {
            for &ch in &[
                FanChannel::Fan1,
                FanChannel::Fan2,
                FanChannel::Fan3,
                FanChannel::Fan4,
                FanChannel::Fan7,
                FanChannel::Fan8,
            ] {
                self.fan_rpms.insert(ch, backend.read_fan(ch).ok());
            }
        }

        // ── Read disk temps (every disk_every_ticks, cheap file read) ─────────
        if self.tick.is_multiple_of(self.disk_every_ticks) {
            self.disk_temps = parse_disks_ini(&self.disks_ini_path);
        }

        // ── Temperature inputs for control decisions ───────────────────────────
        // pwm7: 30-second trailing max of package temp, in whole °C (§4.2)
        let pkg_trailing_c: Option<i16> = self
            .pkg_ring
            .window_max(TRAILING_MAX_SLOTS)
            .filter(|&v| v != NO_DATA)
            .map(|deci| deci / 10);

        // pwm1: instantaneous max disk temp, in whole °C
        let max_disk_c: Option<i16> = self
            .disk_temps
            .iter()
            .filter_map(|(_, t)| *t)
            .reduce(i16::max)
            .map(|deci| deci / 10);

        // ── Control decisions (R1: only decide() is called here, once per channel) ──
        let dec7 = decide(
            pkg_trailing_c,
            &mut self.state_pwm7,
            &self.pwm7_cfg,
            &mut self.emergency,
            self.emergency_temp_c,
            self.emergency_release_c,
            now,
        );

        // Emergency: notify once per engagement, log if override cleared (§6)
        if dec7.mode == ControlMode::Emergency {
            if !self.emergency_notified {
                self.emergency_notified = true;
                eprintln!(
                    "[qfand] EMERGENCY: package temp ≥{}°C; all fans → 255",
                    self.emergency_temp_c
                );
                notify_emergency();
            }
        } else {
            self.emergency_notified = false;
        }
        if dec7.override_cleared {
            eprintln!("[qfand] override cleared on pwm7: emergency latch engaged");
        }

        let dec1 = decide(
            max_disk_c,
            &mut self.state_pwm1,
            &self.pwm1_cfg,
            &mut self.emergency,
            self.emergency_temp_c,
            self.emergency_release_c,
            now,
        );
        if dec1.override_cleared {
            eprintln!("[qfand] override cleared on pwm1: emergency latch engaged");
        }

        // ── Write PWM (only when intended differs from last-written — F7) ──────
        let reason7 = mode_reason(&dec7.mode);
        let reason1 = mode_reason(&dec1.mode);
        self.maybe_write(backend, PwmChannel::Pwm7, dec7.intended_pwm, &reason7);
        self.maybe_write(backend, PwmChannel::Pwm1, dec1.intended_pwm, &reason1);

        // ── Publish snapshot for socket clients (outside any lock during EC ops) ──
        // Capture values before the publish closure so the closure has minimal size.
        let pub_pkg = pkg_deci;
        let pub_disks = self.disk_temps.clone();
        let pub_cores = core_temps;
        let pub_fan7 = self.fan_rpms.get(&FanChannel::Fan7).and_then(|&r| r);
        let pub_fan1 = self.fan_rpms.get(&FanChannel::Fan1).and_then(|&r| r);
        let emergency_active = self.emergency.is_latched();
        let snap7_intended = dec7.intended_pwm;
        let mode7 = dec7.mode;
        let snap1_intended = dec1.intended_pwm;
        let mode1 = dec1.mode;

        shared.publish(|p| {
            p.emergency = emergency_active;
            p.temps = TempSnapshot {
                cpu_package_c: pub_pkg,
                cpu_cores_c: pub_cores,
                disk_temps_c: pub_disks,
            };
            p.channels = vec![
                ChannelSnapshot {
                    channel: "pwm7",
                    intended_pwm: snap7_intended,
                    readback_pwm: None, // read-back display only; skip to avoid EC cost (F6)
                    rpm: pub_fan7,
                    override_expires_at: match &mode7 {
                        ControlMode::Override { expires_at_secs } => Some(*expires_at_secs),
                        _ => None,
                    },
                    mode: mode7,
                },
                ChannelSnapshot {
                    channel: "pwm1",
                    intended_pwm: snap1_intended,
                    readback_pwm: None,
                    rpm: pub_fan1,
                    override_expires_at: match &mode1 {
                        ControlMode::Override { expires_at_secs } => Some(*expires_at_secs),
                        _ => None,
                    },
                    mode: mode1,
                },
            ];
        });

        self.tick += 1;
    }

    /// Write to EC only if intended != last_written (F7, F6).
    /// Logs every intended write with channel, old, new, and reason (M3 dry-run audit).
    fn maybe_write(
        &mut self,
        backend: &dyn EcBackend,
        channel: PwmChannel,
        intended: u8,
        reason: &str,
    ) {
        let last = match channel {
            PwmChannel::Pwm7 => &mut self.last_written_pwm7,
            PwmChannel::Pwm1 => &mut self.last_written_pwm1,
        };
        if intended == *last {
            return; // F7: no change against last-intended → skip EC write
        }
        if self.dry_run {
            eprintln!(
                "[qfand dry-run] channel={} old={} new={} reason={}",
                channel.sysfs_name(),
                last,
                intended,
                reason
            );
        } else {
            eprintln!(
                "[qfand] write channel={} old={} new={} reason={}",
                channel.sysfs_name(),
                last,
                intended,
                reason
            );
            if let Err(e) = backend.write_pwm(channel, intended) {
                eprintln!("[qfand] write_pwm error on {}: {e}", channel.sysfs_name());
            }
        }
        *last = intended;
    }
}

// ── run_loop ──────────────────────────────────────────────────────────────────

/// Synchronous control loop. Blocks until `stop` is set to `true`.
///
/// Creates a `FailsafeGuard` on entry — it sets pwm1=pwm7=255 on any exit
/// path including panic unwind (R2). Called from `run_daemon` via
/// `tokio::task::spawn_blocking`.
pub fn run_loop(
    backend: Arc<dyn EcBackend>,
    config: Config,
    shared: Shared,
    dry_run: bool,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    // R2: guard must be constructed before any other work
    let _guard = FailsafeGuard::new(Arc::clone(&backend), dry_run);

    let interval = Duration::from_secs(config.poll_coretemp_s as u64);
    let mut state = LoopState::new(&config, dry_run);

    // Start at 255 immediately (failsafe until first tick completes)
    if !dry_run {
        let _ = backend.write_pwm(PwmChannel::Pwm1, 255);
        let _ = backend.write_pwm(PwmChannel::Pwm7, 255);
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let t0 = Instant::now();
        state.tick(backend.as_ref(), &shared);
        if let Some(rem) = interval.checked_sub(t0.elapsed()) {
            std::thread::sleep(rem);
        }
    }
    // _guard drops here → sets pwm1=pwm7=255
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use qfan_core::{ChannelConfig, CurvePoint, DeciUnits, TempSource};
    use qfan_hal::FakeBackend;
    use std::sync::Arc;

    fn default_state() -> (LoopState, Arc<FakeBackend>, Shared) {
        let backend = Arc::new(FakeBackend::new());
        let shared = Shared::new(false, 0);
        let state = LoopState::new(&Config::default(), false);
        (state, backend, shared)
    }

    // ── F7: change detection compares against last-intended, not read-back ────

    #[test]
    fn f7_no_rewrite_on_ec_rounding() {
        // Write 90 → EC stores ec_pwm_round(90) = 89 (read-back is 89).
        // On next tick, intended is still 90.
        // last_written_pwm7 = 90, NOT the read-back 89. So no write should occur.
        let backend = Arc::new(FakeBackend::new());
        let shared = Shared::new(false, 0);
        let mut state = LoopState::new(&Config::default(), false);

        // Simulate having previously written 90
        state.last_written_pwm7 = 90;
        state.state_pwm7 = ChannelState::new(90);

        // Confirm EC read-back rounding
        backend.write_pwm(PwmChannel::Pwm7, 90).unwrap();
        assert_eq!(
            backend.read_pwm(PwmChannel::Pwm7).unwrap(),
            89,
            "FakeBackend must reproduce EC rounding (write 90 → read 89)"
        );

        // Set temp below first breakpoint → curve=90, decide returns 90
        backend.set_temp(TempSensor::Package, DeciUnits(400)); // 40.0 °C

        let counts_before = backend.call_counts();
        state.tick(backend.as_ref(), &shared);
        let counts_after = backend.call_counts();

        let writes_before = counts_before
            .pwm_writes
            .get(&PwmChannel::Pwm7)
            .copied()
            .unwrap_or(0);
        let writes_after = counts_after
            .pwm_writes
            .get(&PwmChannel::Pwm7)
            .copied()
            .unwrap_or(0);
        assert_eq!(
            writes_before, writes_after,
            "EC rounding must not trigger a rewrite: \
             compare against last-intended (90), not read-back (89)"
        );
    }

    // ── Write issued when intended differs ────────────────────────────────────

    #[test]
    fn write_issued_when_intended_differs() {
        // last_written_pwm7 starts at 255. 40°C gives slew-limited ramp-down → 235 ≠ 255.
        let (mut state, backend, shared) = default_state();
        backend.set_temp(TempSensor::Package, DeciUnits(400)); // 40.0 °C

        let counts_before = backend.call_counts();
        state.tick(backend.as_ref(), &shared);
        let counts_after = backend.call_counts();

        let writes = counts_after
            .pwm_writes
            .get(&PwmChannel::Pwm7)
            .copied()
            .unwrap_or(0)
            - counts_before
                .pwm_writes
                .get(&PwmChannel::Pwm7)
                .copied()
                .unwrap_or(0);
        assert_eq!(
            writes, 1,
            "a write must be issued when intended differs from last_written"
        );
    }

    // ── Write skipped when intended equals last-written ───────────────────────

    #[test]
    fn write_skipped_when_intended_unchanged() {
        // last_written_pwm7 = 255. 80°C (above last breakpoint 70°C) → curve=255.
        // 255 == 255 → no write.
        let (mut state, backend, shared) = default_state();
        backend.set_temp(TempSensor::Package, DeciUnits(800)); // 80.0 °C

        let counts_before = backend.call_counts();
        state.tick(backend.as_ref(), &shared);
        let counts_after = backend.call_counts();

        let writes = counts_after
            .pwm_writes
            .get(&PwmChannel::Pwm7)
            .copied()
            .unwrap_or(0)
            - counts_before
                .pwm_writes
                .get(&PwmChannel::Pwm7)
                .copied()
                .unwrap_or(0);
        assert_eq!(
            writes, 0,
            "write must be skipped when intended == last_written"
        );

        let counts_before2 = backend.call_counts();
        state.tick(backend.as_ref(), &shared);
        let counts_after2 = backend.call_counts();
        let writes2 = counts_after2
            .pwm_writes
            .get(&PwmChannel::Pwm7)
            .copied()
            .unwrap_or(0)
            - counts_before2
                .pwm_writes
                .get(&PwmChannel::Pwm7)
                .copied()
                .unwrap_or(0);
        assert_eq!(
            writes2, 0,
            "repeated identical intended values must not rewrite"
        );
    }

    // ── Failsafe guard: normal drop writes 255 ────────────────────────────────

    #[test]
    fn failsafe_guard_normal_drop_writes_255() {
        let backend = Arc::new(FakeBackend::new());
        backend.write_pwm(PwmChannel::Pwm1, 128).unwrap();
        backend.write_pwm(PwmChannel::Pwm7, 128).unwrap();

        {
            let _guard = FailsafeGuard::new(Arc::clone(&backend) as Arc<dyn EcBackend>, false);
        } // guard drops here

        assert_eq!(
            backend.read_pwm(PwmChannel::Pwm1).unwrap(),
            255,
            "failsafe guard must set pwm1=255 on normal drop"
        );
        assert_eq!(
            backend.read_pwm(PwmChannel::Pwm7).unwrap(),
            255,
            "failsafe guard must set pwm7=255 on normal drop"
        );
    }

    // ── Failsafe guard: panic unwind writes 255 (R2) ─────────────────────────

    // Darwin 25 (macOS 15) + Nix: pthread_setspecific returns EIO when
    // initiating a panic inside a test binary, causing the entire process to
    // abort before the stack is unwound. The unwind correctness is proven by
    // `failsafe_guard_normal_drop_writes_255` (same Drop impl) together with
    // Rust's guarantee that stack-allocated values always drop during unwind.
    #[test]
    // This Mac (Darwin 25 + Nix toolchain) aborts instead of unwinding:
    // `fatal runtime error: failed to initiate panic, error 5`. Verified
    // independently by the coordinator — it is an environment quirk, not a
    // defect in the guard.
    //
    // Gated on macOS ONLY, not ignored outright: Linux is the deployment target
    // and CI runs ubuntu-latest, so the unwind path is exercised there on every
    // push. R2 is the reason this daemon exists; asserting it by reasoning on
    // the one platform that cannot run it, and nowhere else, would leave the
    // most safety-critical test permanently unexecuted.
    #[cfg_attr(
        target_os = "macos",
        ignore = "panic!() aborts on Darwin 25/Nix (pthread_setspecific EIO); runs in CI on Linux"
    )]
    fn failsafe_guard_panic_unwind_writes_255() {
        let backend = Arc::new(FakeBackend::new());
        backend.write_pwm(PwmChannel::Pwm1, 100).unwrap();
        backend.write_pwm(PwmChannel::Pwm7, 100).unwrap();

        let backend2 = Arc::clone(&backend);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = FailsafeGuard::new(Arc::clone(&backend2) as Arc<dyn EcBackend>, false);
            panic!("intentional panic — guard Drop must set both channels to 255");
        }));

        assert!(result.is_err(), "catch_unwind must return Err for a panic");
        assert_eq!(backend.read_pwm(PwmChannel::Pwm1).unwrap(), 255);
        assert_eq!(backend.read_pwm(PwmChannel::Pwm7).unwrap(), 255);
    }

    // ── Sensor read None → pwm 255 ────────────────────────────────────────────

    #[test]
    fn sensor_read_none_yields_255() {
        let backend = Arc::new(FakeBackend::new());
        let shared = Shared::new(false, 0);
        let mut state = LoopState::new(&Config::default(), false);

        // Start from 90 so we can detect the failsafe write to 255
        state.last_written_pwm7 = 90;
        state.state_pwm7 = ChannelState::new(90);

        // Inject a read failure — read_temp returns Err → tick sees None
        backend.inject_read_temp_failure(Some("simulated sensor failure"));

        state.tick(backend.as_ref(), &shared);

        // Ring has only NO_DATA → window_max returns None → decide(None) → 255
        assert_eq!(
            state.last_written_pwm7, 255,
            "sensor read failure must produce pwm=255 (failsafe)"
        );
    }

    // ── 30-second trailing max: spike then dip doesn't follow dip ────────────

    #[test]
    fn trailing_max_suppresses_dip() {
        let (mut state, backend, shared) = default_state();

        // Spike: 80 °C (above 70 °C last breakpoint → curve=255)
        backend.set_temp(TempSensor::Package, DeciUnits(800));

        // Fill trailing window (15 slots)
        for _ in 0..TRAILING_MAX_SLOTS {
            state.tick(backend.as_ref(), &shared);
        }
        assert_eq!(
            state.last_written_pwm7, 255,
            "high temp must produce pwm=255"
        );

        // Dip: 40 °C (below first breakpoint → curve=90, but trailing max is still 80 °C)
        backend.set_temp(TempSensor::Package, DeciUnits(400));

        // ONE tick at low temp — trailing max still includes 14 slots at 80 °C
        state.tick(backend.as_ref(), &shared);

        assert_eq!(
            state.last_written_pwm7, 255,
            "one dip tick must not reduce fans: 30 s trailing max still reflects the spike"
        );

        // After TRAILING_MAX_SLOTS more low-temp ticks, all window slots are low
        for _ in 0..TRAILING_MAX_SLOTS {
            state.tick(backend.as_ref(), &shared);
        }
        assert!(
            state.last_written_pwm7 < 255,
            "after window fully flushed, fans must start ramping down"
        );
    }

    // ── Invalid curve via Command rejected; previous curve governs ────────────

    #[test]
    fn invalid_set_curve_rejected_previous_curve_governs() {
        let (mut state, backend, shared) = default_state();

        // Record original first breakpoint
        let orig_pwm = state.pwm7_cfg.breakpoints[0].pwm;
        let orig_temp = state.pwm7_cfg.breakpoints[0].temp_c;

        // Submit a curve that would deliver < 35% duty above 75 °C
        let bad_cfg = ChannelConfig {
            source: TempSource::CpuPackage,
            breakpoints: vec![
                CurvePoint {
                    temp_c: 45,
                    pwm: 10,
                },
                CurvePoint {
                    temp_c: 85,
                    pwm: 10,
                },
            ],
            slew_down_max: 20,
            hysteresis_c: 3,
        };
        shared.submit(Command::SetCurve {
            channel: "pwm7".into(),
            config: bad_cfg,
        });

        backend.set_temp(TempSensor::Package, DeciUnits(500));
        state.tick(backend.as_ref(), &shared);

        assert_eq!(
            state.pwm7_cfg.breakpoints[0].pwm, orig_pwm,
            "invalid curve must be rejected; original curve must still govern"
        );
        assert_eq!(
            state.pwm7_cfg.breakpoints[0].temp_c, orig_temp,
            "rejected curve must not replace the original breakpoints"
        );
    }

    // ── Disks INI parser ──────────────────────────────────────────────────────

    #[test]
    fn parse_disks_ini_basic() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "[disk1]\ndevice=\"sda\"\ntemp=\"34\"\n[cache]\ndevice=\"sdb\"\ntemp=\"40\"\n"
        )
        .unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        let result = parse_disks_ini(&path);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ("disk1".into(), Some(340)));
        assert_eq!(result[1], ("cache".into(), Some(400)));
    }

    #[test]
    fn parse_disks_ini_missing_file_returns_empty() {
        let result = parse_disks_ini("/nonexistent/path/disks.ini");
        assert!(result.is_empty());
    }
}
