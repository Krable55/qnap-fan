//! Hardware abstraction layer: EC backend trait, sysfs and fake implementations,
//! and hwmon discovery (§2.1, §10).
//!
//! # Trait object-safety
//! [`EcBackend`] is **object-safe**. The daemon holds `Box<dyn EcBackend>` and
//! swaps [`SysfsBackend`] for [`FakeBackend`] in tests.
//!
//! # Temperature units
//! sysfs gives millidegrees Celsius. This crate divides by 100 and returns
//! [`DeciUnits`] (tenths of °C): `DeciUnits(500)` = 50.0 °C.
//! The history format (§6.3) also uses deci-units, so no second conversion is needed.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use qfan_core::DeciUnits;
use thiserror::Error;

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum HalError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("hwmon '{name}' not found in sysfs")]
    HwmonNotFound { name: &'static str },
    #[error("parse error for {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: std::num::ParseIntError,
    },
    /// A sysfs read returned ENOENT — the hwmonN index moved between boots (§2.1).
    /// The caller must call [`HwmonPaths::discover()`] again and retry.
    #[error("hwmon path disappeared (ENOENT); call HwmonPaths::discover() again")]
    HwmonMoved,
    #[error("injected test failure: {0}")]
    Injected(String),
}

fn io_to_hal(path: impl Into<String>, source: io::Error) -> HalError {
    if source.kind() == io::ErrorKind::NotFound {
        HalError::HwmonMoved
    } else {
        HalError::Io {
            path: path.into(),
            source,
        }
    }
}

// ── Channel types (R6: pwm21 and fan22-35 excluded by design) ────────────────

/// PWM output channels owned by the daemon.
/// `pwm21` is **excluded by design** (§2.2, R6): it cannot be named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PwmChannel {
    /// System fans → `fan1..4`. sysfs file: `pwm1`.
    Pwm1,
    /// CPU fans → `fan7`, `fan8`. sysfs file: `pwm7`.
    Pwm7,
}

impl PwmChannel {
    pub fn sysfs_name(self) -> &'static str {
        match self {
            PwmChannel::Pwm1 => "pwm1",
            PwmChannel::Pwm7 => "pwm7",
        }
    }
}

/// Fan RPM input channels. `fan22..35` are **excluded by design** (§2.2, R6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FanChannel {
    Fan1,
    Fan2,
    Fan3,
    Fan4,
    Fan7,
    Fan8,
}

impl FanChannel {
    pub fn sysfs_name(self) -> &'static str {
        match self {
            FanChannel::Fan1 => "fan1_input",
            FanChannel::Fan2 => "fan2_input",
            FanChannel::Fan3 => "fan3_input",
            FanChannel::Fan4 => "fan4_input",
            FanChannel::Fan7 => "fan7_input",
            FanChannel::Fan8 => "fan8_input",
        }
    }
}

/// Temperature sensors available from coretemp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TempSensor {
    /// CPU package temperature. sysfs file: `temp1_input`.
    Package,
    /// Per-core temperature, 0..=5. `Core(0)` = `temp2_input`, ..., `Core(5)` = `temp7_input`.
    Core(u8),
}

impl TempSensor {
    /// sysfs filename for this sensor.
    ///
    /// # Panics
    /// Panics if `Core(n)` with n > 5.
    pub fn sysfs_name(self) -> &'static str {
        const CORE_NAMES: [&str; 6] = [
            "temp2_input",
            "temp3_input",
            "temp4_input",
            "temp5_input",
            "temp6_input",
            "temp7_input",
        ];
        match self {
            TempSensor::Package => "temp1_input",
            TempSensor::Core(n @ 0..=5) => CORE_NAMES[n as usize],
            TempSensor::Core(n) => panic!("Core index {n} out of range (valid: 0..=5)"),
        }
    }
}

// ── PWM rounding (§2.3) ──────────────────────────────────────────────────────
// ── EC PWM rounding ───────────────────────────────────────────────────────────
//
// The rounding model and the delivered-duty floor live in qfan-core as pure
// arithmetic, so the curve validator can enforce the floor in DELIVERED units
// without qfan-hal depending on it (that would invert the layering). They are
// re-exported here because this is the crate that actually talks to the EC.
pub use qfan_core::{
    ec_pwm_min_write_for, ec_pwm_round, DELIVERED_DUTY_FLOOR, EC_DUTY_FLOOR_35PCT,
};

// ── Backend trait (object-safe) ───────────────────────────────────────────────

/// Hardware access abstraction for the QNAP EC and coretemp hwmon.
///
/// **Object-safe**: no generics, no associated types, all `&self` receivers.
/// The daemon holds `Box<dyn EcBackend>`. Use [`SysfsBackend`] in production
/// and [`FakeBackend`] in tests.
///
/// Each EC read/write costs ~12–18 ms (§2.2). Coretemp reads cost ~0.5 ms.
pub trait EcBackend: Send + Sync {
    /// Read a temperature in deci-°C (tenths). sysfs millidegrees ÷ 100.
    fn read_temp(&self, sensor: TempSensor) -> Result<DeciUnits, HalError>;
    /// Read fan RPM. EC operation — costs ~18 ms on hardware.
    fn read_fan(&self, channel: FanChannel) -> Result<u32, HalError>;
    /// Read current PWM. Read-back is lossy (§2.3); compare against
    /// the daemon's last-intended value, not this result.
    fn read_pwm(&self, channel: PwmChannel) -> Result<u8, HalError>;
    /// Write a PWM value. Call only when the intended value changed (§2.2, §2.3).
    fn write_pwm(&self, channel: PwmChannel, pwm: u8) -> Result<(), HalError>;
}

// ── hwmon discovery (§2.1) ───────────────────────────────────────────────────

/// Resolved hwmon directories. The hwmonN index is not stable across boots (§2.1).
/// Re-discover whenever a sysfs read returns [`HalError::HwmonMoved`].
pub struct HwmonPaths {
    /// Path to the `qnap_ec` hwmon directory (writable; each access costs ~12–18 ms).
    pub qnap_ec: PathBuf,
    /// Path to the `coretemp` hwmon directory (read-only; costs ~0.5 ms).
    pub coretemp: PathBuf,
}

impl HwmonPaths {
    /// Scan `/sys/class/hwmon/*/name` for `qnap_ec` and `coretemp`.
    /// Re-call if any read later returns [`HalError::HwmonMoved`].
    pub fn discover() -> Result<Self, HalError> {
        Self::discover_under("/sys/class/hwmon")
    }

    /// Like [`discover`][Self::discover] but scans under `hwmon_root`.
    /// Inject a tmpdir in unit tests for coverage without real hardware.
    pub fn discover_under(hwmon_root: impl AsRef<Path>) -> Result<Self, HalError> {
        let root = hwmon_root.as_ref();
        let mut qnap_ec: Option<PathBuf> = None;
        let mut coretemp: Option<PathBuf> = None;

        for entry in std::fs::read_dir(root).map_err(|e| HalError::Io {
            path: root.display().to_string(),
            source: e,
        })? {
            let entry = entry.map_err(|e| HalError::Io {
                path: root.display().to_string(),
                source: e,
            })?;
            let name_file = entry.path().join("name");
            let raw = match std::fs::read_to_string(&name_file) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(HalError::Io {
                        path: name_file.display().to_string(),
                        source: e,
                    })
                }
            };
            match raw.trim() {
                "qnap_ec" => qnap_ec = Some(entry.path()),
                "coretemp" => coretemp = Some(entry.path()),
                _ => {}
            }
        }

        Ok(HwmonPaths {
            qnap_ec: qnap_ec.ok_or(HalError::HwmonNotFound { name: "qnap_ec" })?,
            coretemp: coretemp.ok_or(HalError::HwmonNotFound { name: "coretemp" })?,
        })
    }
}

// ── SysfsBackend ──────────────────────────────────────────────────────────────

/// Real hardware backend. Each EC access costs ~12–18 ms (§2.2).
pub struct SysfsBackend {
    paths: HwmonPaths,
}

impl SysfsBackend {
    pub fn new(paths: HwmonPaths) -> Self {
        Self { paths }
    }

    fn read_u32(&self, path: &Path) -> Result<u32, HalError> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| io_to_hal(path.display().to_string(), e))?;
        raw.trim().parse().map_err(|e| HalError::Parse {
            path: path.display().to_string(),
            source: e,
        })
    }
}

impl EcBackend for SysfsBackend {
    fn read_temp(&self, sensor: TempSensor) -> Result<DeciUnits, HalError> {
        let path = self.paths.coretemp.join(sensor.sysfs_name());
        // sysfs gives millidegrees; divide by 100 → deci-°C.
        // e.g. 50000 mdeg → DeciUnits(500) = 50.0 °C.
        let milli = self.read_u32(&path)?;
        Ok(DeciUnits((milli / 100) as i16))
    }

    fn read_fan(&self, channel: FanChannel) -> Result<u32, HalError> {
        let path = self.paths.qnap_ec.join(channel.sysfs_name());
        self.read_u32(&path)
    }

    fn read_pwm(&self, channel: PwmChannel) -> Result<u8, HalError> {
        let path = self.paths.qnap_ec.join(channel.sysfs_name());
        let val = self.read_u32(&path)?;
        Ok(val.min(255) as u8)
    }

    fn write_pwm(&self, channel: PwmChannel, pwm: u8) -> Result<(), HalError> {
        let path = self.paths.qnap_ec.join(channel.sysfs_name());
        std::fs::write(&path, format!("{}\n", pwm))
            .map_err(|e| io_to_hal(path.display().to_string(), e))
    }
}

// ── FakeBackend ───────────────────────────────────────────────────────────────

/// Measured EC latencies from DESIGN.md §2.2.
const EC_PWM_LATENCY: Duration = Duration::from_millis(12);
const EC_FAN_LATENCY: Duration = Duration::from_millis(18);
const CORETEMP_LATENCY: Duration = Duration::from_micros(500);

/// Scriptable test backend (§10).
///
/// ## Latency handling
///
/// `latency_scale` multiplies the base §2.2 latencies before sleeping:
/// - `0.0` (default): **no sleep** — tests run fast. The `simulated_latency()`
///   accumulator still records what the latency would be at scale 1.0, so tests
///   can assert the EC cost budget without sleeping.
/// - `1.0`: real 12–18 ms EC sleeps, 0.5 ms coretemp. Use this in R3a to
///   verify the measured latency band. Only one test should set this; all others
///   use the default.
///
/// ## Thermal model
///
/// `temp = thermal_base − (pwm7 / 255) × thermal_range`
///
/// Default: 70.0 °C at PWM 0, 40.0 °C at PWM 255. As the control loop raises
/// pwm7, temperature falls, so the loop converges in tests. Override with
/// [`set_temp`][FakeBackend::set_temp] for precise control.
pub struct FakeBackend {
    inner: Mutex<FakeInner>,
    /// Latency multiplier. 0.0 = no sleep (default). 1.0 = real hardware delays.
    pub latency_scale: f64,
}

struct FakeInner {
    temp_overrides: HashMap<TempSensor, DeciUnits>,
    thermal_base: DeciUnits,
    thermal_range: DeciUnits,

    pwm: HashMap<PwmChannel, u8>,
    fan: HashMap<FanChannel, u32>,

    fail_read_temp: Option<String>,
    fail_read_fan: Option<String>,
    fail_read_pwm: Option<String>,
    fail_write_pwm: Option<String>,

    temp_reads: HashMap<TempSensor, u64>,
    fan_reads: HashMap<FanChannel, u64>,
    pwm_reads: HashMap<PwmChannel, u64>,
    pwm_writes: HashMap<PwmChannel, u64>,

    /// Accumulated simulated latency at scale=1.0, regardless of actual sleep.
    simulated_latency: Duration,
}

impl FakeInner {
    fn new() -> Self {
        Self {
            temp_overrides: HashMap::new(),
            thermal_base: DeciUnits(700),  // 70.0 °C at PWM 0
            thermal_range: DeciUnits(300), // 30.0 °C drop at PWM 255
            pwm: HashMap::from([
                (PwmChannel::Pwm1, ec_pwm_round(128)),
                (PwmChannel::Pwm7, ec_pwm_round(128)),
            ]),
            fan: HashMap::from([
                (FanChannel::Fan1, 1520),
                (FanChannel::Fan2, 1520),
                (FanChannel::Fan3, 1520),
                (FanChannel::Fan4, 1520),
                (FanChannel::Fan7, 1200),
                (FanChannel::Fan8, 1200),
            ]),
            fail_read_temp: None,
            fail_read_fan: None,
            fail_read_pwm: None,
            fail_write_pwm: None,
            temp_reads: HashMap::new(),
            fan_reads: HashMap::new(),
            pwm_reads: HashMap::new(),
            pwm_writes: HashMap::new(),
            simulated_latency: Duration::ZERO,
        }
    }

    fn cpu_temp(&self) -> DeciUnits {
        let pwm7 = *self.pwm.get(&PwmChannel::Pwm7).unwrap_or(&128) as i32;
        let base = self.thermal_base.0 as i32;
        let range = self.thermal_range.0 as i32;
        DeciUnits((base - range * pwm7 / 255) as i16)
    }
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeInner::new()),
            latency_scale: 0.0,
        }
    }

    /// Builder: set the latency scale (0.0 = no sleep, 1.0 = real delays).
    pub fn with_latency_scale(mut self, scale: f64) -> Self {
        self.latency_scale = scale;
        self
    }

    /// Fix a temperature, overriding the thermal model for `sensor`.
    pub fn set_temp(&self, sensor: TempSensor, val: DeciUnits) {
        self.inner
            .lock()
            .unwrap()
            .temp_overrides
            .insert(sensor, val);
    }

    /// Configure the thermal model (base at PWM 0, total drop at PWM 255), in deci-°C.
    pub fn set_thermal_model(&self, base: DeciUnits, range: DeciUnits) {
        let mut g = self.inner.lock().unwrap();
        g.thermal_base = base;
        g.thermal_range = range;
    }

    pub fn set_fan_rpm(&self, channel: FanChannel, rpm: u32) {
        self.inner.lock().unwrap().fan.insert(channel, rpm);
    }

    /// Inject a failure on `read_temp`. Pass `None` to clear.
    pub fn inject_read_temp_failure(&self, msg: Option<&str>) {
        self.inner.lock().unwrap().fail_read_temp = msg.map(str::to_owned);
    }
    pub fn inject_read_fan_failure(&self, msg: Option<&str>) {
        self.inner.lock().unwrap().fail_read_fan = msg.map(str::to_owned);
    }
    pub fn inject_read_pwm_failure(&self, msg: Option<&str>) {
        self.inner.lock().unwrap().fail_read_pwm = msg.map(str::to_owned);
    }
    pub fn inject_write_pwm_failure(&self, msg: Option<&str>) {
        self.inner.lock().unwrap().fail_write_pwm = msg.map(str::to_owned);
    }

    /// Snapshot of per-operation call counters.
    pub fn call_counts(&self) -> CallCounts {
        let g = self.inner.lock().unwrap();
        CallCounts {
            temp_reads: g.temp_reads.clone(),
            fan_reads: g.fan_reads.clone(),
            pwm_reads: g.pwm_reads.clone(),
            pwm_writes: g.pwm_writes.clone(),
        }
    }

    /// Total latency accumulated at scale=1.0 across all calls, regardless of
    /// whether actual sleeping occurred. Use to assert EC access budgets in tests.
    pub fn simulated_latency(&self) -> Duration {
        self.inner.lock().unwrap().simulated_latency
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulate base latency and optionally sleep, without holding the lock during sleep.
fn simulate_latency(inner: &Mutex<FakeInner>, base: Duration, scale: f64) {
    inner.lock().unwrap().simulated_latency += base;
    if scale > 0.0 {
        std::thread::sleep(base.mul_f64(scale));
    }
}

impl EcBackend for FakeBackend {
    fn read_temp(&self, sensor: TempSensor) -> Result<DeciUnits, HalError> {
        simulate_latency(&self.inner, CORETEMP_LATENCY, self.latency_scale);
        let mut g = self.inner.lock().unwrap();
        *g.temp_reads.entry(sensor).or_insert(0) += 1;
        if let Some(msg) = &g.fail_read_temp {
            return Err(HalError::Injected(msg.clone()));
        }
        if let Some(&v) = g.temp_overrides.get(&sensor) {
            return Ok(v);
        }
        Ok(g.cpu_temp())
    }

    fn read_fan(&self, channel: FanChannel) -> Result<u32, HalError> {
        simulate_latency(&self.inner, EC_FAN_LATENCY, self.latency_scale);
        let mut g = self.inner.lock().unwrap();
        *g.fan_reads.entry(channel).or_insert(0) += 1;
        if let Some(msg) = &g.fail_read_fan {
            return Err(HalError::Injected(msg.clone()));
        }
        Ok(*g.fan.get(&channel).unwrap_or(&0))
    }

    fn read_pwm(&self, channel: PwmChannel) -> Result<u8, HalError> {
        simulate_latency(&self.inner, EC_PWM_LATENCY, self.latency_scale);
        let mut g = self.inner.lock().unwrap();
        *g.pwm_reads.entry(channel).or_insert(0) += 1;
        if let Some(msg) = &g.fail_read_pwm {
            return Err(HalError::Injected(msg.clone()));
        }
        Ok(*g.pwm.get(&channel).unwrap_or(&0))
    }

    fn write_pwm(&self, channel: PwmChannel, pwm: u8) -> Result<(), HalError> {
        simulate_latency(&self.inner, EC_PWM_LATENCY, self.latency_scale);
        let mut g = self.inner.lock().unwrap();
        *g.pwm_writes.entry(channel).or_insert(0) += 1;
        if let Some(msg) = &g.fail_write_pwm {
            return Err(HalError::Injected(msg.clone()));
        }
        g.pwm.insert(channel, ec_pwm_round(pwm));
        Ok(())
    }
}

/// Per-operation call counters exposed by [`FakeBackend`].
#[derive(Debug, Default, Clone)]
pub struct CallCounts {
    pub temp_reads: HashMap<TempSensor, u64>,
    pub fan_reads: HashMap<FanChannel, u64>,
    pub pwm_reads: HashMap<PwmChannel, u64>,
    pub pwm_writes: HashMap<PwmChannel, u64>,
}

impl CallCounts {
    /// Total EC accesses (fan reads + pwm reads + pwm writes; coretemp excluded).
    /// Use to prove R3: the daemon does not over-access the EC.
    pub fn total_ec(&self) -> u64 {
        self.fan_reads.values().sum::<u64>()
            + self.pwm_reads.values().sum::<u64>()
            + self.pwm_writes.values().sum::<u64>()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Minimal TempDir without the tempfile crate (avoids rustix→-liconv on macOS).
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "qfan-hal-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── PWM rounding ──────────────────────────────────────────────────────────

    #[test]
    fn ec_pwm_round_verified_values() {
        // Values verified on live hardware, DESIGN.md §2.3.
        assert_eq!(ec_pwm_round(180), 178, "write 180 → read back 178");
        assert_eq!(ec_pwm_round(73), 71, "write 73 → read back 71");
        assert_eq!(ec_pwm_round(0), 0);
        assert_eq!(ec_pwm_round(255), 255);
    }

    // The 35%-duty safety gap: qfan-core validates pwm >= 89 on the INTENDED value,
    // but the EC delivers ec_pwm_round(89) == 86 (33% duty). A config that passes
    // validation can still violate the safety floor. EC_DUTY_FLOOR_35PCT is the
    // smallest write value that guarantees delivery of >= 89.
    #[test]
    fn floor_gap_round_89_delivers_below_89() {
        assert!(
            ec_pwm_round(89) < 89,
            "ec_pwm_round(89) == {} — the qfan-core floor of 89 does not survive rounding",
            ec_pwm_round(89)
        );
    }

    #[test]
    fn ec_pwm_min_write_for_delivers_floor() {
        let w = ec_pwm_min_write_for(89);
        assert!(
            ec_pwm_round(w) >= 89,
            "ec_pwm_min_write_for(89) = {w} but ec_pwm_round({w}) = {} < 89",
            ec_pwm_round(w)
        );
        // Minimal: one less fails to deliver the floor.
        if w > 0 {
            assert!(
                ec_pwm_round(w - 1) < 89,
                "ec_pwm_min_write_for(89) = {w} is not minimal: round({}) = {} >= 89",
                w - 1,
                ec_pwm_round(w - 1)
            );
        }
    }

    #[test]
    fn ec_duty_floor_35pct_constant() {
        // EC_DUTY_FLOOR_35PCT must equal ec_pwm_min_write_for(89).
        // Computed value is 90, not 92: round(90)=89 satisfies the floor.
        // The reviewer's probe showed intended-90→delivered-89 correctly;
        // their "92" was an off-by-one in which write they picked as minimum.
        assert_eq!(EC_DUTY_FLOOR_35PCT, ec_pwm_min_write_for(89));
        assert_eq!(EC_DUTY_FLOOR_35PCT, 90, "minimum write that delivers >=89");
    }

    // R9b: FakeBackend reproduces the write-180 → read-178 rounding.
    #[test]
    fn fake_backend_pwm_rounding_r9b() {
        let be = FakeBackend::new();
        be.write_pwm(PwmChannel::Pwm7, 180).unwrap();
        assert_eq!(
            be.read_pwm(PwmChannel::Pwm7).unwrap(),
            178,
            "FakeBackend must reproduce EC rounding (write 180 → read 178)"
        );
    }

    // ── Call counters (R9c) ───────────────────────────────────────────────────

    #[test]
    fn fake_backend_call_counter_r9c() {
        let be = FakeBackend::new();
        be.read_fan(FanChannel::Fan7).unwrap();
        be.read_fan(FanChannel::Fan7).unwrap();
        be.read_fan(FanChannel::Fan1).unwrap();
        be.write_pwm(PwmChannel::Pwm7, 200).unwrap();

        let counts = be.call_counts();
        assert_eq!(counts.fan_reads.get(&FanChannel::Fan7), Some(&2));
        assert_eq!(counts.fan_reads.get(&FanChannel::Fan1), Some(&1));
        assert_eq!(counts.pwm_writes.get(&PwmChannel::Pwm7), Some(&1));
        assert_eq!(counts.total_ec(), 4);
    }

    // ── Injected failures (R9c) ───────────────────────────────────────────────

    #[test]
    fn fake_backend_injected_read_failure_r9c() {
        let be = FakeBackend::new();
        be.inject_read_fan_failure(Some("simulated sensor loss"));
        let err = be.read_fan(FanChannel::Fan7).unwrap_err();
        assert!(err.to_string().contains("simulated sensor loss"));
    }

    #[test]
    fn fake_backend_injected_write_failure() {
        let be = FakeBackend::new();
        be.inject_write_pwm_failure(Some("write blocked"));
        assert!(be.write_pwm(PwmChannel::Pwm7, 200).is_err());
    }

    #[test]
    fn fake_backend_injected_read_temp_failure() {
        let be = FakeBackend::new();
        be.inject_read_temp_failure(Some("bad sensor"));
        assert!(be.read_temp(TempSensor::Package).is_err());
        // Clear the failure — subsequent read must succeed.
        be.inject_read_temp_failure(None);
        assert!(be.read_temp(TempSensor::Package).is_ok());
    }

    // ── Thermal model ─────────────────────────────────────────────────────────

    #[test]
    fn fake_backend_thermal_model_converges() {
        // Higher PWM → lower temperature — proves a control loop can converge.
        let be = FakeBackend::new(); // base=70°C, range=30°C
        be.write_pwm(PwmChannel::Pwm7, 0).unwrap();
        let temp_lo = be.read_temp(TempSensor::Package).unwrap();
        be.write_pwm(PwmChannel::Pwm7, 255).unwrap();
        let temp_hi = be.read_temp(TempSensor::Package).unwrap();
        assert!(
            temp_hi < temp_lo,
            "higher PWM must produce lower temperature ({temp_hi:?} < {temp_lo:?})"
        );
    }

    #[test]
    fn fake_backend_temp_override() {
        let be = FakeBackend::new();
        be.set_temp(TempSensor::Package, DeciUnits(500));
        assert_eq!(be.read_temp(TempSensor::Package).unwrap(), DeciUnits(500));
    }

    // ── Simulated latency budget ───────────────────────────────────────────────

    #[test]
    fn simulated_latency_accumulates_at_scale_zero() {
        // scale=0 means no actual sleep, but accumulator still runs.
        let be = FakeBackend::new(); // scale=0.0
        be.read_fan(FanChannel::Fan7).unwrap(); // +18 ms
        be.read_pwm(PwmChannel::Pwm1).unwrap(); // +12 ms
        assert_eq!(be.simulated_latency(), Duration::from_millis(30));
    }

    // R3a: single EC fan read with scale=1.0 is in the §2.2 measured band.
    // This test sleeps ~18 ms intentionally to prove the latency model.
    #[test]
    fn ec_latency_realtime_r3a() {
        let be = FakeBackend::new().with_latency_scale(1.0);
        let t0 = Instant::now();
        be.read_fan(FanChannel::Fan7).unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(12),
            "EC fan read must sleep ≥ 12 ms (§2.2 lower bound); got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "EC fan read sanity upper bound; got {elapsed:?}"
        );
    }

    // ── hwmon discovery ──────────────────────────────────────────────────────

    #[test]
    fn hwmon_discover_finds_named_dirs() {
        let dir = TempDir::new();
        let root = dir.path();
        for (hwmon, name) in [
            ("hwmon0", "acpitz"),
            ("hwmon3", "qnap_ec"),
            ("hwmon5", "coretemp"),
        ] {
            let d = root.join(hwmon);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("name"), name).unwrap();
        }
        let paths = HwmonPaths::discover_under(root).unwrap();
        assert_eq!(paths.qnap_ec, root.join("hwmon3"));
        assert_eq!(paths.coretemp, root.join("hwmon5"));
    }

    #[test]
    fn hwmon_discover_missing_qnap_ec_returns_error() {
        let dir = TempDir::new();
        let d = dir.path().join("hwmon0");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), "coretemp").unwrap();
        assert!(matches!(
            HwmonPaths::discover_under(dir.path()),
            Err(HalError::HwmonNotFound { name: "qnap_ec" })
        ));
    }

    // ── SysfsBackend with fake sysfs files ───────────────────────────────────

    fn make_fake_sysfs() -> (TempDir, HwmonPaths) {
        let dir = TempDir::new();
        let root = dir.path();
        let ec = root.join("hwmon3");
        let ct = root.join("hwmon5");
        std::fs::create_dir_all(&ec).unwrap();
        std::fs::create_dir_all(&ct).unwrap();

        std::fs::write(ec.join("name"), "qnap_ec").unwrap();
        std::fs::write(ec.join("pwm1"), "128\n").unwrap();
        std::fs::write(ec.join("pwm7"), "200\n").unwrap();
        std::fs::write(ec.join("fan1_input"), "1520\n").unwrap();
        std::fs::write(ec.join("fan7_input"), "1248\n").unwrap();

        std::fs::write(ct.join("name"), "coretemp").unwrap();
        std::fs::write(ct.join("temp1_input"), "50000\n").unwrap(); // 50.0 °C
        std::fs::write(ct.join("temp2_input"), "48000\n").unwrap(); // 48.0 °C

        (
            dir,
            HwmonPaths {
                qnap_ec: ec,
                coretemp: ct,
            },
        )
    }

    #[test]
    fn sysfs_read_temp_converts_millidegrees() {
        let (_dir, paths) = make_fake_sysfs();
        let be = SysfsBackend::new(paths);
        // 50000 mdeg / 100 = DeciUnits(500) = 50.0 °C
        assert_eq!(be.read_temp(TempSensor::Package).unwrap(), DeciUnits(500));
        // 48000 / 100 = DeciUnits(480) = 48.0 °C
        assert_eq!(be.read_temp(TempSensor::Core(0)).unwrap(), DeciUnits(480));
    }

    #[test]
    fn sysfs_read_fan_rpm() {
        let (_dir, paths) = make_fake_sysfs();
        let be = SysfsBackend::new(paths);
        assert_eq!(be.read_fan(FanChannel::Fan7).unwrap(), 1248);
    }

    #[test]
    fn sysfs_read_pwm() {
        let (_dir, paths) = make_fake_sysfs();
        let be = SysfsBackend::new(paths);
        assert_eq!(be.read_pwm(PwmChannel::Pwm7).unwrap(), 200);
    }

    #[test]
    fn sysfs_write_pwm() {
        let (_dir, paths) = make_fake_sysfs();
        let pwm7_path = paths.qnap_ec.join("pwm7");
        let be = SysfsBackend::new(paths);
        be.write_pwm(PwmChannel::Pwm7, 180).unwrap();
        assert_eq!(std::fs::read_to_string(&pwm7_path).unwrap().trim(), "180");
    }

    #[test]
    fn sysfs_enoent_returns_hwmon_moved() {
        let (_dir, paths) = make_fake_sysfs();
        let temp1 = paths.coretemp.join("temp1_input");
        let be = SysfsBackend::new(paths);
        std::fs::remove_file(&temp1).unwrap();
        assert!(matches!(
            be.read_temp(TempSensor::Package),
            Err(HalError::HwmonMoved)
        ));
    }
}
