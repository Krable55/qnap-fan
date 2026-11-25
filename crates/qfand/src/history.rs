//! History persistence: the writer side of the `.qfrb` tiers.
//!
//! F17: `qfan-hist` was complete and tested, and `ipc::handle_history` already
//! read `<hist_base>/<tier>/<series>.qfrb`, but nothing ever created those
//! files. The socket API answered `slots: []` correctly and would have done so
//! forever. Both sides passed their own tests; the seam between them was not
//! anyone's task. This module is that seam.
//!
//! Design constraints that shape this code (DESIGN.md §6.2, D2):
//!
//! - **The control loop must never block on storage.** Writes happen on a
//!   separate thread fed by an unbounded channel. If the cache pool is spun
//!   down or appdata is missing, the fans keep being controlled. A 12-18 ms EC
//!   write budget (F6) cannot absorb a flash write.
//! - **The hour tier lives on flash** (`/boot/config/plugins/qnap-fan/history`)
//!   so multi-year history is readable before the array mounts. 27 series x 12
//!   bytes once an hour is negligible wear.
//! - **The minute tier lives in appdata** on the cache pool, because it carries
//!   the real write volume (~350 KB/day). When appdata is absent the tier is
//!   skipped rather than failing the daemon.
//! - **A write error must never kill the daemon.** Errors are rate-limited to
//!   the log; fan control continues.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use qfan_core::Series;
use qfan_hist::{FileRingBuffer, Tier, Unit};

/// Where each tier's files live. Separate roots because they have different
/// durability and write-volume characteristics (D2).
pub struct HistoryPaths {
    /// Hour tier: flash, readable before the array mounts.
    pub hour_base: PathBuf,
    /// Minute tier: appdata on the cache pool. `None` disables the tier.
    pub minute_base: Option<PathBuf>,
}

impl HistoryPaths {
    /// Production layout. The minute tier is enabled only if its parent exists,
    /// so a box whose appdata has not mounted yet still records the hour tier
    /// rather than logging an error every minute.
    pub fn production() -> Self {
        let appdata = PathBuf::from("/mnt/user/appdata/qnap-fan/history");
        // Enable the minute tier only when /mnt/user/appdata exists; otherwise
        // the tier is skipped rather than erroring once per tick.
        let appdata_root = PathBuf::from("/mnt/user/appdata");
        let minute_base = if appdata_root.exists() {
            Some(appdata)
        } else {
            None
        };

        Self {
            hour_base: PathBuf::from("/boot/config/plugins/qnap-fan/history"),
            minute_base,
        }
    }
}

/// One sample handed to the writer thread.
pub struct Sample {
    pub series: Series,
    pub unit: Unit,
    pub value: i16,
    pub unix_time: u64,
}

/// Handle held by the control loop. Dropping it closes the channel, which ends
/// the writer thread after it drains.
#[derive(Clone)]
pub struct HistoryWriter {
    tx: Option<Sender<Sample>>,
}

impl HistoryWriter {
    /// A writer that discards everything. Used in dry-run and in tests, so the
    /// control loop needs no `Option` branching at the call site.
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Queue a sample. Never blocks, never fails the caller: if the writer
    /// thread is gone the sample is dropped, because losing a history point is
    /// always preferable to stalling the loop that controls the fans.
    pub fn record(&self, series: Series, unit: Unit, value: i16, unix_time: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Sample {
                series,
                unit,
                value,
                unix_time,
            });
        }
    }
}

/// Spawn the writer thread and return its handle.
///
/// Shutdown is by channel close: drop every `HistoryWriter`, then `join()`.
/// There is deliberately no stop flag — a flag checked inside the receive loop
/// would abandon queued samples, making the drain on shutdown decorative.
pub fn spawn(paths: HistoryPaths) -> (HistoryWriter, std::thread::JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel::<Sample>();
    let handle = std::thread::Builder::new()
        .name("qfand-hist".into())
        .spawn(move || writer_thread(paths, rx))
        .expect("spawning history writer thread");

    (HistoryWriter { tx: Some(tx) }, handle)
}

/// Open files lazily and keep them open, keyed by (tier, series name).
/// Keyed by (tier dir name, series name) — `Tier` is not Hash/Eq.
type Handles = HashMap<(&'static str, String), FileRingBuffer>;

fn writer_thread(paths: HistoryPaths, rx: Receiver<Sample>) {
    let mut handles: Handles = HashMap::new();
    let mut err_count: u64 = 0;

    // Ends when every sender is dropped: recv() returns Err and the queue is
    // already empty, so no sample is lost on a clean shutdown.
    while let Ok(sample) = rx.recv() {
        // Hour tier always; minute tier only when its root is configured.
        let targets: [(Tier, Option<&Path>); 2] = [
            (Tier::Hour, Some(paths.hour_base.as_path())),
            (Tier::Minute, paths.minute_base.as_deref()),
        ];

        for (tier, base) in targets {
            let Some(base) = base else { continue };
            if let Err(e) = write_one(&mut handles, base, tier, &sample) {
                err_count += 1;
                // Rate-limit: first few, then every 1000th. A spun-down pool
                // must not fill the syslog, and must not stop fan control.
                if err_count <= 3 || err_count.is_multiple_of(1000) {
                    eprintln!("[qfand] history write failed ({} total): {e:#}", err_count);
                }
            }
        }
    }

    // Drain is implicit: recv() returned Err because the channel closed.
}

fn write_one(
    handles: &mut Handles,
    base: &Path,
    tier: Tier,
    sample: &Sample,
) -> anyhow::Result<()> {
    let name = sample.series.name();
    let key = (tier_dir(tier), name.clone());

    if !handles.contains_key(&key) {
        let dir = base.join(tier_dir(tier));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.qfrb"));
        let rb = FileRingBuffer::open_or_create(
            &path,
            tier,
            &sample.series,
            sample.unit,
            sample.unix_time as i64,
        )?;
        handles.insert(key.clone(), rb);
    }

    let rb = handles.get_mut(&key).expect("just inserted");
    rb.write_sample(sample.unix_time, sample.value)?;
    Ok(())
}

/// Directory name per tier. Must match what `ipc::handle_history` joins, which
/// is the string the socket client sends: "raw", "minute", "hour".
pub fn tier_dir(tier: Tier) -> &'static str {
    match tier {
        Tier::Raw => "raw",
        Tier::Minute => "minute",
        Tier::Hour => "hour",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qfand_hist_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The whole point of F17: a recorded sample must become a file that
    /// `ipc::handle_history` can find, at exactly the path it builds.
    #[test]
    fn recorded_sample_lands_at_the_path_ipc_reads() {
        let root = tmpdir("path");
        let paths = HistoryPaths {
            hour_base: root.clone(),
            minute_base: Some(root.clone()),
        };
        let (w, handle) = spawn(paths);

        w.record(Series::CpuPackage, Unit::DeciCelsius, 520, 1_700_000_000);
        drop(w); // closes channel; thread drains and exits
        handle.join().unwrap();

        // These are the exact joins ipc::handle_history performs.
        for tier_name in ["hour", "minute"] {
            let p = root.join(tier_name).join("cpu.package.qfrb");
            assert!(
                p.exists(),
                "ipc reads {}, which was never created",
                p.display()
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reading back through the same API the socket uses must return the value,
    /// not a gap. Guards against writing a file whose bucket arithmetic does
    /// not match what the reader computes.
    #[test]
    fn written_sample_reads_back_as_real_data() {
        let root = tmpdir("roundtrip");
        let paths = HistoryPaths {
            hour_base: root.clone(),
            minute_base: None,
        };
        let (w, handle) = spawn(paths);

        let t = 1_700_000_000u64;
        w.record(Series::CpuPackage, Unit::DeciCelsius, 530, t);
        drop(w);
        handle.join().unwrap();

        let path = root.join("hour").join("cpu.package.qfrb");
        let mut rb = FileRingBuffer::open_or_create(
            &path,
            Tier::Hour,
            &Series::CpuPackage,
            Unit::DeciCelsius,
            t as i64,
        )
        .unwrap();

        let bucket = (t / Tier::Hour.interval_s() as u64) as u32;
        let slot = rb.read_slot(bucket).unwrap().expect("slot must exist");
        assert_eq!(slot.count, 1, "sample must be counted");
        assert_eq!(slot.avg, 530, "value must round-trip");
        assert_ne!(slot.avg, qfan_hist::NO_DATA, "must not read back as a gap");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Disk series are keyed by Unraid name, and those names reach the
    /// filesystem. F18 produced names like `"\"parity\""`; a quoted name would
    /// create a file the reader cannot address.
    #[test]
    fn disk_series_filename_is_clean() {
        let root = tmpdir("disk");
        let paths = HistoryPaths {
            hour_base: root.clone(),
            minute_base: None,
        };
        let (w, handle) = spawn(paths);

        w.record(
            Series::Disk("parity".into()),
            Unit::DeciCelsius,
            330,
            1_700_000_000,
        );
        drop(w);
        handle.join().unwrap();

        let p = root.join("hour").join("disk.parity.qfrb");
        assert!(p.exists(), "expected {}", p.display());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A broken storage path must not take down fan control. The writer is on
    /// its own thread precisely so this is survivable.
    #[test]
    fn unwritable_base_does_not_panic_or_block() {
        let paths = HistoryPaths {
            // /proc is real but cannot host new directories.
            hour_base: PathBuf::from("/proc/qfand-cannot-exist"),
            minute_base: None,
        };
        let (w, handle) = spawn(paths);

        for i in 0..50 {
            w.record(
                Series::CpuPackage,
                Unit::DeciCelsius,
                500,
                1_700_000_000 + i,
            );
        }
        drop(w);
        handle.join().expect("writer thread must not panic");
    }

    /// `disabled()` is used in dry-run; it must be a no-op, not a crash.
    #[test]
    fn disabled_writer_is_inert() {
        let w = HistoryWriter::disabled();
        w.record(Series::CpuPackage, Unit::DeciCelsius, 500, 1_700_000_000);
    }

    /// The tier directory names must match the strings the socket API accepts,
    /// or the writer and reader disagree about where files live — which is the
    /// F17 class of bug all over again.
    #[test]
    fn tier_dir_names_match_socket_tier_strings() {
        assert_eq!(tier_dir(Tier::Raw), "raw");
        assert_eq!(tier_dir(Tier::Minute), "minute");
        assert_eq!(tier_dir(Tier::Hour), "hour");
    }
}
