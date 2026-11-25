//! Hardware abstraction layer: EC backend trait, sysfs and fake implementations,
//! and hwmon discovery (§2.1, §10).

#![allow(clippy::todo)]

use anyhow::Result;
use qfan_core::DeciUnits;

// ── Backend trait ─────────────────────────────────────────────────────────────

/// Hardware access abstraction for the QNAP EC and coretemp hwmon.
/// The daemon is the sole writer of EC PWM values (§3, D3).
pub trait EcBackend: Send + Sync {
    /// Read a temperature from the named hwmon input (e.g. `temp1_input`).
    fn read_temp(&self, sensor: &str) -> Result<DeciUnits>;
    /// Read fan RPM from the given EC channel number.
    fn read_fan(&self, channel: u8) -> Result<u32>;
    /// Read the current PWM setting. Compare against daemon's last-intended
    /// value, not this read-back — see §2.3 for why.
    fn read_pwm(&self, channel: u8) -> Result<u8>;
    /// Write a PWM value to the EC. Only call when the intended value has
    /// changed from the last-intended value (§2.3).
    fn write_pwm(&self, channel: u8, pwm: u8) -> Result<()>;
}

// ── hwmon discovery ───────────────────────────────────────────────────────────

/// Resolved hwmon paths after scanning `/sys/class/hwmon/*/name` (§2.1).
/// Re-resolve if any read returns ENOENT — the hwmonN index is not stable.
pub struct HwmonPaths {
    /// Path to the qnap_ec hwmon directory.
    pub qnap_ec: std::path::PathBuf,
    /// Path to the coretemp hwmon directory.
    pub coretemp: std::path::PathBuf,
}

impl HwmonPaths {
    /// Scan sysfs for the `qnap_ec` and `coretemp` hwmon entries.
    pub fn discover() -> Result<Self> {
        todo!("T2: scan /sys/class/hwmon/*/name for qnap_ec and coretemp")
    }
}

// ── Sysfs backend ─────────────────────────────────────────────────────────────

/// Real hardware backend that reads and writes sysfs files via the qnap-ec
/// kernel module (§2.2: each access is a process spawn, ~12–18 ms).
pub struct SysfsBackend {
    #[allow(dead_code)]
    paths: HwmonPaths,
}

impl SysfsBackend {
    pub fn new(paths: HwmonPaths) -> Self {
        Self { paths }
    }
}

impl EcBackend for SysfsBackend {
    fn read_temp(&self, _sensor: &str) -> Result<DeciUnits> {
        todo!("T2: read coretemp sysfs input")
    }

    fn read_fan(&self, _channel: u8) -> Result<u32> {
        todo!("T2: read EC fan RPM from sysfs")
    }

    fn read_pwm(&self, _channel: u8) -> Result<u8> {
        todo!("T2: read EC PWM from sysfs")
    }

    fn write_pwm(&self, _channel: u8, _pwm: u8) -> Result<()> {
        todo!("T2: write EC PWM to sysfs")
    }
}

// ── Fake backend ──────────────────────────────────────────────────────────────

/// Scriptable test backend with injectable temperatures, simulated read/write
/// failures, a simple thermal model, and per-call latency matching §2.2
/// (12–18 ms) so the EC cost budget is exercised in tests (§10).
pub struct FakeBackend;

impl FakeBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EcBackend for FakeBackend {
    fn read_temp(&self, _sensor: &str) -> Result<DeciUnits> {
        todo!("T2: scriptable fake backend — inject via FakeBackend config")
    }

    fn read_fan(&self, _channel: u8) -> Result<u32> {
        todo!("T2: scriptable fake backend")
    }

    fn read_pwm(&self, _channel: u8) -> Result<u8> {
        todo!("T2: scriptable fake backend")
    }

    fn write_pwm(&self, _channel: u8, _pwm: u8) -> Result<()> {
        todo!("T2: scriptable fake backend")
    }
}
