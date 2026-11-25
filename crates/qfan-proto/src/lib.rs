//! Socket API request/response types (§5).
//! Shared by qfand (daemon side) and qfand tui / qfand status (client side).
//! Transport: newline-delimited JSON over `/var/run/qfand.sock`.

use qfan_core::CurvePoint;
use serde::{Deserialize, Serialize};

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Current channel status, temps, RPMs, mode, uptime.
    Status,
    /// Historical slot array for a series/tier/range.
    History {
        series: String,
        tier: String,
        from: u64,
        to: u64,
    },
    /// Current effective config.
    Config,
    /// Validate and apply a new fan curve (rejected if unsafe, §5).
    SetCurve {
        channel: String,
        points: Vec<CurvePoint>,
    },
    /// Fixed-PWM override with mandatory expiry (§4.4; hard cap 1 h).
    Override {
        channel: String,
        pwm: u8,
        /// Seconds until the override expires and curve control resumes.
        ttl_s: u32,
    },
    /// Cancel an active manual override.
    ClearOverride { channel: String },
    /// Recent daemon event log entries since a Unix timestamp.
    Events { since: u64 },
}

// ── Responses ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelStatus {
    pub channel: String,
    /// Last value the daemon intended to write (§2.3: never compare to read-back).
    pub intended_pwm: u8,
    /// Last value read back from the EC (rounded to whole percent, §2.3).
    pub readback_pwm: Option<u8>,
    pub rpm: Option<u32>,
    pub mode: String,
    pub override_expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempSnapshot {
    pub cpu_package_c: Option<f32>,
    pub cpu_cores_c: Vec<Option<f32>>,
    pub disk_temps_c: Vec<(String, Option<f32>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub channels: Vec<ChannelStatus>,
    pub temps: TempSnapshot,
    pub uptime_s: u64,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySlot {
    pub bucket: u32,
    pub min: i16,
    pub avg: i16,
    pub max: i16,
    pub count: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub series: String,
    pub tier: String,
    pub slots: Vec<HistorySlot>,
}

/// Generic success/error envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Ack {
    Ok,
    Error { message: String },
}

/// Top-level response enum covering all op types.
///
/// Untagged: serde decodes into the FIRST variant that fits. `Config` holds a
/// bare `Value` and fits anything, so it must stay last.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Status(StatusResponse),
    History(HistoryResponse),
    Ack(Ack),
    Config(serde_json::Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Response` is untagged, so serde takes the first variant that fits.
    /// `Config(Value)` fits any JSON at all; while it was listed before `Ack`,
    /// every ack decoded as `Config` and clients matching `Response::Ack`
    /// never saw an accept or a rejection.
    #[test]
    fn acks_decode_as_acks() {
        let ok: Response = serde_json::from_str(r#"{"result":"ok"}"#).unwrap();
        assert!(matches!(ok, Response::Ack(Ack::Ok)), "{ok:?}");

        let err: Response = serde_json::from_str(r#"{"result":"error","message":"nope"}"#).unwrap();
        assert!(
            matches!(&err, Response::Ack(Ack::Error { message }) if message == "nope"),
            "{err:?}"
        );
    }

    #[test]
    fn config_still_decodes_as_config() {
        let v: Response = serde_json::from_str(r#"{"pwm7":{"breakpoints":[]}}"#).unwrap();
        assert!(matches!(v, Response::Config(_)), "{v:?}");
    }
}
