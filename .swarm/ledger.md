# Acceptance ledger — qnap-fan

Stable IDs. Never renumber; record approved changes instead.

## Requirements

| ID | Requirement | Evidence expected |
|---|---|---|
| R1 | Daemon drives `pwm1` and `pwm7` from per-channel curves; never touches `pwm21` or `fan22..35` | Curve unit tests; dry-run log on NAS showing both channels and no pwm21 access |
| R2 | Every failure path ends with fans at 255: Drop guard, signal handler, panic hook, sensor-read failure, emergency ≥90°C, external watchdog | Integration tests per path; live `SIGKILL` test showing watchdog ramp |
| R3 | EC access budget respected: PWM written only on intended-value change (never vs read-back), coretemp 2s / EC RPM 10s / disks.ini 30s | Test asserting no write when intended value unchanged despite differing read-back; task interval verification |
| R4 | History: 3 tiers, ≥1yr retention, round-robin fixed-slot files, explicit no-data sentinel, per-series files keyed by Unraid disk name | Round-trip + wrap + gap tests; `qfand export` CSV output |
| R5 | Socket API: validated curve writes, override requires expiry with hard cap | API round-trip tests; rejection tests for invalid curve and missing/excessive ttl |
| R6 | ratatui TUI usable over SSH; client-only, degrades honestly when socket unreachable | Live run against daemon on NAS |
| R7 | Unraid plugin installs, appears in webGUI, `rc.qfand` start/stop/status works, config persists across reboot | Install on NAS; page renders; reboot survival |
| R8 | GH Actions: PR gate (test/clippy/fmt/musl build) and tag → checksummed release artifact | Green CI run; release artifact downloadable and SHA verified |
| R9 | Hardware layer is a trait with a fake backend simulating measured 12–18ms EC latency and injectable failures | Tests run against FakeBackend without hardware |

## Tasks

| ID | Task | Serves | Depends on | Owner model | Status |
|---|---|---|---|---|---|
| T1 | M0 scaffold: workspace, crates, config schema, CI skeleton | R8 (partial) | — | sonnet-4-6 | DONE, coord-verified |
| T2 | M1 qfan-hal: hwmon discovery, sysfs + fake backends | R3, R9 | T1 | sonnet-4-6 | DONE, coord-verified (f5059e5) |
| T3 | M2 qfan-core: curves, slew, hysteresis, overrides | R1, R5 (validation) | T1 | sonnet-4-6 | in progress |
| T4 | M3 control loop + failsafe + dry-run | R1, R2, R3 | T2, T3 | sonnet-4-6 | pending |
| T5 | M4 qfan-hist | R4 | T1 | sonnet-4-6 | DONE, coord-verified (0632b4b) |
| T6 | M5 socket API + status subcommand | R5 | T4, T5 | sonnet-4-6 | pending |
| T7 | M6 ratatui TUI | R6 | T6 | sonnet-4-6 | pending |
| T8 | M7 plugin packaging | R7, R2 (watchdog) | T4 | sonnet-4-6 | pending |
| T9 | M8 release pipeline | R8 | T1 | sonnet-4-6 | pending |
| T10 | M9 live cutover | R1, R2, R7 | all | coordinator | pending |

## Model routing

Verified 2026-11-24 via `claude --print --model <alias>`:
- `sonnet` → `claude-sonnet-4-6`
- `opus` → `claude-opus-4-6` (strictly higher tier; eligible reviewer for Sonnet work)
- Claude Code 2.1.76

Review capability floor: Sonnet-4-6 work requires Opus-4-6 independent review.

## Assessment log

`task | complexity | risk | evidence | designated | recommended | reason`

- T1 | mechanical | low | design doc fully specifies layout | sonnet-4-6 | sonnet-4-6 | scaffold, no design choices

## T1 verification record (coordinator-run, not worker self-report)

Commands run by coordinator on 2026-11-24, `export PATH=/etc/profiles/per-user/kyle/bin:$PATH`:

- R8a `cargo build --all` → `Finished dev profile` (needs `LIBRARY_PATH=<nix libiconv>/lib` on this aarch64 Mac; see env quirk below)
- R8b `cargo test --all` → 7 passed, 0 failed
- R8c `cargo clippy --all-targets -- -D warnings` → exit 0, zero errors
- R8d `cargo fmt --check` → exit 0

### Defect found in review and fixed (F8)

Worker's first `Config::validate()` only checked breakpoints at/above 80°C plus
segments crossing 80°C. It never checked the **clamp region above the last
breakpoint**. Coordinator probe confirmed this config was ACCEPTED:

```
breakpoints = [{temp_c: 45, pwm: 50}, {temp_c: 70, pwm: 60}]
```

Above 70°C the curve clamps to 60/255 = 23% duty, so at 95°C the fans would sit
at 23% — the exact failure mode that crashed this server. Probe returned `Ok(())`.

Fix: `ChannelConfig::pwm_at(temp_c)` now implements the real piecewise-linear +
clamp evaluation the daemon will use, and `validate_channel` sweeps `pwm_at(t)`
for every whole degree 80..=110, rejecting any result < 89. The per-breakpoint
and crossing-segment checks were removed as subsumed. Re-probed after the fix:

```
HOLE CASE RESULT: Err(Validation("pwm7: curve produces pwm 60 < 89 (35% duty minimum) at 80°C"))
```

Regression test `reject_clamp_region_low_pwm` now covers it. Coordinator's
throwaway probe files were removed; only worker-authored tests remain.

### Open question from review (Q4)

The sweep starts at 80°C, so a curve may legally dip arbitrarily low just below
it. Verified: `[{79, 10}, {80, 89}]` is accepted, `pwm_at(79) == 10`. That is
~4% duty at 79°C, which is only one degree below the danger threshold. Decide
whether the sweep floor should drop to ~70°C with a lower minimum, or whether a
monotonic-pwm requirement should be added instead. Not blocking T2/T5.

### Environment quirk (not a code issue)

`cargo build` of the `qfand` binary fails on this aarch64 macOS host with
`ld: library not found for -liconv` unless `LIBRARY_PATH` points at a Nix
libiconv, e.g.
`export LIBRARY_PATH=/nix/store/7wxwini9iia901jpqyb9qzkmqlwyc88n-libiconv-1.18/lib`.
Library crates build without it. CI on `ubuntu-latest` has system iconv and
needs no workaround, so this never affects the shipping artifact.

## T5 verification record (coordinator-run)

- R4a-R4f: `cargo test -p qfan-hist` → 9 passed. Clippy/fmt clean.
- Coordinator probe confirmed the properties that matter most:
  - unwritten slot → `None` (gap), evicted/colliding bucket → `None`, never 0°C or stale
  - bad magic → `Err("bad magic: expected QFRB, got [171,171,171,171]")`, not misread
  - retention: T1 = 60s x 20160 = 14 days; T2 = 3600s x 43800 = **1825 days**, meets R4's 1-year floor
  - T2 flash volume: 8760 writes/series/year, 27 series = 2.8 MB/year. Negligible, as DESIGN.md 6.2 claimed.
- `RingBuffer::window_max(n)` exists and is what §4.2's trailing 30s CPU-max window needs. T4 should use it rather than rolling its own.
- Committed 0632b4b.

## T2 verification record (coordinator-run)

- R9a-R9c, R3a: `cargo test -p qfan-hal` → 17 passed. Clippy/fmt clean.
- R6 lockout is **structural, not advisory**: `PwmChannel` and `FanChannel` enums
  cannot name pwm21 or fan22..35. This is the right implementation.
- `EcBackend` is object-safe; daemon holds `Box<dyn EcBackend>`.
- Rounding model verified against measured hardware: `round(180)=178`, `round(73)=71`,
  `round(255)=255`, and lossy for 250/256 inputs, so T4's change-detection guard
  will be meaningfully exercised.

### F9 — cross-crate safety gap found in review (fix dispatched)

Neither crate can see this alone. qfan-core's `validate()` enforces the 35%-duty
floor as `intended pwm >= 89`. The EC *delivers* `ec_pwm_round(intended)`.
Coordinator probe of the composition:

```
intended 89 -> delivered 86 (33% duty)   <-- validation accepts
intended 90 -> delivered 89 (34% duty)
intended 92 -> delivered 91 (35% duty)
```

A config that passes validation can deliver 33% duty above 80°C. The floor does
not survive hardware rounding. Fix in flight: qfan-hal gains
`ec_pwm_min_write_for(delivered_floor)` plus a verified 35% write-floor constant
(expected 92); coordinator then applies the matching change to qfan-core's
validation so the floor is expressed in *delivered* duty.

**Lesson for later tasks:** per-crate green tests cannot catch floor/rounding
composition errors. Any safety invariant stated in intended units must be
re-checked in delivered units at the crate boundary.

### F9 resolved (f5059e5)

`ec_pwm_min_write_for(delivered_floor)` is a `const fn`; `EC_DUTY_FLOOR_35PCT`
is computed at compile time and equals **90**, not the 92 I estimated. Coordinator
re-verified: write 90 delivers 89 (34% duty); write 89 delivers 86 (33%); one less
than the constant fails the floor, so it is minimal. The worker verified the value
instead of trusting my guess, which is the correct behaviour.

FakeBackend latency: default `latency_scale = 0.0`, no sleeping. Coordinator
measured 200 EC reads at default = **86 microseconds**. Only `ec_latency_realtime_r3a`
sets scale 1.0. T4's control-loop tests will not sleep. Resolved.

### A2 — coordinator TODO, blocked on T3

qfan-core's `validate()` still enforces the floor in INTENDED units (`pwm >= 89`).
It must enforce DELIVERED duty instead. qfan-core cannot depend on qfan-hal
(layering inversion), so the fix is to move `ec_pwm_round` + `ec_pwm_min_write_for`
down into qfan-core as pure arithmetic and re-export them from qfan-hal (which
already depends on qfan-core). **Serialized behind T3**: the T3 worker is editing
qfan-core right now, so applying this concurrently would conflict. Do it after T3
lands and before T4 consumes either crate.
