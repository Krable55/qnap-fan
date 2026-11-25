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
    pub fn pwm_at(&self, temp_c: i16) -> u8 {
        let bp = &self.breakpoints;
        if bp.is_empty() {
            return 0;
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
    for t in 80i16..=110 {
        let pwm = ch.pwm_at(t);
        if pwm < 89 {
            return Err(ConfigError::Validation(format!(
                "{name}: curve produces pwm {pwm} < 89 (35% duty minimum) at {t}°C"
            )));
        }
    }

    Ok(())
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
}
