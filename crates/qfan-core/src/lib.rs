//! Core types: configuration, fan curves, series definitions, and unit types.
//! No I/O — pure data types and business logic.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Units ─────────────────────────────────────────────────────────────────────

/// Deci-unit newtype: values are tenths (e.g. 345 = 34.5 °C).
/// `i16::MIN` is the explicit no-data sentinel (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeciUnits(pub i16);

impl DeciUnits {
    pub const NO_DATA: DeciUnits = DeciUnits(i16::MIN);
}

// ── Series ────────────────────────────────────────────────────────────────────

/// A named data series stored in the history tiers (§6.4).
/// Fixed series are strongly typed; disk entries carry an Unraid name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum Series {
    CpuPackage,
    CpuCore(u8), // 0..=5
    Pwm(u8),     // 1 or 7
    Fan(u8),     // 1, 2, 3, 4, 7, 8
    /// Keyed by Unraid disk name (parity, disk1..N, cache), never by /dev/sdX.
    Disk(String),
}

impl Series {
    /// Returns the dot-separated name used in file paths and the socket API.
    pub fn name(&self) -> String {
        match self {
            Series::CpuPackage => "cpu.package".into(),
            Series::CpuCore(n) => format!("cpu.core{n}"),
            Series::Pwm(n) => format!("pwm.{n}"),
            Series::Fan(n) => format!("fan.{n}"),
            Series::Disk(name) => format!("disk.{name}"),
        }
    }
}

// ── Curve ─────────────────────────────────────────────────────────────────────

/// One breakpoint in a piecewise-linear fan curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurvePoint {
    /// Trigger temperature in whole °C.
    pub temp_c: i16,
    /// Target PWM value (0–255).
    pub pwm: u8,
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Which temperature measurement drives a fan channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempSource {
    CpuPackage,
    MaxDisk,
    EcBoard,
}

/// Per-channel fan curve configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub source: TempSource,
    pub breakpoints: Vec<CurvePoint>,
    /// Maximum PWM decrease per control cycle (slew-down limit, §4.2).
    pub slew_down_max: u8,
    /// Hysteresis deadband in °C.
    pub hysteresis_c: u8,
}

impl ChannelConfig {
    /// Evaluate the piecewise-linear curve at `temp_c`.
    /// - Below the first breakpoint: clamp to the first breakpoint's pwm.
    /// - Between breakpoints: linearly interpolate.
    /// - Above the last breakpoint: clamp to the last breakpoint's pwm.
    ///
    /// Returns 255 for empty breakpoints (fail-safe). `validate()` also rejects
    /// empty breakpoints, so this case requires bypassing validation; this is
    /// defence in depth — empty curves must never produce fans-off.
    pub fn pwm_at(&self, temp_c: i16) -> u8 {
        let bp = &self.breakpoints;
        if bp.is_empty() {
            return 255;
        }
        if temp_c <= bp[0].temp_c {
            return bp[0].pwm;
        }
        let last = bp[bp.len() - 1];
        if temp_c >= last.temp_c {
            return last.pwm;
        }
        for w in bp.windows(2) {
            let lo = w[0];
            let hi = w[1];
            if temp_c >= lo.temp_c && temp_c <= hi.temp_c {
                let t_range = (hi.temp_c - lo.temp_c) as i32;
                let p_range = hi.pwm as i32 - lo.pwm as i32;
                let t_offset = (temp_c - lo.temp_c) as i32;
                let pwm = lo.pwm as i32 + p_range * t_offset / t_range;
                return pwm.clamp(0, 255) as u8;
            }
        }
        last.pwm
    }
}

fn default_pwm7() -> ChannelConfig {
    ChannelConfig {
        source: TempSource::CpuPackage,
        breakpoints: vec![
            CurvePoint {
                temp_c: 45,
                pwm: 90,
            },
            CurvePoint {
                temp_c: 70,
                pwm: 255,
            },
        ],
        slew_down_max: 20,
        hysteresis_c: 3,
    }
}

fn default_pwm1() -> ChannelConfig {
    ChannelConfig {
        source: TempSource::MaxDisk,
        breakpoints: vec![
            CurvePoint {
                temp_c: 35,
                pwm: 85,
            },
            CurvePoint {
                temp_c: 60,
                pwm: 180,
            },
        ],
        slew_down_max: 20,
        hysteresis_c: 3,
    }
}

fn default_poll_coretemp_s() -> u32 {
    2
}
fn default_poll_ec_fan_s() -> u32 {
    10
}
fn default_poll_disks_s() -> u32 {
    30
}
fn default_emergency_temp_c() -> i16 {
    90
}
fn default_emergency_release_c() -> i16 {
    80
}

/// Top-level daemon configuration, loaded from `/boot/config/plugins/qnap-fan/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// How often to poll coretemp (seconds). Default: 2.
    #[serde(default = "default_poll_coretemp_s")]
    pub poll_coretemp_s: u32,
    /// How often to poll EC fan RPM (seconds). Default: 10.
    #[serde(default = "default_poll_ec_fan_s")]
    pub poll_ec_fan_s: u32,
    /// How often to read disks.ini (seconds). Default: 30.
    #[serde(default = "default_poll_disks_s")]
    pub poll_disks_s: u32,
    /// Package temp threshold for emergency all-fans-to-255 latch (°C). Default: 90.
    #[serde(default = "default_emergency_temp_c")]
    pub emergency_temp_c: i16,
    /// Emergency latch releases when package temp drops below this (°C). Default: 80.
    #[serde(default = "default_emergency_release_c")]
    pub emergency_release_c: i16,
    /// CPU fan (pwm7) curve configuration.
    #[serde(default = "default_pwm7")]
    pub pwm7: ChannelConfig,
    /// System fan (pwm1) curve configuration.
    #[serde(default = "default_pwm1")]
    pub pwm1: ChannelConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_coretemp_s: default_poll_coretemp_s(),
            poll_ec_fan_s: default_poll_ec_fan_s(),
            poll_disks_s: default_poll_disks_s(),
            emergency_temp_c: default_emergency_temp_c(),
            emergency_release_c: default_emergency_release_c(),
            pwm7: default_pwm7(),
            pwm1: default_pwm1(),
        }
    }
}

/// Errors from loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validation: {0}")]
    Validation(String),
}

impl Config {
    /// Load and validate config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate config semantics:
    /// - Breakpoints must be strictly monotonic in temperature.
    /// - PWM 0–255 is enforced by the `u8` type at deserialization.
    /// - No curve may allow PWM < 89 (< 35% duty) at temperatures ≥ 80 °C.
    /// - `slew_down_max` must be at least 1; 0 would prevent fans from ever ramping down.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_channel(&self.pwm7, "pwm7")?;
        validate_channel(&self.pwm1, "pwm1")?;
        if self.emergency_release_c >= self.emergency_temp_c {
            return Err(ConfigError::Validation(
                "emergency_release_c must be less than emergency_temp_c".into(),
            ));
        }
        Ok(())
    }
}

fn validate_channel(ch: &ChannelConfig, name: &str) -> Result<(), ConfigError> {
    if ch.breakpoints.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{name}: breakpoints must not be empty"
        )));
    }

    // slew_down_max = 0 would prevent fans from ever ramping down (D2).
    if ch.slew_down_max == 0 {
        return Err(ConfigError::Validation(format!(
            "{name}: slew_down_max must be at least 1; \
             0 would permanently prevent fans from ramping down"
        )));
    }

    // Strictly monotonic in temperature.
    for w in ch.breakpoints.windows(2) {
        if w[0].temp_c >= w[1].temp_c {
            return Err(ConfigError::Validation(format!(
                "{name}: breakpoints must be strictly monotonic in temperature \
                 (got {}°C then {}°C)",
                w[0].temp_c, w[1].temp_c
            )));
        }
    }

    // Sweep every whole-degree temperature from 80 to 110 °C inclusive.
    // pwm_at() uses the same piecewise-linear + clamp logic the daemon uses,
    // so this catches breakpoint values, interpolated values, AND the clamp
    // region above the last breakpoint — the case the per-breakpoint check missed.
    //
    // The floor is checked in DELIVERED units, not intended ones: the EC rounds
    // writes down to whole percent, so an intended 89 is delivered as 86 (33%
    // duty). Comparing the intended value against 89 would accept a curve that
    // physically runs below the 35% safety floor.
    for t in 80i16..=110 {
        let intended = ch.pwm_at(t);
        let delivered = ec_pwm_round(intended);
        if delivered < DELIVERED_DUTY_FLOOR {
            return Err(ConfigError::Validation(format!(
                "{name}: curve produces pwm {intended} at {t}°C, which the EC \
                 delivers as {delivered} (< {DELIVERED_DUTY_FLOOR}, below the 35% \
                 duty minimum). Use at least {EC_DUTY_FLOOR_35PCT}."
            )));
        }
    }

    Ok(())
}

// ── EC PWM rounding (pure arithmetic; hardware model lives here so the curve
//    validator can enforce a DELIVERED duty floor without depending on qfan-hal) ──

/// Model the QNAP EC's whole-percent PWM rounding (§2.3).
///
/// The EC converts a written 0–255 value to whole percent, then back, so a
/// read-back never equals what was written:
///
/// - Write 180 → stored as 70% → read back **178**
/// - Write 73  → stored as 28% → read back **71**
///
/// Callers comparing for change detection must compare against their own
/// last-intended value, never a read-back, or they will rewrite forever.
pub const fn ec_pwm_round(written: u8) -> u8 {
    let pct = written as u32 * 100 / 255;
    (pct * 255 / 100) as u8
}

/// Smallest value you can WRITE such that `ec_pwm_round(written) >= delivered_floor`.
///
/// Writing a raw floor value is not sufficient, because the EC rounds down:
/// `ec_pwm_round(89) == 86`, which is 33% duty and below the 35% safety limit.
/// Saturates at 255 when no value satisfies the constraint.
pub const fn ec_pwm_min_write_for(delivered_floor: u8) -> u8 {
    let mut w = 0u8;
    loop {
        if ec_pwm_round(w) >= delivered_floor {
            return w;
        }
        if w == 255 {
            return 255;
        }
        w += 1;
    }
}

/// Delivered-PWM floor corresponding to 35% duty.
pub const DELIVERED_DUTY_FLOOR: u8 = 89;

/// Smallest WRITE value that delivers at least 35% duty (≥ 89 after rounding).
///
/// Equals 90: writing 90 delivers 89, whereas writing the raw floor 89 would
/// deliver only 86. Curve validation enforces this composed floor, so a config
/// cannot pass validation and still deliver sub-35% duty on real hardware.
pub const EC_DUTY_FLOOR_35PCT: u8 = ec_pwm_min_write_for(DELIVERED_DUTY_FLOOR);

// ── Override ──────────────────────────────────────────────────────────────────

/// Error from constructing an [`Override`] with an invalid argument.
#[derive(Debug, Error)]
pub enum OverrideError {
    #[error("ttl_s {ttl_s} out of range; must be 1..={max}")]
    InvalidTtl { ttl_s: u32, max: u32 },
    #[error("pwm {pwm} is below the 35% duty minimum; use at least {min}")]
    PwmBelowFloor { pwm: u8, min: u8 },
}

/// A manual PWM override with a mandatory expiry (§4.4).
///
/// The only public constructor requires `ttl_s`. There is no way to construct
/// an `Override` without an expiry — the type makes a TTL-less override a
/// compile-time error. An out-of-range TTL is returned as an `Err`, never
/// silently clamped. This is the type-system encoding of "an override with no
/// timeout is the failure mode that crashed this server."
///
/// Additionally, `pwm` must be at least `EC_DUTY_FLOOR_35PCT` (90). An override
/// below the 35% duty floor is rejected at construction: the 80–89°C range is not
/// covered by the emergency latch, so an override at 10% duty while the CPU is
/// at 85°C would deliver dangerously low airflow with no hardware recovery.
#[derive(Debug, Clone)]
pub struct Override {
    pub pwm: u8,
    expires_at_secs: u64,
}

impl Override {
    /// Default TTL: 10 minutes.
    pub const DEFAULT_TTL_S: u32 = 600;
    /// Hard cap: 1 hour. No override may exceed this.
    pub const MAX_TTL_S: u32 = 3_600;

    /// Construct an override.
    /// - `ttl_s` must be in `1..=MAX_TTL_S`; anything outside is an error, not a clamp.
    /// - `pwm` must be at least `EC_DUTY_FLOOR_35PCT` (90); anything below is an error.
    pub fn new(pwm: u8, ttl_s: u32, now_secs: u64) -> Result<Self, OverrideError> {
        if ttl_s == 0 || ttl_s > Self::MAX_TTL_S {
            return Err(OverrideError::InvalidTtl {
                ttl_s,
                max: Self::MAX_TTL_S,
            });
        }
        if pwm < EC_DUTY_FLOOR_35PCT {
            return Err(OverrideError::PwmBelowFloor {
                pwm,
                min: EC_DUTY_FLOOR_35PCT,
            });
        }
        Ok(Self {
            pwm,
            // saturating_add: now_secs near u64::MAX saturates to u64::MAX, keeping
            // is_active() = true rather than wrapping to a past timestamp (G1).
            expires_at_secs: now_secs.saturating_add(ttl_s as u64),
        })
    }

    pub fn is_active(&self, now_secs: u64) -> bool {
        now_secs < self.expires_at_secs
    }

    pub fn expires_at_secs(&self) -> u64 {
        self.expires_at_secs
    }
}

// ── Emergency latch ───────────────────────────────────────────────────────────

/// Latching emergency state (§4.3).
///
/// Activates at or above `emergency_temp` and only releases strictly below
/// `release_temp`. The two-threshold design prevents chatter at the boundary.
#[derive(Debug, Default)]
pub struct EmergencyLatch {
    latched: bool,
}

impl EmergencyLatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the latest temperature. Returns `true` if the latch just activated
    /// this call (useful for daemon logging and Unraid notification on entry).
    pub fn update(&mut self, temp_c: i16, emergency_temp: i16, release_temp: i16) -> bool {
        let was_latched = self.latched;
        if temp_c >= emergency_temp {
            self.latched = true;
        } else if temp_c < release_temp {
            self.latched = false;
        }
        // Between release_temp and emergency_temp: maintain current state.
        self.latched && !was_latched
    }

    pub fn is_latched(&self) -> bool {
        self.latched
    }
}

// ── Control decision ──────────────────────────────────────────────────────────

/// Which control mode is in effect for a given decision.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMode {
    /// Normal curve + asymmetric slew + hysteresis.
    Curve,
    /// Manual override in effect until `expires_at_secs`.
    Override { expires_at_secs: u64 },
    /// Emergency latch active: all channels at 255.
    Emergency,
    /// Sensor read failed: treating temperature as hot, channel at 255.
    FailSafe,
}

/// The result of one control cycle for a channel.
#[derive(Debug, Clone)]
pub struct ControlDecision {
    pub intended_pwm: u8,
    pub mode: ControlMode,
    /// True when an active manual override was cleared because the emergency
    /// latch just engaged. The daemon should log this event: the operator's
    /// earlier manual setting has been discarded in favour of the failsafe.
    pub override_cleared: bool,
}

// ── Channel state ─────────────────────────────────────────────────────────────

/// Per-channel control state: last intended PWM, active override, hysteresis reference.
#[derive(Debug)]
pub struct ChannelState {
    /// The last PWM value this crate intended to write.
    /// Change detection compares against this, never against EC readback (§2.3).
    pub last_intended_pwm: u8,
    /// Active manual override, if any.
    pub active_override: Option<Override>,
    /// Temperature reference for the hysteresis filter.
    /// `None` on the first cycle; treated as "no history" (all moves allowed).
    reference_temp: Option<i16>,
}

impl ChannelState {
    pub fn new(initial_pwm: u8) -> Self {
        Self {
            last_intended_pwm: initial_pwm,
            active_override: None,
            reference_temp: None,
        }
    }

    pub fn set_override(&mut self, ov: Override) {
        self.active_override = Some(ov);
        self.reference_temp = None; // reset so curve resumes from clean state on expiry
    }

    pub fn clear_override(&mut self) {
        self.active_override = None;
        self.reference_temp = None;
    }
}

// ── Control decision function ─────────────────────────────────────────────────

/// Compute the next intended PWM for one channel (pure, no I/O, no clock).
///
/// # Precedence (highest first)
/// 1. `temp == None` → 255 (`FailSafe`). Unreadable temperature is treated as hot.
/// 2. Emergency latch active → 255. The latch overrides any manual override.
/// 3. Active manual override → `override.pwm`.
/// 4. Curve + asymmetric slew + hysteresis.
///
/// # Emergency latch update
/// This function calls `emergency.update()` internally using `emergency_temp` and
/// `release_temp`, so the caller cannot call `decide()` before updating the latch.
/// When the latch transitions from inactive to active, any active override is cleared
/// and `ControlDecision::override_cleared` is set to `true` — the daemon should log
/// this, as the operator's earlier manual setting has been discarded.
///
/// # Hysteresis
/// The deadband filter is applied to the **temperature signal** before curve
/// evaluation, not to the PWM output. Rising temperature updates the reference
/// immediately; falling temperature only updates the reference after a drop of
/// at least `hysteresis_c`. This structural separation guarantees that the
/// ramp-up path never executes hysteresis logic — a deadband on the PWM output
/// could suppress ramp-up, which is a safety regression.
///
/// # Slew
/// Ramp UP is immediate (no per-cycle limit). Ramp DOWN is capped at
/// `slew_down_max` per call. Asymmetry is a safety property: being late to
/// speed up is dangerous; being late to slow down is merely noisy.
pub fn decide(
    temp: Option<i16>,
    channel: &mut ChannelState,
    config: &ChannelConfig,
    emergency: &mut EmergencyLatch,
    emergency_temp: i16,
    release_temp: i16,
    now_secs: u64,
) -> ControlDecision {
    // 1. Sensor failure: unreadable temperature is treated as hot.
    let temp_c = match temp {
        None => {
            channel.last_intended_pwm = 255;
            return ControlDecision {
                intended_pwm: 255,
                mode: ControlMode::FailSafe,
                override_cleared: false,
            };
        }
        Some(t) => t,
    };

    // 2. Update emergency latch. When the latch engages (false→true), any active
    //    override is cleared: the operator's earlier manual judgement is not
    //    trustworthy while the machine is in thermal distress, and a stale
    //    override at the duty floor could hold fans slow after the latch releases
    //    into a hot state (A2).
    let just_engaged = emergency.update(temp_c, emergency_temp, release_temp);
    let override_cleared = just_engaged && channel.active_override.is_some();
    if override_cleared {
        channel.clear_override();
    }

    if emergency.is_latched() {
        channel.last_intended_pwm = 255;
        return ControlDecision {
            intended_pwm: 255,
            mode: ControlMode::Emergency,
            override_cleared,
        };
    }

    // 3. Manual override — extract values before mutating to avoid borrow conflict.
    let ov_state = channel.active_override.as_ref().map(|ov| {
        if ov.is_active(now_secs) {
            Some((ov.pwm, ov.expires_at_secs()))
        } else {
            None // expired
        }
    });
    match ov_state {
        Some(Some((pwm, exp))) => {
            channel.last_intended_pwm = pwm;
            return ControlDecision {
                intended_pwm: pwm,
                mode: ControlMode::Override {
                    expires_at_secs: exp,
                },
                override_cleared: false,
            };
        }
        Some(None) => {
            // Override existed but expired: clear and fall through to curve.
            channel.active_override = None;
            channel.reference_temp = None;
        }
        None => {}
    }

    // 4. Curve + hysteresis (temperature-signal filter) + asymmetric slew.
    //
    // The filter only gates the reference_temp update:
    //   - temp rising above reference → update immediately (no hysteresis for up).
    //   - temp falling >= hysteresis_c below reference → update (significant drop).
    //   - temp falling < hysteresis_c → hold reference (suppress small oscillations).
    let ref_t = match channel.reference_temp {
        None => {
            // First cycle: no history, accept the current temperature directly.
            channel.reference_temp = Some(temp_c);
            temp_c
        }
        Some(r) => {
            let drop_threshold = r.saturating_sub(config.hysteresis_c as i16);
            if temp_c > r || temp_c < drop_threshold {
                channel.reference_temp = Some(temp_c);
                temp_c
            } else {
                r // inside deadband: hold reference temperature
            }
        }
    };

    let target = config.pwm_at(ref_t);
    let last = channel.last_intended_pwm;

    let new_pwm = if target > last {
        target // ramp up immediately — no slew limit (safety: late to speed up is dangerous)
    } else if target < last {
        target.max(last.saturating_sub(config.slew_down_max)) // slew-limited ramp down
    } else {
        last
    };

    channel.last_intended_pwm = new_pwm;
    ControlDecision {
        intended_pwm: new_pwm,
        mode: ControlMode::Curve,
        override_cleared: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_defaults_ok() {
        Config::default()
            .validate()
            .expect("default config must be valid");
    }

    #[test]
    fn reject_non_monotonic_breakpoints() {
        let mut config = Config::default();
        // Temperature decreases from first to second breakpoint.
        config.pwm7.breakpoints = vec![
            CurvePoint {
                temp_c: 70,
                pwm: 255,
            },
            CurvePoint {
                temp_c: 45,
                pwm: 90,
            },
        ];
        assert!(
            config.validate().is_err(),
            "non-monotonic breakpoints must be rejected"
        );
    }

    #[test]
    fn reject_pwm_out_of_range_at_type_boundary() {
        // u8 enforces 0..=255; toml::from_str rejects pwm = 256.
        let bad_toml = r#"
poll_coretemp_s = 2
poll_ec_fan_s = 10
poll_disks_s = 30
emergency_temp_c = 90
emergency_release_c = 80

[pwm7]
source = "cpu_package"
slew_down_max = 20
hysteresis_c = 3

[[pwm7.breakpoints]]
temp_c = 45
pwm = 256

[pwm1]
source = "max_disk"
slew_down_max = 20
hysteresis_c = 3

[[pwm1.breakpoints]]
temp_c = 35
pwm = 85

[[pwm1.breakpoints]]
temp_c = 60
pwm = 180
"#;
        let result: Result<Config, _> = toml::from_str(bad_toml);
        assert!(result.is_err(), "pwm = 256 must fail u8 deserialization");
    }

    #[test]
    fn reject_low_pwm_above_80c() {
        let mut config = Config::default();
        // 85 < 89 at 85 °C (above 80 °C) — dangerously low duty cycle.
        config.pwm7.breakpoints = vec![
            CurvePoint {
                temp_c: 45,
                pwm: 90,
            },
            CurvePoint {
                temp_c: 85,
                pwm: 85,
            },
        ];
        assert!(
            config.validate().is_err(),
            "pwm < 89 above 80°C must be rejected"
        );
    }

    #[test]
    fn reject_clamp_region_low_pwm() {
        // Regression test for the clamp-region hole: both breakpoints are below
        // 80 °C, so the curve clamps to pwm=60 for every temp above 70 °C.
        // Previously accepted; must now be rejected.
        let mut config = Config::default();
        config.pwm7.breakpoints = vec![
            CurvePoint {
                temp_c: 45,
                pwm: 50,
            },
            CurvePoint {
                temp_c: 70,
                pwm: 60,
            },
        ];
        assert!(
            config.validate().is_err(),
            "curve that clamps to pwm 60 above 70°C must be rejected (< 89 at 80°C+)"
        );
    }

    #[test]
    fn pwm_at_interpolates_mid_segment() {
        let ch = default_pwm7();
        // Default pwm7: 45°C→90, 70°C→255. Mid-point is 57.5°C → ~172 PWM.
        let at_45 = ch.pwm_at(45);
        let at_57 = ch.pwm_at(57);
        let at_70 = ch.pwm_at(70);
        assert_eq!(at_45, 90, "at first breakpoint");
        assert_eq!(at_70, 255, "at last breakpoint");
        assert!(
            at_57 > 90 && at_57 < 255,
            "mid-segment pwm_at(57) = {at_57} must be strictly between 90 and 255"
        );
        // Strictly increasing across the segment.
        assert!(
            ch.pwm_at(50) < ch.pwm_at(60),
            "pwm_at must be strictly increasing across the 45→70 segment"
        );
    }

    #[test]
    fn pwm_at_clamps_outside_breakpoints() {
        let ch = default_pwm7();
        // Below first breakpoint → clamp to first pwm.
        assert_eq!(ch.pwm_at(0), 90, "below first breakpoint clamps to 90");
        assert_eq!(
            ch.pwm_at(44),
            90,
            "just below first breakpoint clamps to 90"
        );
        // Above last breakpoint → clamp to last pwm.
        assert_eq!(
            ch.pwm_at(71),
            255,
            "just above last breakpoint clamps to 255"
        );
        assert_eq!(
            ch.pwm_at(120),
            255,
            "well above last breakpoint clamps to 255"
        );
    }

    // ── R1a: curve properties ──────────────────────────────────────────────────

    #[test]
    fn r1a_pwm_at_non_decreasing() {
        let ch = default_pwm7();
        let mut prev = ch.pwm_at(-50);
        for t in -49i16..=200 {
            let p = ch.pwm_at(t);
            assert!(
                p >= prev,
                "pwm_at must be non-decreasing (dropped at t={t}: {prev} -> {p})"
            );
            prev = p;
        }
    }

    // ── R1b: slew asymmetry ────────────────────────────────────────────────────

    #[test]
    fn r1b_slew_asymmetry() {
        // Flat curve: pwm = temp value directly for easy math.
        let config = ChannelConfig {
            source: TempSource::CpuPackage,
            breakpoints: vec![
                CurvePoint { temp_c: 0, pwm: 0 },
                CurvePoint {
                    temp_c: 200,
                    pwm: 200,
                },
            ],
            slew_down_max: 20,
            hysteresis_c: 0, // disabled for clean slew test
        };
        // Use i16::MAX as emergency_temp so the 200°C test temperature does not
        // trigger the latch.
        let mut latch = EmergencyLatch::new();

        // Up: last=50, temp gives target=200 → reach in ONE cycle.
        let mut ch = ChannelState::new(50);
        let d = decide(Some(200), &mut ch, &config, &mut latch, i16::MAX, 0, 0);
        assert_eq!(
            d.intended_pwm, 200,
            "ramp up must reach target in one cycle"
        );

        // Down: last=200, target=0, slew_down_max=20.
        // Expected cycles = ceil(200 / 20) = 10.
        let mut ch = ChannelState::new(200);
        let mut cycles = 0u32;
        loop {
            let d = decide(Some(0), &mut ch, &config, &mut latch, i16::MAX, 0, 0);
            cycles += 1;
            if d.intended_pwm == 0 {
                break;
            }
            assert!(cycles <= 15, "ramp down taking too long");
        }
        assert_eq!(
            cycles, 10,
            "ramp down from 200 to 0 must take ceil(200/20)=10 cycles"
        );
    }

    // ── R1c: hysteresis ────────────────────────────────────────────────────────

    #[test]
    fn r1c_hysteresis() {
        let config = ChannelConfig {
            source: TempSource::CpuPackage,
            breakpoints: vec![
                CurvePoint {
                    temp_c: 40,
                    pwm: 90,
                },
                CurvePoint {
                    temp_c: 80,
                    pwm: 200,
                },
            ],
            slew_down_max: 100, // large enough to not interfere
            hysteresis_c: 5,
        };
        let mut latch = EmergencyLatch::new();
        let mut ch = ChannelState::new(90);

        // Rise to 60°C → reference updates immediately, PWM increases.
        let d = decide(Some(60), &mut ch, &config, &mut latch, 90, 80, 0);
        let pwm_60 = d.intended_pwm;
        assert!(
            pwm_60 > 90,
            "60°C must produce pwm above first-breakpoint floor"
        );

        // Drop 2°C to 58°C (drop < hysteresis=5): PWM must NOT decrease.
        let d = decide(Some(58), &mut ch, &config, &mut latch, 90, 80, 0);
        assert_eq!(
            d.intended_pwm, pwm_60,
            "2°C drop inside 5°C deadband must not reduce PWM"
        );

        // Rise to 65°C: hysteresis must NOT delay the increase (ramp-up immediate).
        let d = decide(Some(65), &mut ch, &config, &mut latch, 90, 80, 0);
        let pwm_65 = d.intended_pwm;
        assert!(
            pwm_65 > pwm_60,
            "rise from 60 to 65°C must increase PWM immediately (hysteresis must not damp up)"
        );

        // Drop exactly hysteresis_c (5°C) from 65 to 60: boundary — still inside deadband.
        // drop_threshold = 65 - 5 = 60; condition is temp < 60, so 60 is NOT < 60 → hold.
        let d = decide(Some(60), &mut ch, &config, &mut latch, 90, 80, 0);
        assert_eq!(
            d.intended_pwm, pwm_65,
            "drop equal to hysteresis_c (boundary) must still hold"
        );

        // Drop 6°C to 59°C (> hysteresis=5 from reference 65): must allow reduction.
        let d = decide(Some(59), &mut ch, &config, &mut latch, 90, 80, 0);
        assert!(
            d.intended_pwm < pwm_65,
            "6°C drop past deadband must reduce PWM"
        );
    }

    // ── R1d: emergency latch ───────────────────────────────────────────────────

    #[test]
    fn r1d_emergency_latch() {
        let cfg = Config::default();
        let ch_cfg = cfg.pwm7.clone();
        let mut latch = EmergencyLatch::new();
        let mut ch = ChannelState::new(100);
        let (et, rt) = (cfg.emergency_temp_c, cfg.emergency_release_c); // 90, 80

        // Latch must engage at emergency_temp (90°C): mode is Emergency.
        let d = decide(Some(90), &mut ch, &ch_cfg, &mut latch, et, rt, 0);
        assert_eq!(d.intended_pwm, 255);
        assert_eq!(
            d.mode,
            ControlMode::Emergency,
            "must latch at emergency_temp"
        );
        assert!(latch.is_latched());

        // 85°C (between release=80 and emergency=90): must stay latched.
        let d = decide(Some(85), &mut ch, &ch_cfg, &mut latch, et, rt, 1);
        assert_eq!(d.mode, ControlMode::Emergency, "latch must hold at 85°C");

        // 79°C (< release=80): latch must release; curve resumes.
        let _d = decide(Some(79), &mut ch, &ch_cfg, &mut latch, et, rt, 2);
        assert!(!latch.is_latched(), "latch must release below release_temp");

        // After release, curve resumes.
        let d = decide(Some(50), &mut ch, &ch_cfg, &mut latch, et, rt, 3);
        assert_ne!(d.intended_pwm, 255, "curve must resume after release");
        assert_eq!(d.mode, ControlMode::Curve);

        // A2 (strengthened): set an override with the MINIMUM valid PWM while NOT
        // latched, then trigger the latch.  The override must be cleared on engagement
        // so it cannot survive latch release into a hot state.
        let ov = Override::new(EC_DUTY_FLOOR_35PCT, 600, 4).unwrap();
        ch.set_override(ov);
        assert!(ch.active_override.is_some());

        // Latch engages at 95°C: override must be cleared, mode Emergency.
        let d = decide(Some(95), &mut ch, &ch_cfg, &mut latch, et, rt, 4);
        assert_eq!(
            d.mode,
            ControlMode::Emergency,
            "emergency must override manual override"
        );
        assert_eq!(d.intended_pwm, 255);
        assert!(
            d.override_cleared,
            "override_cleared must be true when latch engages with active override"
        );
        assert!(
            ch.active_override.is_none(),
            "active_override must be cleared from channel state on latch engagement"
        );

        // Latch releases at 79°C.
        let _d = decide(Some(79), &mut ch, &ch_cfg, &mut latch, et, rt, 5);
        assert!(!latch.is_latched());

        // Post-release at 85°C: curve (255) must win, not stale override.
        let d = decide(Some(85), &mut ch, &ch_cfg, &mut latch, et, rt, 6);
        assert_eq!(
            d.mode,
            ControlMode::Curve,
            "curve must control after latch release, not stale override"
        );
        // Default curve at 85°C: above last breakpoint (70°C→255), clamps to 255.
        assert_eq!(
            d.intended_pwm, 255,
            "post-release at 85°C must use curve (255), not stale override floor"
        );

        // Emergency still takes precedence over a fresh override.
        let ov2 = Override::new(200, 600, 7).unwrap();
        ch.set_override(ov2);
        let d = decide(Some(90), &mut ch, &ch_cfg, &mut latch, et, rt, 7);
        assert_eq!(
            d.mode,
            ControlMode::Emergency,
            "emergency must override a fresh manual override"
        );
        assert_eq!(d.intended_pwm, 255);
        assert!(
            d.override_cleared,
            "override must be cleared when latch engages"
        );
    }

    // ── R1e: failed sensor reading ─────────────────────────────────────────────

    #[test]
    fn r1e_failed_reading() {
        let config = default_pwm7();
        let mut latch = EmergencyLatch::new();
        let mut ch = ChannelState::new(100);

        let d = decide(None, &mut ch, &config, &mut latch, 90, 80, 0);
        assert_eq!(d.intended_pwm, 255, "None temp must yield 255");
        assert_eq!(d.mode, ControlMode::FailSafe);
        // Channel state must also reflect 255 so daemon does not write stale value.
        assert_eq!(ch.last_intended_pwm, 255);
    }

    // ── R5a: override TTL validation ───────────────────────────────────────────

    #[test]
    fn r5a_override_ttl_validation() {
        // Valid range.
        assert!(Override::new(200, 1, 0).is_ok(), "ttl=1 must be valid");
        assert!(
            Override::new(200, Override::DEFAULT_TTL_S, 0).is_ok(),
            "default TTL must be valid"
        );
        assert!(
            Override::new(200, Override::MAX_TTL_S, 0).is_ok(),
            "max TTL must be valid"
        );

        // ttl=0: rejected (not silently clamped to 1).
        let e = Override::new(200, 0, 0);
        assert!(e.is_err(), "ttl=0 must be rejected, not clamped");

        // ttl > MAX: rejected (not silently clamped).
        let e = Override::new(200, Override::MAX_TTL_S + 1, 0);
        assert!(e.is_err(), "ttl > MAX must be rejected, not clamped");

        let e = Override::new(200, u32::MAX, 0);
        assert!(e.is_err(), "ttl=u32::MAX must be rejected");

        // A1 (strengthened): PWM below the 35% duty floor must be rejected regardless
        // of TTL.  The 80–89°C range is unprotected by the emergency latch; a low
        // override in that range would deliver dangerous airflow with no hardware recovery.
        assert!(
            Override::new(EC_DUTY_FLOOR_35PCT - 1, 600, 0).is_err(),
            "pwm one below the duty floor must be rejected"
        );
        assert!(Override::new(0, 600, 0).is_err(), "pwm=0 must be rejected");
        // The floor itself is accepted.
        assert!(
            Override::new(EC_DUTY_FLOOR_35PCT, 600, 0).is_ok(),
            "pwm exactly at the duty floor must be accepted"
        );
    }

    // ── R5b: override expiry ───────────────────────────────────────────────────

    #[test]
    fn r5b_override_expiry() {
        let config = default_pwm7();
        let mut latch = EmergencyLatch::new();
        let mut ch = ChannelState::new(100);

        // Use the minimum-valid PWM at a hot temperature to prove that expiry
        // actually hands control back to the curve rather than silently leaving
        // fans at the duty floor.
        let ov = Override::new(EC_DUTY_FLOOR_35PCT, 600, 0).unwrap();
        ch.set_override(ov);

        // At t=599 and 85°C: override is active (EC_DUTY_FLOOR_35PCT), even
        // though the curve at 85°C would give 255.
        let d = decide(Some(85), &mut ch, &config, &mut latch, 90, 80, 599);
        assert_eq!(
            d.intended_pwm, EC_DUTY_FLOOR_35PCT,
            "override must be active before expiry"
        );
        assert!(
            matches!(d.mode, ControlMode::Override { .. }),
            "mode must be Override before expiry"
        );

        // At t=600 and 85°C: override expires; curve resumes.
        // Default curve at 85°C: above last breakpoint (70°C→255), clamps to 255.
        // Ramp-up is immediate (last=EC_DUTY_FLOOR_35PCT, target=255 → 255 > 90).
        let d = decide(Some(85), &mut ch, &config, &mut latch, 90, 80, 600);
        assert_ne!(
            d.intended_pwm, EC_DUTY_FLOOR_35PCT,
            "override must have expired at t=600"
        );
        assert_eq!(
            d.mode,
            ControlMode::Curve,
            "mode must return to Curve after override expiry"
        );
        assert_eq!(
            d.intended_pwm, 255,
            "post-expiry at 85°C must use curve (255), not leave fans at override floor"
        );
    }

    // ── New tests: one per finding ─────────────────────────────────────────────

    /// A1: Override::new must reject pwm below the 35% duty floor.
    #[test]
    fn a1_override_pwm_floor_rejected() {
        // The emergency latch only fires at ≥90°C; the 80–89°C band has no
        // latch protection, so an override at e.g. 10% duty at 85°C is a real hazard.
        assert!(
            Override::new(EC_DUTY_FLOOR_35PCT - 1, 600, 0).is_err(),
            "pwm one below the floor must be rejected"
        );
        assert!(Override::new(0, 600, 0).is_err(), "pwm=0 must be rejected");
        // The floor itself is accepted.
        assert!(
            Override::new(EC_DUTY_FLOOR_35PCT, 600, 0).is_ok(),
            "pwm at the floor must be accepted"
        );
    }

    /// A2: Override is cleared when the latch engages; post-release decide()
    /// at 85°C returns the curve value (255), not the stale override floor.
    #[test]
    fn a2_override_cleared_on_latch_engagement() {
        let ch_cfg = default_pwm7();
        let mut latch = EmergencyLatch::new();
        let mut ch = ChannelState::new(100);

        // Set override at the minimum valid PWM while the latch is NOT engaged.
        ch.set_override(Override::new(EC_DUTY_FLOOR_35PCT, 600, 0).unwrap());
        assert!(ch.active_override.is_some());

        // Latch engages at 95°C: override must be cleared, mode Emergency.
        let d = decide(Some(95), &mut ch, &ch_cfg, &mut latch, 90, 80, 1);
        assert_eq!(d.mode, ControlMode::Emergency);
        assert_eq!(d.intended_pwm, 255);
        assert!(
            d.override_cleared,
            "override_cleared must be set when latch engages with an active override"
        );
        assert!(
            ch.active_override.is_none(),
            "override must be absent from channel state after engagement"
        );

        // Latch releases at 79°C.
        let _d = decide(Some(79), &mut ch, &ch_cfg, &mut latch, 90, 80, 2);
        assert!(!latch.is_latched());

        // Post-release at 85°C: curve (255) must control, not the cleared override.
        let d = decide(Some(85), &mut ch, &ch_cfg, &mut latch, 90, 80, 3);
        assert_eq!(
            d.mode,
            ControlMode::Curve,
            "post-release must use curve, not stale override"
        );
        assert_eq!(
            d.intended_pwm, 255,
            "curve at 85°C (above last breakpoint 70°C→255) must give 255, not stale floor"
        );
    }

    /// G1: Override::new with now_secs near u64::MAX must not panic.
    #[test]
    fn g1_override_new_no_overflow() {
        // With saturating_add, now_secs + ttl_s saturates to u64::MAX instead of
        // wrapping.  This must not panic in debug or release builds.
        let result = Override::new(200, 3600, u64::MAX - 100);
        assert!(result.is_ok(), "Override::new must not panic near u64::MAX");
        // The override's expiry saturates to u64::MAX, keeping it active for all
        // practical now_secs values.
        let ov = result.unwrap();
        assert!(
            ov.is_active(u64::MAX - 50),
            "saturated expiry must remain active"
        );
    }

    /// E3: pwm_at() with empty breakpoints must return 255, not 0.
    #[test]
    fn e3_pwm_at_empty_breakpoints_returns_255() {
        let config = ChannelConfig {
            source: TempSource::CpuPackage,
            breakpoints: vec![],
            slew_down_max: 20,
            hysteresis_c: 3,
        };
        // validate() also rejects empty breakpoints; this tests the pub API
        // directly to confirm it fails safe rather than producing fans-off.
        assert_eq!(
            config.pwm_at(50),
            255,
            "empty breakpoints must return 255 (fail-safe), not 0"
        );
    }

    /// B1: decide() on a fresh (never-updated) latch at 95°C must report Emergency.
    #[test]
    fn b1_decide_hot_start_reports_emergency() {
        let ch_cfg = default_pwm7();
        let mut latch = EmergencyLatch::new(); // never externally updated
        let mut ch = ChannelState::new(100);
        // decide() must call latch.update() internally before checking is_latched(),
        // so the first call at 95°C correctly engages the latch.
        let d = decide(Some(95), &mut ch, &ch_cfg, &mut latch, 90, 80, 0);
        assert_eq!(
            d.mode,
            ControlMode::Emergency,
            "first decide() at 95°C must report Emergency, not Curve"
        );
        assert_eq!(d.intended_pwm, 255);
    }

    /// D2: validate() must reject slew_down_max = 0.
    #[test]
    fn d2_slew_down_max_zero_rejected() {
        let mut config = Config::default();
        config.pwm7.slew_down_max = 0;
        assert!(
            config.validate().is_err(),
            "slew_down_max = 0 must be rejected: fans could never ramp down"
        );
    }
}
