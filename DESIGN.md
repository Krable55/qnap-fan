# qnap-fan — fan control for Unraid on QNAP EC hardware

Rust daemon, ratatui TUI, and Unraid plugin for controlling the fans on a QNAP
TVS-1288 running Unraid, via the third-party `qnap-ec` kernel module.

## 1. Problem

The CPU fans on this machine are wired to QNAP's own embedded controller (ITE
IT8528), not to a standard motherboard fan chip. With QTS removed, nothing
adjusts them: the EC holds whatever PWM value was last written to it, forever,
with no automatic fallback. On this box that was ~28% duty, which let the Xeon
W-1250 reach 100°C daily and hard-crash the server on 2026-11-24.

Causally confirmed during investigation: forcing `pwm7=255` dropped idle package
temperature from 58–62°C to 34–39°C, and load bursts from 73–84°C to 51–61°C.
The cooler and thermal paste are fine (~0.4 °C/W over ambient at full fan speed).
This is a control-software gap, not a hardware fault.

An interim bash fix (User Scripts `control_fans`) now drives `pwm7` on a curve.
Its limits are what this project replaces: a ~12 s dead window every minute, no
recovery if the script dies, ~28% fans for the first minute after boot, and
`smartctl` calls that can wake sleeping drives.

## 2. Verified hardware facts

Measured on the live machine, 2026-11-24. Everything below is confirmed, not assumed.

| Item | Value |
|---|---|
| Host | QNAP TVS-1288, Xeon W-1250 (6C/12T, 80 W), Unraid 7.3.2, kernel 6.18.38 |
| EC hwmon | `name=qnap_ec`, at `/sys/devices/platform/qnap-ec/hwmon/hwmon3` |
| coretemp hwmon | `name=coretemp`, `temp1_input`=package, `temp2..7_input`=cores 0–5 |
| `pwm1` | system fans → `fan1..4_input` (~1520–1540 RPM at PWM 84) |
| `pwm7` | **CPU fans** → `fan7_input`, `fan8_input` (~1184–1248 RPM at PWM 135) |
| `pwm21` | → `fan22..25`, `fan32..35`; `fan22`=7453 constant, `fan32`=0 |
| Drive temps | `/var/local/emhttp/disks.ini`, `temp="34"` per disk, maintained by Unraid |
| Notifications | `/usr/local/emhttp/webGui/scripts/notify` |
| Module config | `/etc/modprobe.d/qnap-ec.config` → `options qnap-ec check-for-chip=no` |

### 2.1 hwmon index is not stable

The `hwmonN` number changes between boots. Resolve by scanning
`/sys/class/hwmon/*/name` for the literal strings `qnap_ec` and `coretemp` at
startup, and re-resolve if a read fails with `ENOENT`. Never hardcode `hwmon3`.

### 2.2 EC access is expensive — this drives the whole design

Measured read latency, 20 samples each:

| Path | Per-read |
|---|---|
| `qnap_ec/pwm1` | 12 ms |
| `qnap_ec/pwm7` | 13 ms |
| `qnap_ec/fan7_input` | 18 ms |
| `coretemp/temp1_input` | 0.5 ms |

`qnap-ec.ko` does not talk to the chip directly. It calls out to the userspace
helper `/usr/bin/qnap-ec` through the character device `/dev/qnap-ec` on **every
single access**. Each EC read or write is a process spawn, ~25× the cost of a
normal sysfs read.

Consequences, all of which are requirements and not optimisations:

- Poll `coretemp` fast (2 s). Poll EC fan RPM slow (10 s). These are different tasks.
- Write a PWM **only when the intended value changes**.
- Never poll `pwm21` or the `fan22..35` placeholder channels at all.
- A full EC sweep is ~150 ms of forking. Budget it; do not do it in a tight loop.
  This box has already OOM-killed Plex twice.

### 2.3 The EC rounds PWM to whole percent

Write 73, read back 71. Write 180, read back 178. Read-back never equals the
written value.

**Change detection must compare against the daemon's own last-intended value,
never against a read-back.** Comparing to read-back produces an infinite rewrite
loop — on this hardware that means a permanent fork treadmill.

### 2.4 The EC has no failsafe of its own

If the controlling process dies, the fans hold their last value indefinitely.
There is no watchdog in the chip and no automatic ramp. Every failure path in
this software must end with the fans at 255.

## 3. Decisions

- **D1** Rust, cross-compiled to `x86_64-unknown-linux-musl`. Static, no runtime,
  flat memory. Compiling on the NAS is what triggered the 100°C event, so the box
  never builds its own binary.
- **D2** One binary, multiple subcommands: `qfand daemon`, `qfand tui`,
  `qfand status`, `qfand export`. Shared curve/storage code, one artifact to ship.
- **D3** The daemon is the **sole writer** of EC PWM values. TUI and web page are
  clients over a Unix socket. Curve logic exists in exactly one place.
- **D4** The daemon owns both `pwm1` and `pwm7` from day one. `pwm21` is never
  touched.
- **D5** Fixed-slot round-robin history files, not SQLite. See §6.
- **D6** Distributed as a GitHub release; `.plg` fetches a versioned, checksummed
  artifact. CI builds it.

## 4. Architecture

```
                   ┌──────────────────────────────────────┐
                   │ qfand daemon (sole EC writer)        │
   coretemp 2s ───▶│  sensors ─▶ curves ─▶ pwm writer     │──▶ /sys .../pwm1
   disks.ini 30s ──▶│                │                     │──▶ /sys .../pwm7
   EC fan RPM 10s ─▶│                ▼                     │
                   │           history tiers              │
                   │                │                     │
                   │         unix socket API              │
                   └────────────────┬─────────────────────┘
                                    │  /var/run/qfand.sock
                   ┌────────────────┼─────────────────────┐
                   ▼                ▼                     ▼
            qfand tui         PHP .page              qfand status
            (ratatui)      (Unraid webGui)            (scripts)

   rc.qfand watchdog  ──  if daemon pid is gone: pwm1=pwm7=255 + notify
```

### 4.1 Control loop

Three independent tasks, so a slow EC read can never stall the control decision:

1. **Fast sensor task, 2 s** — read `coretemp` package + 6 cores. Cheap.
2. **Slow sensor task** — `disks.ini` every 30 s, EC fan RPM every 10 s.
3. **Control task, 2 s** — evaluate curves against the latest sensor snapshot,
   write a PWM only if the intended value differs from the last intended value.

### 4.2 Curve evaluation

Per channel: a temperature source, a list of `(temp, pwm)` breakpoints with
linear interpolation between them, a floor, and asymmetric slew.

- `pwm7` ← max of package temp over a trailing 30 s window, so brief single-core
  spikes don't cause surging. Default breakpoints 45°C→90 (35%), 70°C→255 (100%).
- `pwm1` ← max drive temp from `disks.ini`, with EC board temp as a floor.
  Default 35°C→85, 60°C→180. Preserves the existing behaviour.
- Slew: ramp **up** immediately, ramp **down** at most 20 PWM steps per cycle.
  Thermal risk is asymmetric; being late to speed up is dangerous, being late to
  slow down is just noise.
- Hysteresis: a few degrees of deadband so the fans don't oscillate at a breakpoint.

### 4.3 Failsafe

Every exit path ends with both PWMs at 255:

- `Drop` guard on the PWM writer handle — covers normal exit, `?` propagation,
  and panic unwind.
- Signal handler for `SIGTERM`/`SIGINT`/`SIGHUP` — set 255, then exit.
- `panic::set_hook` — set 255 before the default hook runs, since a panic in a
  worker task would not otherwise reach the Drop guard.
- Sensor read failure → that channel goes to 255 and an event is logged.
  Unreadable temperature is treated as hot, never as cold.
- **Emergency override** at package ≥90°C: all owned channels to 255 plus an
  Unraid notification. Latches until temperature drops below 80°C.
- **External watchdog** (`rc.qfand`, bash): if the daemon pid disappears, set
  `pwm1` and `pwm7` to 255 and notify. Rust cannot catch `SIGKILL`, OOM-kill, or
  a hard crash, and §2.4 means the fans would otherwise stay wherever they were.

### 4.4 Manual override

Socket clients may request a fixed PWM, but **an override must carry an expiry**
(default 10 min, hard cap 1 h). On expiry the daemon logs the fact and resumes
curve control. A manual override with no timeout is the exact failure mode that
caused the original crash; the API must make it impossible to leave one set.

## 5. Socket API

`/var/run/qfand.sock`, mode 0660, newline-delimited JSON request/response.

| Request | Response |
|---|---|
| `{"op":"status"}` | channels, intended + readback PWM, RPMs, all temps, mode, uptime |
| `{"op":"history","series":"cpu.package","tier":"hour","from":…,"to":…}` | slot array |
| `{"op":"config"}` | current effective config |
| `{"op":"set_curve","channel":"pwm7","points":[…]}` | validated, applied, persisted |
| `{"op":"override","channel":"pwm7","pwm":200,"ttl_s":600}` | accepted + expiry |
| `{"op":"clear_override","channel":"pwm7"}` | ok |
| `{"op":"events","since":…}` | recent event log |

Curve writes are validated before they are applied: breakpoints must be
monotonic in temperature, PWM in 0–255, and a curve that would allow <35% duty
above 80°C is rejected outright.

## 6. History storage

### 6.1 Why not SQLite

- `rusqlite` with the bundled amalgamation is a C compile that would dominate
  both cross-build time and binary size, defeating the reason Rust was chosen.
- A database can only live on the cache pool, which **is not mounted at boot**,
  when fan control must already be running.
- Downsampling becomes scheduled `INSERT … SELECT` + `DELETE` + `VACUUM`. With
  fixed slots, aggregation is "write to this offset" and retention is automatic
  because the ring overwrites itself. There is no cleanup job that can fall behind.

A year of 2 s raw samples is 15.8M points per series and useless for a
months-scale chart, which aggregates to a few hundred pixels anyway. What a long
view needs is pre-aggregated min/avg/max per bucket. Fixed buckets make every
sample's file offset computable, so there is no index and no query engine.

### 6.2 Tiers

| Tier | Interval | Retention | Location | Total (~27 series) |
|---|---|---|---|---|
| T0 | 2 s raw | 1 h | RAM ring | 97 KB |
| T1 | 1 min min/avg/max | 14 days | appdata on cache | 5.4 MB |
| T2 | 1 h min/avg/max | 5 years | `/boot/config/plugins/qnap-fan/` | 14 MB |

- **T2 lives on flash.** 27 series × 12 bytes = 324 bytes written once an hour,
  in place at a computed offset, with no file rewrite. ~8,760 writes/year is
  negligible flash wear. In exchange the full multi-year history is readable the
  instant the daemon starts, before the array mounts, and survives an array that
  never comes up at all.
- **T1 lives in appdata**, because it is the tier with real write volume
  (~350 KB/day). It degrades to RAM-only when appdata is not yet mounted, and
  back-fills on mount. The control loop never blocks on storage.

### 6.3 File format

One file per series — `history/<tier>/<series>.qfrb`. Adding or removing a disk
is a file create or delete with no migration, which matters because the drive set
does change. A single packed file would need a schema migration for every disk
swap.

```
header, 128 bytes, fixed:
  magic "QFRB" (4) | format_version u16 | reserved u16
  interval_s u32 | slot_count u32 | unit (enum u8: deci_celsius|pwm|rpm)
  series name, 64 bytes, NUL-padded
  created_unix i64 | reserved to 128

slot, 12 bytes, at offset 128 + (bucket % slot_count) * 12:
  bucket u32    // unix_time / interval_s — makes the slot self-describing
  min   i16     // deci-units
  avg   i16
  max   i16
  count u16     // samples in the bucket; honest averages and partial buckets
```

- The per-slot `bucket` field is what makes downtime unambiguous: a slot whose
  bucket doesn't match the requested one is a gap, not stale data.
- **`i16::MIN` is the explicit no-data sentinel.** Without it, downtime reads back
  as 0°C and the charts lie. SQLite gives this for free with `NULL`; here it is a
  deliberate choice.
- Deci-units (tenths) in `i16` give 0.1°C resolution over −3276..3276°C, ample
  for temperatures, RPM up to 32767, and PWM 0–255.

### 6.4 Series

~27 series: `cpu.package`, `cpu.core0..5`, `pwm.1`, `pwm.7`, `fan.1..4`,
`fan.7`, `fan.8`, `disk.<name>` for each of ~12 disks.

Disks are keyed by **Unraid name** (`parity`, `disk1`, `cache`), never by `sdX`,
because device letters reshuffle across boots and would silently corrupt history.

### 6.5 Export

`qfand export --series cpu.package --tier hour --from … --to … --format csv`
covers the ad-hoc querying that SQLite would otherwise provide. **R3:** adding a
new aggregation interval later needs a converter, where SQLite would need only a
different `GROUP BY`. Accepted: the tiers are driven by what the charts need, and
that is unlikely to churn.

## 7. TUI

`qfand tui`, ratatui + crossterm, run over SSH from the NAS. Client only — it
talks to the socket and holds no control logic.

- **Overview** — per-channel gauges (intended PWM, readback, RPM), package and
  per-core temps, drive temps, current mode, active overrides.
- **History** — sparklines over a selectable window (1 h / 24 h / 14 d / 1 y),
  reading whichever tier matches the span.
- **Curves** — interactive breakpoint editor, preview against recent temperature
  history before applying, then `set_curve`.
- **Events** — scrollback of the daemon event log.
- Degrades honestly: if the socket is unreachable it says so rather than drawing
  stale values.

## 8. Unraid plugin

| File | Purpose |
|---|---|
| `qnap-fan.plg` | XML installer; fetches the versioned `.txz` from the GH release, verifies SHA-256 |
| `qnap-fan.page` | PHP/HTML settings page under Settings → Utilities |
| `rc.qfand` | start / stop / status / restart, plus the §4.3 watchdog |
| `/boot/config/plugins/qnap-fan/config.toml` | persisted config, survives reboots |
| `/usr/local/sbin/qfand` | the static binary |

The PHP page is a thin shim over the socket API — it renders status and posts
curve edits, and contains no fan logic of its own.

Boot order: `rc.qfand` starts from the plugin's install step and from
`/boot/config/go`, before the array comes up, so the fans are under control
within seconds of boot rather than after the first cron minute.

**R1 — cutover hazard.** `qfand` and the existing User Scripts `control_fans`
entry must never run simultaneously; two writers would fight over `pwm1` and
`pwm7`. Disabling the cron schedule and enabling the daemon is one atomic step,
with the bash script retained on flash as rollback.

## 9. Build and release

GitHub Actions on `ubuntu-latest` is x86_64, so
`cargo build --release --target x86_64-unknown-linux-musl` is a native build with
no cross-linker needed. **This removes the earlier cross-compilation blocker (R2)
entirely** — the dev Mac is aarch64 with no musl linker and no running Docker.

- Tag `v*` → build, test, package `.txz`, emit SHA-256, create the release,
  update `qnap-fan.plg` version and URL.
- PR / push → `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, and
  a musl build to prove the shipping target still compiles.
- Local iteration against real hardware uses the `builder` NixOS host
  (x86_64, nix + docker) and `scp` to the NAS. The NAS never compiles.

## 10. Testing

The daemon cannot be tested on the real EC without risking the CPU, so the
hardware layer is a trait with two implementations.

- `EcBackend` trait: `read_temp`, `read_fan`, `read_pwm`, `write_pwm`.
  - `SysfsBackend` — the real one.
  - `FakeBackend` — scriptable temperatures, injectable read/write failures, a
    simple thermal model where PWM affects temperature, and **per-call latency
    matching the measured 12–18 ms** so the EC cost budget is exercised in tests.
- Unit tests: curve interpolation and clamping, slew asymmetry, hysteresis,
  override expiry, curve-validation rejection, ring-buffer wrap, bucket
  aggregation, no-data sentinel handling, the §2.3 rewrite-loop guard.
- Integration tests against `FakeBackend`: emergency latch and release, sensor
  failure → 255, socket API round-trips, failsafe on each shutdown signal path.
- Hardware validation, in order: dry-run mode (log intended writes, write
  nothing) → `pwm7` only with the bash script disabled → both channels → a
  deliberate `SIGKILL` to prove the watchdog ramps to 255 → observe across a
  HolyArrs CPU burst and a parity check.

## 11. Repo layout

```
Cargo.toml                  workspace
crates/
  qfan-core/                curves, config, series defs, units — no I/O
  qfan-hal/                 EcBackend trait, sysfs + fake backends, hwmon discovery
  qfan-hist/                round-robin file format, tiers, aggregation, export
  qfan-proto/              socket request/response types, shared by daemon and TUI
  qfand/                    daemon + CLI + TUI (single binary)
plugin/
  qnap-fan.plg
  qnap-fan.page
  rc.qfand
  config.default.toml
.github/workflows/
  ci.yml
  release.yml
DESIGN.md
```

## 12. Milestones

Bounded units with explicit handoffs; each ends in something runnable or testable.

| # | Deliverable | Done when |
|---|---|---|
| M0 | Workspace scaffold, CI skeleton, config schema | `cargo test` and CI green, no fan logic |
| M1 | `qfan-hal`: hwmon discovery, sysfs + fake backends | Fake backend drives tests; real backend reads the live box read-only |
| M2 | `qfan-core`: curves, slew, hysteresis, overrides | Unit tests cover §4.2 and §4.4 |
| M3 | Control loop + failsafe, `--dry-run` | Runs on the NAS logging intended writes, writing nothing |
| M4 | `qfan-hist`: format, tiers, aggregation, export | Round-trip and wrap tests; export produces CSV |
| M5 | Socket API + `qfand status` | Round-trip integration tests |
| M6 | ratatui TUI | Usable over SSH against a live daemon |
| M7 | Plugin packaging: `.plg`, `.page`, `rc.qfand`, watchdog | Installs on the NAS; watchdog proven with `SIGKILL` |
| M8 | GH Actions release pipeline | Tag produces an installable checksummed artifact |
| M9 | Live cutover | Bash script disabled, daemon owns both channels, observed through a load burst |

## 13. Open risks

- **R1** Dual-writer conflict during cutover (§8).
- **R2** ~~Cross-compilation~~ — resolved by CI on x86_64 (§9).
- **R3** New aggregation intervals need a converter (§6.5).
- **R4** EC fork cost under an unexpectedly chatty web page. The PHP shim must
  read the daemon's cached snapshot, never poll the EC itself.
- **R5** `qnap-ec.ko` is a third-party out-of-tree module. An Unraid kernel
  upgrade can leave it unbuilt, in which case the EC vanishes and the daemon must
  fail loudly rather than silently idle.
- **R6** `pwm21` and the `fan22..35` placeholder readings are not understood.
  Left strictly untouched.
