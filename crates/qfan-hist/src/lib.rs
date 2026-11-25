//! History storage: round-robin binary files, three-tier aggregation, and
//! CSV export (§6). Implementation deferred to T4.

#![allow(clippy::todo, dead_code)]

use anyhow::Result;
use qfan_core::Series;

// ── File format constants (§6.3) ──────────────────────────────────────────────

pub const MAGIC: &[u8; 4] = b"QFRB";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 128;
pub const SLOT_SIZE: usize = 12;
/// Explicit no-data sentinel in deci-units; see §6.3 for rationale.
pub const NO_DATA: i16 = i16::MIN;

// ── Unit ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Unit {
    DeciCelsius = 0,
    Pwm = 1,
    Rpm = 2,
}

// ── Tier ──────────────────────────────────────────────────────────────────────

/// History storage tier (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// T0: 2 s raw samples, 1 h retention, in-process RAM ring buffer.
    Raw,
    /// T1: 1-min min/avg/max, 14-day retention, appdata on cache pool.
    Minute,
    /// T2: 1-h min/avg/max, 5-year retention, flash at `/boot/config/plugins/qnap-fan/`.
    Hour,
}

impl Tier {
    pub fn interval_s(self) -> u32 {
        match self {
            Tier::Raw => 2,
            Tier::Minute => 60,
            Tier::Hour => 3600,
        }
    }

    pub fn slot_count(self) -> u32 {
        match self {
            Tier::Raw => 1_800,     // 1 h at 2 s
            Tier::Minute => 20_160, // 14 d at 1 min
            Tier::Hour => 43_800,   // 5 y at 1 h (approximate)
        }
    }
}

// ── Slot ──────────────────────────────────────────────────────────────────────

/// One aggregated slot on disk (12 bytes, §6.3).
/// A slot whose `bucket` doesn't match the requested one is a gap, not stale data.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// unix_time / interval_s — identifies this bucket.
    pub bucket: u32,
    /// Minimum value in deci-units; `NO_DATA` sentinel when no samples.
    pub min: i16,
    pub avg: i16,
    pub max: i16,
    /// Number of raw samples in this bucket; enables honest partial-bucket averages.
    pub count: u16,
}

// ── Ring buffer (T0) ──────────────────────────────────────────────────────────

/// In-memory ring buffer for T0 (2 s raw samples, 1 h window, 97 KB for ~27 series).
pub struct RingBuffer {
    series: Series,
    slots: Vec<Option<i16>>,
    head: usize,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(series: Series, capacity: usize) -> Self {
        Self {
            series,
            slots: vec![None; capacity],
            head: 0,
            capacity,
        }
    }

    pub fn series(&self) -> &Series {
        &self.series
    }

    pub fn push(&mut self, _value: i16) {
        todo!("T4: ring buffer write at head, advance head % capacity")
    }

    /// Maximum value over the last `n_slots` slots; None if no data.
    pub fn window_max(&self, _n_slots: usize) -> Option<i16> {
        todo!("T4: scan [head-n .. head] wrapping")
    }
}

// ── Tier file I/O ─────────────────────────────────────────────────────────────

/// Read a slot from a tier file at the computed offset: `128 + (bucket % slot_count) * 12`.
pub fn read_slot(_path: &std::path::Path, _tier: Tier, _bucket: u32) -> Result<Option<Slot>> {
    todo!("T4: open file, seek, read 12 bytes, decode Slot")
}

/// Write a slot to a tier file at the computed offset (in-place, no rewrite).
pub fn write_slot(_path: &std::path::Path, _tier: Tier, _slot: Slot) -> Result<()> {
    todo!("T4: open file, seek, write 12 bytes")
}

// ── Export ────────────────────────────────────────────────────────────────────

/// Wire format for `qfand export` (§6.5).
#[derive(Debug, Clone, Copy)]
pub enum ExportFormat {
    Csv,
}

/// Export a series range to stdout or a writer (§6.5, D2).
pub fn export(
    _series: &Series,
    _tier: Tier,
    _from: u64,
    _to: u64,
    _format: ExportFormat,
    _out: &mut dyn std::io::Write,
) -> Result<()> {
    todo!("T4: iterate slots, format, write")
}
