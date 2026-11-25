//! History storage: round-robin binary files, three-tier aggregation, and
//! CSV export (§6).
//!
//! All multi-byte integer fields are little-endian throughout — header and slots.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use qfan_core::Series;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const MAGIC: &[u8; 4] = b"QFRB";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 128;
pub const SLOT_SIZE: usize = 12;
/// Explicit no-data sentinel (§6.3). Without this, downtime shows as 0°C.
pub const NO_DATA: i16 = i16::MIN;

// ── Unit ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Unit {
    DeciCelsius = 0,
    Pwm = 1,
    Rpm = 2,
}

impl Unit {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Unit::DeciCelsius),
            1 => Some(Unit::Pwm),
            2 => Some(Unit::Rpm),
            _ => None,
        }
    }
}

// ── Tier ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// T0: 2 s raw samples, 1 h retention, in-process RAM ring.
    Raw,
    /// T1: 1-min min/avg/max, 14-day retention, appdata.
    Minute,
    /// T2: 1-h min/avg/max, 5-year retention, flash at /boot.
    Hour,
}

impl Tier {
    pub fn interval_s(self) -> u32 {
        match self {
            Tier::Raw => 2,
            Tier::Minute => 60,
            Tier::Hour => 3_600,
        }
    }

    fn retention_s(self) -> u32 {
        match self {
            Tier::Raw => 3_600,                 // 1 h
            Tier::Minute => 14 * 24 * 3_600,    // 14 days
            Tier::Hour => 5 * 365 * 24 * 3_600, // 5 years (approximate, ignoring leap years)
        }
    }

    /// Slot count derived from retention / interval — never hardcoded.
    pub fn slot_count(self) -> u32 {
        self.retention_s() / self.interval_s()
    }
}

// ── Slot ──────────────────────────────────────────────────────────────────────

/// One aggregated slot on disk or in a file (12 bytes, §6.3).
/// A slot whose `bucket` doesn't match the requested one is a gap, not stale data.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// unix_time / interval_s — identifies this bucket.
    pub bucket: u32,
    /// Minimum value in deci-units; `NO_DATA` sentinel when no samples.
    pub min: i16,
    pub avg: i16,
    pub max: i16,
    /// Number of raw samples; count=0 means no data, never interpreted as 0°C.
    pub count: u16,
}

impl Slot {
    fn encode(self) -> [u8; SLOT_SIZE] {
        let mut b = [0u8; SLOT_SIZE];
        b[0..4].copy_from_slice(&self.bucket.to_le_bytes());
        b[4..6].copy_from_slice(&self.min.to_le_bytes());
        b[6..8].copy_from_slice(&self.avg.to_le_bytes());
        b[8..10].copy_from_slice(&self.max.to_le_bytes());
        b[10..12].copy_from_slice(&self.count.to_le_bytes());
        b
    }

    fn decode(b: &[u8; SLOT_SIZE]) -> Self {
        Self {
            bucket: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            min: i16::from_le_bytes(b[4..6].try_into().unwrap()),
            avg: i16::from_le_bytes(b[6..8].try_into().unwrap()),
            max: i16::from_le_bytes(b[8..10].try_into().unwrap()),
            count: u16::from_le_bytes(b[10..12].try_into().unwrap()),
        }
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

struct Header {
    interval_s: u32,
    slot_count: u32,
    unit: Unit,
    series_name: [u8; 64],
    created_unix: i64,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_SIZE] {
        // Layout (all little-endian):
        //   0..4   "QFRB" magic
        //   4..6   format_version u16
        //   6..8   reserved u16 (zero)
        //   8..12  interval_s u32
        //   12..16 slot_count u32
        //   16     unit u8
        //   17..81 series_name [u8; 64], NUL-padded
        //   81..89 created_unix i64
        //   89..128 reserved (zero)
        let mut b = [0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(MAGIC);
        b[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&self.interval_s.to_le_bytes());
        b[12..16].copy_from_slice(&self.slot_count.to_le_bytes());
        b[16] = self.unit as u8;
        b[17..81].copy_from_slice(&self.series_name);
        b[81..89].copy_from_slice(&self.created_unix.to_le_bytes());
        b
    }

    fn decode(b: &[u8; HEADER_SIZE]) -> Result<Self> {
        if &b[0..4] != MAGIC {
            bail!("bad magic: expected QFRB, got {:?}", &b[0..4]);
        }
        let version = u16::from_le_bytes(b[4..6].try_into().unwrap());
        if version != FORMAT_VERSION {
            bail!("unsupported format version {version} (expected {FORMAT_VERSION})");
        }
        let interval_s = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let slot_count = u32::from_le_bytes(b[12..16].try_into().unwrap());
        let unit_byte = b[16];
        let unit = Unit::from_u8(unit_byte)
            .ok_or_else(|| anyhow::anyhow!("unknown unit byte {unit_byte}"))?;
        let mut series_name = [0u8; 64];
        series_name.copy_from_slice(&b[17..81]);
        let created_unix = i64::from_le_bytes(b[81..89].try_into().unwrap());
        Ok(Header {
            interval_s,
            slot_count,
            unit,
            series_name,
            created_unix,
        })
    }

    #[cfg(test)]
    fn series_name_str(&self) -> &str {
        let nul = self.series_name.iter().position(|&b| b == 0).unwrap_or(64);
        std::str::from_utf8(&self.series_name[..nul]).unwrap_or("")
    }
}

// ── In-memory ring buffer (T0) ────────────────────────────────────────────────

/// In-memory ring buffer for T0 (2 s raw samples, 1 h window).
/// Fixed allocation at construction; `push` overwrites in place, no per-sample alloc.
pub struct RingBuffer {
    series: Series,
    slots: Vec<i16>,
    head: usize,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(series: Series, capacity: usize) -> Self {
        Self {
            series,
            slots: vec![NO_DATA; capacity],
            head: 0,
            capacity,
        }
    }

    pub fn series(&self) -> &Series {
        &self.series
    }

    /// Overwrite the oldest slot with `value` and advance head.
    pub fn push(&mut self, value: i16) {
        self.slots[self.head] = value;
        self.head = (self.head + 1) % self.capacity;
    }

    /// Maximum value over the last `n_slots` slots; `None` if no data at all.
    pub fn window_max(&self, n_slots: usize) -> Option<i16> {
        let n = n_slots.min(self.capacity);
        let mut max: Option<i16> = None;
        for i in 0..n {
            // Walk backwards from head-1 (most recent) using wrapping arithmetic.
            let idx = (self.capacity + self.head - 1 - i) % self.capacity;
            let v = self.slots[idx];
            if v != NO_DATA {
                max = Some(match max {
                    None => v,
                    Some(m) => m.max(v),
                });
            }
        }
        max
    }
}

// ── File-backed ring buffer (T1, T2) ─────────────────────────────────────────

/// File-backed ring buffer for T1 and T2 tiers.
///
/// Writes are in-place seeks of exactly 12 bytes; the file is never rewritten,
/// keeping flash wear (T2 lives on /boot) proportional to write rate, not file size.
pub struct FileRingBuffer {
    file: File,
    slot_count: u32,
    interval_s: u32,
}

impl FileRingBuffer {
    fn slot_offset(bucket: u32, slot_count: u32) -> u64 {
        HEADER_SIZE as u64 + (bucket % slot_count) as u64 * SLOT_SIZE as u64
    }

    /// Open an existing `.qfrb` file or create one with the correct header.
    /// Returns an error on magic/version mismatch — never silently misreads.
    pub fn open_or_create(
        path: &Path,
        tier: Tier,
        series: &Series,
        unit: Unit,
        now_unix: i64,
    ) -> Result<Self> {
        let slot_count = tier.slot_count();
        let interval_s = tier.interval_s();

        if path.exists() {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;

            let mut buf = [0u8; HEADER_SIZE];
            file.read_exact(&mut buf).context("reading header")?;
            let hdr = Header::decode(&buf)?;

            if hdr.interval_s != interval_s || hdr.slot_count != slot_count {
                bail!(
                    "{}: interval/slot_count mismatch (file: {}/{}, tier: {}/{})",
                    path.display(),
                    hdr.interval_s,
                    hdr.slot_count,
                    interval_s,
                    slot_count
                );
            }

            Ok(Self {
                file,
                slot_count,
                interval_s,
            })
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }

            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;

            let name = series.name();
            let name_bytes = name.as_bytes();
            let mut series_name = [0u8; 64];
            let len = name_bytes.len().min(64);
            series_name[..len].copy_from_slice(&name_bytes[..len]);

            let hdr = Header {
                interval_s,
                slot_count,
                unit,
                series_name,
                created_unix: now_unix,
            };
            file.write_all(&hdr.encode()).context("writing header")?;

            // Pre-size file so all slot positions exist as zeroed bytes.
            let total = HEADER_SIZE as u64 + slot_count as u64 * SLOT_SIZE as u64;
            file.set_len(total).context("pre-sizing file")?;

            Ok(Self {
                file,
                slot_count,
                interval_s,
            })
        }
    }

    /// Read the slot at `bucket`.
    ///
    /// Returns `None` (gap) when the stored bucket doesn't match — the ring has
    /// wrapped past this bucket. Returns `Some` with `count == 0` and
    /// `min/avg/max == NO_DATA` when the slot was never written or is explicitly
    /// empty. Never returns `Some` with value 0 when the slot has no data.
    pub fn read_slot(&mut self, bucket: u32) -> Result<Option<Slot>> {
        let offset = Self::slot_offset(bucket, self.slot_count);
        self.file
            .seek(SeekFrom::Start(offset))
            .context("seek for read")?;
        let mut buf = [0u8; SLOT_SIZE];
        self.file.read_exact(&mut buf).context("reading slot")?;
        let raw = Slot::decode(&buf);

        if raw.bucket != bucket {
            return Ok(None); // gap: ring wrapped, this bucket is evicted
        }

        if raw.count == 0 {
            // Never written (zeroed) or explicitly empty: normalize to NO_DATA sentinel.
            return Ok(Some(Slot {
                bucket,
                min: NO_DATA,
                avg: NO_DATA,
                max: NO_DATA,
                count: 0,
            }));
        }

        Ok(Some(raw))
    }

    /// Write a slot in-place at the computed offset (12-byte seek+write, no file rewrite).
    pub fn write_slot(&mut self, slot: Slot) -> Result<()> {
        let offset = Self::slot_offset(slot.bucket, self.slot_count);
        self.file
            .seek(SeekFrom::Start(offset))
            .context("seek for write")?;
        self.file.write_all(&slot.encode()).context("writing slot")
    }

    /// Feed a raw sample into the bucket covering `unix_time`.
    ///
    /// Running-average strategy: reconstruct the running sum from `avg × count`
    /// (rounding error ≤ 0.5 deci-unit per prior call, negligible for display).
    /// Uses i64 for the intermediate sum to prevent overflow even at count = 65535
    /// and value = i16::MAX (max product: 32767 × 65535 = 2.1 × 10⁹, fits in i64).
    pub fn write_sample(&mut self, unix_time: u64, value: i16) -> Result<()> {
        let bucket = (unix_time / self.interval_s as u64) as u32;

        let new_slot = match self.read_slot(bucket)? {
            Some(s) if s.count > 0 => {
                let sum: i64 = s.avg as i64 * s.count as i64 + value as i64;
                let new_count = s.count.saturating_add(1);
                Slot {
                    bucket,
                    min: s.min.min(value),
                    avg: (sum / new_count as i64) as i16,
                    max: s.max.max(value),
                    count: new_count,
                }
            }
            _ => Slot {
                bucket,
                min: value,
                avg: value,
                max: value,
                count: 1,
            },
        };

        self.write_slot(new_slot)
    }

    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    pub fn interval_s(&self) -> u32 {
        self.interval_s
    }
}

// ── Export ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum ExportFormat {
    Csv,
}

/// Export buckets covering `[from_unix, to_unix]` to `out`.
///
/// Gap rows (bucket mismatch — ring wrapped) and no-data rows (count == 0) use
/// empty min/avg/max/count fields with a "gap" or "no_data" label, so they are
/// visibly distinguishable from real zeros in the output.
pub fn export(
    rb: &mut FileRingBuffer,
    from_unix: u64,
    to_unix: u64,
    format: ExportFormat,
    out: &mut dyn Write,
) -> Result<()> {
    let interval = rb.interval_s() as u64;
    let from_bucket = from_unix / interval;
    let to_bucket = to_unix / interval;

    match format {
        ExportFormat::Csv => {
            writeln!(out, "unix_time,bucket,min,avg,max,count").context("write CSV header")?;
            for b in from_bucket..=to_bucket {
                let unix_time = b * interval;
                match rb.read_slot(b as u32)? {
                    None => {
                        writeln!(out, "{unix_time},{b},,,,gap").context("write gap row")?;
                    }
                    Some(s) if s.count == 0 => {
                        writeln!(out, "{unix_time},{b},,,,no_data").context("write no_data row")?;
                    }
                    Some(s) => {
                        writeln!(
                            out,
                            "{unix_time},{b},{},{},{},{}",
                            s.min, s.avg, s.max, s.count
                        )
                        .context("write data row")?;
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn minute_rb(dir: &TempDir, name: &str) -> FileRingBuffer {
        let path = dir.path().join(format!("{name}.qfrb"));
        FileRingBuffer::open_or_create(
            &path,
            Tier::Minute,
            &Series::CpuPackage,
            Unit::DeciCelsius,
            0,
        )
        .unwrap()
    }

    // R4a: round-trip — write a slot, read it back, values and bucket match.
    #[test]
    fn r4a_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut rb = minute_rb(&dir, "round_trip");

        let slot = Slot {
            bucket: 100,
            min: 300,
            avg: 350,
            max: 400,
            count: 10,
        };
        rb.write_slot(slot).unwrap();

        let got = rb.read_slot(100).unwrap().expect("slot must be Some");
        assert_eq!(got.bucket, 100);
        assert_eq!(got.min, 300);
        assert_eq!(got.avg, 350);
        assert_eq!(got.max, 400);
        assert_eq!(got.count, 10);
    }

    // R4b: wrap test — ring overwrites oldest; evicted bucket → gap (None).
    #[test]
    fn r4b_wrap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wrap.qfrb");
        // Tier::Raw has slot_count=1800; writing bucket 1800 wraps to index 0,
        // overwriting bucket 0.
        let mut rb = FileRingBuffer::open_or_create(
            &path,
            Tier::Raw,
            &Series::CpuPackage,
            Unit::DeciCelsius,
            0,
        )
        .unwrap();

        let sc = rb.slot_count();
        assert_eq!(sc, 1800);

        rb.write_slot(Slot {
            bucket: 0,
            min: 10,
            avg: 10,
            max: 10,
            count: 1,
        })
        .unwrap();
        assert!(
            rb.read_slot(0).unwrap().is_some(),
            "bucket 0 must be present before wrap"
        );

        // Bucket sc maps to slot index sc % sc = 0 — overwrites bucket 0.
        rb.write_slot(Slot {
            bucket: sc,
            min: 99,
            avg: 99,
            max: 99,
            count: 1,
        })
        .unwrap();

        assert!(
            rb.read_slot(0).unwrap().is_none(),
            "evicted bucket must be a gap"
        );

        let got = rb
            .read_slot(sc)
            .unwrap()
            .expect("new bucket must be present");
        assert_eq!(got.min, 99);
    }

    // R4c: unwritten slot → gap (None); count-0 bucket → no-data (not 0).
    #[test]
    fn r4c_no_data() {
        let dir = TempDir::new().unwrap();
        let mut rb = minute_rb(&dir, "no_data");

        // Unwritten slot: slot index 42 has zeroed bytes; stored bucket = 0 ≠ 42 → gap.
        let result = rb.read_slot(42).unwrap();
        assert!(result.is_none(), "unwritten slot must be a gap (None)");

        // Explicitly written count-0 slot → reads back as NO_DATA, never 0.
        rb.write_slot(Slot {
            bucket: 99,
            min: NO_DATA,
            avg: NO_DATA,
            max: NO_DATA,
            count: 0,
        })
        .unwrap();
        let got = rb
            .read_slot(99)
            .unwrap()
            .expect("count-0 slot must be Some");
        assert_eq!(got.count, 0);
        assert_eq!(got.min, NO_DATA, "count-0 slot min must be NO_DATA, not 0");
        assert_ne!(got.min, 0, "count-0 slot must not read back as 0");
    }

    // R4d: aggregation — min/avg/max/count correct over a known sequence (partial bucket ok).
    #[test]
    fn r4d_aggregation() {
        let dir = TempDir::new().unwrap();
        let mut rb = minute_rb(&dir, "agg");

        // Bucket 500 starts at unix_time = 500 * 60 = 30000.
        // Write 5 samples in the same bucket.
        let base = 500u64 * 60;
        for (i, &v) in [100i16, 200, 300, 400, 500].iter().enumerate() {
            rb.write_sample(base + i as u64 * 2, v).unwrap();
        }

        let got = rb.read_slot(500).unwrap().expect("must have data");
        assert_eq!(got.bucket, 500);
        assert_eq!(got.count, 5);
        assert_eq!(got.min, 100);
        assert_eq!(got.max, 500);
        // mean([100,200,300,400,500]) = 1500/5 = 300
        assert_eq!(got.avg, 300, "avg must be 300 for [100,200,300,400,500]");
    }

    // R4e: header round-trip; bad magic and unknown version are rejected with errors.
    #[test]
    fn r4e_header_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("header.qfrb");

        // Create file for a Disk series.
        FileRingBuffer::open_or_create(
            &path,
            Tier::Minute,
            &Series::Disk("disk1".into()),
            Unit::DeciCelsius,
            12345678,
        )
        .unwrap();

        // Re-open the file and inspect the raw header bytes.
        let hdr = {
            let mut f = std::fs::File::open(&path).unwrap();
            let mut buf = [0u8; HEADER_SIZE];
            f.read_exact(&mut buf).unwrap();
            Header::decode(&buf).unwrap()
        };

        assert_eq!(hdr.interval_s, Tier::Minute.interval_s());
        assert_eq!(hdr.slot_count, Tier::Minute.slot_count());
        assert_eq!(hdr.unit, Unit::DeciCelsius);
        assert_eq!(hdr.created_unix, 12345678);
        assert_eq!(hdr.series_name_str(), "disk.disk1");

        // Bad magic → error.
        let bad_magic = dir.path().join("bad_magic.qfrb");
        std::fs::write(&bad_magic, [0xFFu8; 128]).unwrap();
        assert!(
            FileRingBuffer::open_or_create(
                &bad_magic,
                Tier::Minute,
                &Series::CpuPackage,
                Unit::DeciCelsius,
                0
            )
            .is_err(),
            "bad magic must be rejected"
        );

        // Unknown format version → error.
        let bad_ver = dir.path().join("bad_ver.qfrb");
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..6].copy_from_slice(&99u16.to_le_bytes());
        std::fs::write(&bad_ver, buf).unwrap();
        assert!(
            FileRingBuffer::open_or_create(
                &bad_ver,
                Tier::Minute,
                &Series::CpuPackage,
                Unit::DeciCelsius,
                0
            )
            .is_err(),
            "unknown format version must be rejected"
        );
    }

    // R4f: export CSV for a known range with a gap; gap is distinguishable from zeros.
    #[test]
    fn r4f_export_csv() {
        let dir = TempDir::new().unwrap();
        let mut rb = minute_rb(&dir, "export");

        let interval = Tier::Minute.interval_s() as u64;
        // Write buckets 0 and 2; leave bucket 1 unwritten (gap).
        rb.write_slot(Slot {
            bucket: 0,
            min: 100,
            avg: 150,
            max: 200,
            count: 5,
        })
        .unwrap();
        rb.write_slot(Slot {
            bucket: 2,
            min: 300,
            avg: 350,
            max: 400,
            count: 5,
        })
        .unwrap();

        let mut out: Vec<u8> = Vec::new();
        export(&mut rb, 0, 2 * interval, ExportFormat::Csv, &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();

        // Buckets 0 and 2 have real data.
        assert!(csv.contains("100"), "bucket 0 min must appear");
        assert!(csv.contains("300"), "bucket 2 min must appear");

        // Bucket 1 must be labeled "gap", not 0.
        assert!(csv.contains("gap"), "gap bucket must be labeled gap");

        // Confirm the bucket-1 row does not contain "0" as its min field.
        for line in csv.lines().skip(1) {
            let cols: Vec<&str> = line.split(',').collect();
            if cols.get(1).copied() == Some("1") {
                // min is the 3rd column (index 2); must be empty for a gap.
                assert_ne!(
                    cols.get(2).copied().unwrap_or(""),
                    "0",
                    "gap row must not have 0 as min"
                );
            }
        }
    }

    // Slot count is computed from retention/interval, not hardcoded.
    #[test]
    fn tier_slot_count_computed() {
        assert_eq!(Tier::Raw.slot_count(), 1800, "1 h / 2 s = 1800");
        assert_eq!(Tier::Minute.slot_count(), 20_160, "14 days / 60 s = 20160");
        assert_eq!(
            Tier::Hour.slot_count(),
            43_800,
            "5 * 365 days / 3600 s = 43800"
        );
    }

    // Ring buffer T0: push + window_max.
    #[test]
    fn ring_buffer_window_max() {
        let mut rb = RingBuffer::new(Series::CpuPackage, 10);
        assert_eq!(rb.window_max(5), None, "empty ring must return None");

        rb.push(100);
        rb.push(200);
        rb.push(50);

        assert_eq!(rb.window_max(3), Some(200));
        assert_eq!(
            rb.window_max(1),
            Some(50),
            "window of 1 returns most recent"
        );
        assert_eq!(rb.window_max(100), Some(200), "clamped to capacity");
    }

    // Ring buffer T0: wrap overwrites oldest, window_max reflects new data.
    #[test]
    fn ring_buffer_wrap() {
        let mut rb = RingBuffer::new(Series::CpuPackage, 3);
        rb.push(10);
        rb.push(20);
        rb.push(30);
        // Next push wraps: overwrites slot 0 (value 10).
        rb.push(5);
        // Last 3 slots are [5, 20, 30] — max is 30.
        assert_eq!(rb.window_max(3), Some(30));
    }
}
