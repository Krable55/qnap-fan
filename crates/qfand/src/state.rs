//! Shared daemon state — the handoff boundary between the control loop (T4)
//! and the socket API (T6).
//!
//! The control loop is the only writer. The socket server is a reader that may
//! additionally queue *requests* (curve changes, overrides) for the loop to
//! apply on its next tick. The socket server must never write to the EC and
//! must never call `qfan_core::decide` itself: a second decision-maker would
//! break the single-writer invariant (R1) that the whole design rests on.

use qfan_core::{ChannelConfig, ControlMode, Override};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A snapshot of one PWM channel, published by the control loop after each tick.
#[derive(Debug, Clone)]
pub struct ChannelSnapshot {
    pub channel: &'static str,
    /// Last value the daemon *intended* to write. Change detection compares
    /// against this, never against a sysfs read-back (§2.3).
    pub intended_pwm: u8,
    /// Last value read back from the EC, which rounds to whole percent.
    /// Informational only — never used for change detection.
    pub readback_pwm: Option<u8>,
    pub rpm: Option<u32>,
    pub mode: ControlMode,
    pub override_expires_at: Option<u64>,
}

/// Temperatures published by the control loop.
#[derive(Debug, Clone, Default)]
pub struct TempSnapshot {
    pub cpu_package_c: Option<i16>,
    pub cpu_cores_c: Vec<Option<i16>>,
    /// Keyed by Unraid disk name (`disk1`, `cache`), not `sdX` (§2.4).
    pub disk_temps_c: Vec<(String, Option<i16>)>,
}

/// Everything the socket server is allowed to read.
#[derive(Debug, Clone, Default)]
pub struct Published {
    pub channels: Vec<ChannelSnapshot>,
    pub temps: TempSnapshot,
    pub started_at_secs: u64,
    /// True while running with `--dry-run`, so clients can say so plainly.
    pub dry_run: bool,
    /// Set when the emergency latch is engaged, for display and notification.
    pub emergency: bool,
}

/// A change requested by a client, applied by the control loop on its next tick.
///
/// Validation happens in the control loop, not here: it owns the config and is
/// the only thing that may accept a curve. The socket server's job is to
/// serialise the request and report back what the loop decided.
#[derive(Debug, Clone)]
pub enum Command {
    SetCurve {
        channel: String,
        config: ChannelConfig,
    },
    SetOverride {
        channel: String,
        ov: Override,
    },
    ClearOverride {
        channel: String,
    },
}

/// Shared handle. Cheap to clone; hand one clone to the socket server.
#[derive(Clone, Default)]
pub struct Shared {
    published: Arc<Mutex<Published>>,
    pending: Arc<Mutex<VecDeque<Command>>>,
}

impl Shared {
    pub fn new(dry_run: bool, started_at_secs: u64) -> Self {
        let s = Self::default();
        {
            let mut p = s.published.lock().expect("published lock poisoned");
            p.dry_run = dry_run;
            p.started_at_secs = started_at_secs;
        }
        s
    }

    /// Control loop: publish a fresh snapshot. Keep the critical section tiny —
    /// EC reads take 12–18 ms each (F6) and must happen OUTSIDE the lock.
    pub fn publish(&self, f: impl FnOnce(&mut Published)) {
        let mut p = self.published.lock().expect("published lock poisoned");
        f(&mut p);
    }

    /// Socket server: read the latest snapshot. Never blocks on hardware.
    pub fn snapshot(&self) -> Published {
        self.published
            .lock()
            .expect("published lock poisoned")
            .clone()
    }

    /// Socket server: queue a client request for the control loop.
    pub fn submit(&self, cmd: Command) {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .push_back(cmd);
    }

    /// Control loop: drain queued requests at the top of a tick.
    pub fn drain(&self) -> Vec<Command> {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .drain(..)
            .collect()
    }
}
