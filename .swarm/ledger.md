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
| T3 | M2 qfan-core: curves, slew, hysteresis, overrides | R1, R5 (validation) | T1 | sonnet-4-6 | DONE: all 7 review findings fixed, mutation-verified |
| T4 | M3 control loop + failsafe + dry-run | R1, R2, R3 | T2, T3 | sonnet-4-6 | pending |
| T5 | M4 qfan-hist | R4 | T1 | sonnet-4-6 | DONE, coord-verified (0632b4b) |
| T6 | M5 socket API + status subcommand | R5 | T4, T5 | sonnet-4-6 | pending |
| T7 | M6 ratatui TUI | R6 | T6 | sonnet-4-6 | pending |
| T8 | M7 plugin packaging | R7, R2 (watchdog) | T4 | sonnet-4-6 | pending |
| T9 | M8 release pipeline + M7 packaging | R7, R8, R2 | T1 | sonnet-4-6 | DONE, coord-verified (729fbc5) |
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

## T3 verification record (coordinator-run) — commit 602af33

14 tests pass (7 originals intact). Coordinator probes of the safety properties:

```
60C w/ override 90:  pwm=90  mode=Override{expires_at_secs:1600}
95C w/ override 90:  pwm=255 mode=Emergency      <- latch beats override
85C after latch:     pwm=255 mode=Emergency      <- no premature release
sensor failure:      pwm=255 mode=FailSafe
ramp up 90->255:     1 cycle
ramp down 255->90:   9 cycles
50C->51C in 3C band: pwm=129 == curve@51         <- hysteresis never damps a rise
ttl=0 / ttl=3601 / ttl=u32::MAX -> Err(InvalidTtl), not clamped
```

Hysteresis filters the TEMPERATURE SIGNAL, not the PWM output, so the ramp-up
path never executes deadband logic. Verified against the code, not just the comment.

### A2 applied (closes F9 at source)

`ec_pwm_round` / `ec_pwm_min_write_for` / `DELIVERED_DUTY_FLOOR` /
`EC_DUTY_FLOOR_35PCT` moved into qfan-core as pure arithmetic; qfan-hal
re-exports them. `validate()` now compares DELIVERED pwm:

```
pwm7: curve produces pwm 89 at 80°C, which the EC delivers as 86
(< 89, below the 35% duty minimum). Use at least 90.
```

Note: the defaults already used 90, so they were safe by accident, not enforcement.

## T9 verification record (coordinator-run)

- Both workflows are valid YAML (ruby psych): release.yml on `push.tags=[v*]`,
  1 job / 8 steps; ci.yml 1 job / 7 steps, both ubuntu-latest.
- R8b artifact-name agreement **verified**: workflow emits
  `qnap-fan-${VERSION}.txz` + `.txz.sha256`; .plg entities `&PACKAGE;` /
  `&SHA256FILE;` derive the identical names from `&name;-&version;`. No mismatch.
- `bash -n` passes on rc.qfand and both extracted `<INLINE>` blocks.
- Uninstall path sets pwm1/pwm7 to 255, skips pwm21, and preserves flash config.
- Boot start confirmed: Unraid re-runs each `/boot/config/plugins/*.plg` at boot,
  and the install block calls `rc.qfand start`, so no `/boot/config/go` edit is needed.

### R2a watchdog — EXERCISED, not just read

Simulated against a fake hwmon tree in TMPDIR with a real `kill -9`:

```
pwm before:            pwm1=90  pwm7=90
(SIGKILL the daemon)
pwm after SIGKILL:     pwm1=255 pwm7=255
notify log:            "NOTIFY: qfand pid N exited; fans to 100%"
```

Clean-stop path separately verified to NOT false-alarm: `stop_watchdog` runs
before the daemon is signalled, fans stay at 90, notify log empty. `local` inside
the backgrounded subshell works correctly.

### F10 — CRITICAL, fix dispatched

`plugin/qnap-fan.plg` is **not well-formed XML**:

```
$ xmllint --noout plugin/qnap-fan.plg
plugin/qnap-fan.plg:65: parser error : xmlParseEntityRef: no name
(cd "$PLUGDIR" && sha256sum -c "&SHA256FILE;") || {
...also 66, 67, 126
```

Bare `&&` and `>&2` inside `<INLINE>` are entity-reference starts. Unraid parses
.plg with a real XML parser, so the plugin would be rejected at install.
Precedent pulled from the NAS: the shipping `qnap-ec.plg` IS well-formed, uses no
CDATA, and escapes as `2>&amp;1`. Fix follows that house style; CDATA is
explicitly forbidden because it would stop `&PACKAGE;` expanding.

### R7b blocked

`php` is not installed on this Mac, so `php -l plugin/qnap-fan.page` cannot run.
Recorded as BLOCKED, not PASS. Options: lint on the NAS (php 8.4.23 present) or
in CI.

## Defect pattern (3 of 3 so far)

Every defect found has been green under its own tooling and broken at a boundary
that tooling could not see:

- F8 clamp region: unit tests green, curve evaluation unchecked above last breakpoint.
- F9 intended-vs-delivered: both crates green, composition unsafe.
- F10 XML vs shell: `bash -n` green, XML parser rejects the same bytes.

**Rule for remaining tasks:** verify at the boundary, in the units and the parser
that production actually uses.

## Opus 4.6 independent review of 602af33 — full text at review-findings.md

Reviewer confirmed both prior fixes (clamp sweep, delivered-unit floor) are
genuine and complete, then found 7 new findings. Coordinator independently
reproduced the two criticals and E3:

```
A1: override 30 @85C -> pwm=30  mode=Override  (curve would be 255) = 10% delivered duty
A2: while latched @95C -> pwm=255 mode=Emergency
A2: after release @85C -> pwm=50 mode=Override  (curve would be 255)
E3: pwm_at(50) with empty breakpoints = 0
```

| # | Sev | Finding | Status |
|---|---|---|---|
| A1 | CRITICAL | `Override::new` has no duty floor; `decide()` returns it verbatim. 80–89°C is unprotected because the latch has not engaged yet | reproduced |
| A2 | CRITICAL | Override set while latched survives latch release into a hot state | reproduced |
| G1 | HIGH | `Override::new` does `now_secs + ttl_s`; debug build panics and aborts the daemon | confirmed by reviewer probe |
| B1 | MEDIUM | `decide()` reads `is_latched()` but nothing enforces that `update()` ran first; cold start at 95°C reports mode Curve, so no notification | confirmed |
| E3 | MEDIUM | `pwm_at()` returns 0 for empty breakpoints and is `pub` | reproduced |
| F2 | MEDIUM | Sweep starts at 80°C; `[{79,10},{80,90}]` is legal = 4% duty at 79°C | SUSPECTED, deferred to Q5 |
| D2 | LOW | `slew_down_max == 0` blocks all ramp-down (errs safe) | confirmed |

### Why my own earlier probe missed A1 (lesson)

My T3 verification tested an override of **90** and saw the latch correctly beat
it at 95°C, so I recorded overrides as safe. The hole is 80–89°C with a
*sub-floor* override value. **Testing a safe value and generalising from it is not
verification.** Probe the adversarial value, not the plausible one.

### Test-quality finding — the most valuable part of the review

Three existing tests would still pass if the property they claim to check were
broken:

- `r5b_override_expiry` — uses pwm 200, tests timing only, never a dangerous value
- `r5a_override_ttl_validation` — never checks pwm bounds at all
- `r1d_emergency_latch` — sets an override while latched but never exercises the
  *release* path with a low override still active

Green tests that cannot fail for the right reason. All three being strengthened
alongside the fixes.

## T9 verification completed

- R7b **PASS** (was BLOCKED): `php -l` run on the NAS itself with php 8.4.23 —
  the same interpreter that will serve the page. `No syntax errors detected`.
- Required extensions present on the NAS: `sockets`, `json`.
- Page audited against the R4 risk: **no** sysfs paths, **no** `exec`/`shell_exec`/
  `system`/`popen`. Pure socket I/O, 2s timeout, honest "daemon not running" state.
  Channel allowlist is `['pwm1','pwm7']`, so pwm21 is unreachable from the web UI.
- F10 fix verified: `xmllint --noout` exits 0, entity refs intact, and both
  `<INLINE>` blocks still pass `bash -n` after un-escaping `&amp;` back to `&`.

## Q5 — open decision for the user (F2, the 79°C cliff)

Sweep starts at 80°C, so a curve may legally deliver ~4% duty at 79°C. With the
20°C-in-seconds transients in the crash logs, exposure is ~2-4s. Reviewer ruled
out monotonic-PWM (the cliff is non-decreasing, so it would not help). Remaining
options: lower the sweep floor to 75°C (rejects legitimate quiet-below-75 configs)
or add a gradient check rejecting >100 PWM change across 1°C. Coordinator leans
gradient check. Requires a deliberately adversarial config to trigger.

## Safety fixes applied — all 7 review findings closed

20 tests (14 original + 6 named for the findings). Coordinator re-probed every
reproduction case against the fixed code:

```
A1: Override::new(30,..) = Err(PwmBelowFloor { pwm: 30, min: 90 })
    floor 90 accepted, 89 rejected
A2: @95C -> pwm=255 Emergency override_cleared=true
    @79C release -> Curve;  @85C after release -> pwm=255 Curve (NOT the stale 100)
G1: no panic at u64::MAX-100
E3: pwm_at(50) with empty breakpoints = 255 (fails safe)
B1: cold-start @95C -> pwm=255 mode=Emergency
D2: slew_down_max=0 -> validate() rejects
RAMP: 90 -> 255 in one cycle at 85C   <- ramp-up still immediate after all fixes
```

B1 was fixed structurally, not by convention: `decide()` now takes
`&mut EmergencyLatch` plus the thresholds and calls `update()` itself, so a caller
cannot evaluate a stale latch. It also returns `override_cleared` so the daemon
can log the A2 clear.

### MUTATION TESTING — the tests now fail for the right reasons

The review's sharpest finding was that three tests would pass even if the property
they claimed to check were broken. Fixing that claim required proving it, so each
safety property was deliberately broken in the source and the suite re-run:

| Mutation | Result |
|---|---|
| delete the A1 override duty-floor check | CAUGHT |
| delete the A2 override-clear on latch engage | CAUGHT |
| `saturating_add` -> `wrapping_add` at the G1 code site | CAUGHT |
| empty-breakpoint return 255 -> 0 | CAUGHT |
| hysteresis moved onto the PWM output instead of the temp signal | CAUGHT |
| slew-limit applied to ramp UP as well | CAUGHT |

7 of 7 caught (one earlier attempt was an invalid mutation that did not compile).

**Two coordinator harness errors worth recording**, both of which initially
reported a false "SURVIVED":
1. Grepping for `FAILED` lines instead of checking the process exit code. A test
   binary that ABORTS (`SIGABRT`, returncode 101) prints no `FAILED` line at all.
   Check the exit code, not the log text.
2. `str.replace(old, new, 1)` hit a *comment* mentioning `saturating_add` on line
   395 rather than the code on line 397, so the "mutation" was a no-op and the
   survival was meaningless. Anchor mutations on unique code text and assert the
   mutated file actually differs in the intended place.

Both are the same class as the F8/F9/F10 defects: the tool reported green because
it was not measuring what it claimed to measure.

## Full gate after fixes

```
qfan-core 20 passed | qfan-hal 20 passed | qfan-hist 9 passed
clippy --all-targets -D warnings: 0 errors
cargo fmt --check: OK
cargo build -p qfan-hal: Finished (re-exports intact)
```

## Q5 RESOLVED (user: "Lower to 75") — F2 closed

`SWEEP_FLOOR_C: i16 = 75` added next to the other duty-floor constants;
`validate()` now sweeps `SWEEP_FLOOR_C..=110`.

Checked against the defaults BEFORE changing the bound, so the decision was not
made blind:

```
pwm1: 75C..81C -> intended 180 / delivered 178   (floor is 89)
pwm7: 75C..81C -> intended 255 / delivered 255
```

Both are far above the floor, so lowering the sweep rejects nothing legitimate.

Two tests added:
- `f2_sub_80c_cliff_rejected` — the original F2 config `[{79,10},{80,90}]`, plus
  every degree in 75..=79. **Reverting SWEEP_FLOOR_C to 80 makes this test fail**
  (verified by mutation), so it measures the constant instead of restating it.
- `f2_quiet_below_sweep_floor_still_allowed` — a curve quiet below 75 °C stays
  legal and `pwm_at(60)` is preserved as written. Guards against the fix
  over-rejecting.

Note on the error message: the sweep reports the FIRST offending degree, which is
now 75 °C rather than 79 °C, because the clamp region below the 79 °C breakpoint
holds pwm 10 all the way down. My first assertion asserted on "79" and failed;
the assertion was wrong, not the code.

## Q6 RESOLVED (user: "create the remote using the gh cli")

`gh repo create qnap-fan --private --source=. --remote=origin` →
https://github.com/Krable55/qnap-fan (PRIVATE — flip to public when ready to
publish, since the .plg download URLs must be publicly reachable for Unraid to
install from them).

`.plg` entity `&github;` now `Krable55/qnap-fan`. Resolved URLs verified by
expanding the entity graph:

```
https://github.com/Krable55/qnap-fan/releases/download/v2026.11.24/qnap-fan-2026.11.24.txz
https://github.com/Krable55/qnap-fan/releases/download/v2026.11.24/qnap-fan-2026.11.24.txz.sha256
```

These match the names the release workflow publishes (`qnap-fan-${VERSION}.txz`
+ `.sha256`). No `OWNER` placeholder remains anywhere in .github/, plugin/, or
DESIGN.md.

### CI verified on real infrastructure — run 45544351922, conclusion success

```
✓ Check formatting
✓ Clippy (all targets, deny warnings)
✓ Tests
✓ Build musl release (native on ubuntu-latest x86_64, no cross-linker needed)
```

This is the first evidence the build works anywhere other than this Mac, and it
confirms D6/R2: the musl target builds natively on a GH runner, so the aarch64
cross-linker problem never has to be solved.

**Known benign warnings** (NOT suppressed): `unused dependency qfan-hal`,
`qfan-hist`, `qfan-proto` in `crates/qfand/Cargo.toml`. `qfand` is still the M0
stub; T4 wires these up. Left visible on purpose — they are an accurate statement
that the daemon is not yet implemented. Note `-D warnings` does not catch these
because cargo manifest warnings are not lints.

## Stray-file hygiene

`crates/qfan-core/tests/coord_verify.rs` (a coordinator probe) had survived into
the working tree and was caught by `git status` before the commit. Probe files
must be deleted in the same call that runs them, or they ship.

## T4 / T6 dispatched in parallel — handoff contract written by coordinator first

`crates/qfand/src/state.rs` (commit 1729ed3) is the ONLY shared surface. Written
by the coordinator rather than negotiated between two workers editing one file.
The split is what enforces R1 structurally:

- control loop = sole PWM writer, sole caller of `qfan_core::decide`
- socket server = reads snapshots, queues `state::Command` for the loop

`ChannelSnapshot` keeps `intended_pwm` and `readback_pwm` as separate fields with
the F7 warning in the type docs. `publish()` takes a closure so the critical
section cannot accidentally wrap a 12-18 ms EC read (F6).

## T6 (socket API) — coordinator review COMPLETE

PASS so far:
- **R1 boundary respected.** `grep -E "decide\(|write_pwm|qfan_hal|/sys/"` on
  ipc.rs returns only a comment stating it does none of those. No EC access.
- Stale-socket unlink before bind, `chmod 0o660`, 30 s read timeout,
  `MAX_LINE_BYTES = 65_536` cap, per-client `tokio::spawn` so a silent client
  cannot stall others.
- **Override delegates to `qfan_core::Override::new`** and surfaces its error
  verbatim instead of reimplementing the duty floor. This is the F9 lesson
  applied by a worker that never saw F9 — the brief carried it.
- **Gap-vs-zero correct**: `None` slot → `NO_DATA` (i16::MIN) AND `count: 0`, so
  clients get two independent signals that downtime is not 0 °C.

### F11 — silent success on an unknown channel (MEDIUM)

`handle_set_curve` validates the channel against an allowlist and returns
`unknown channel ...; use pwm1 or pwm7`. But `handle_override` and
`handle_clear_override` do NOT: any channel string is accepted and returns
`Ack::Ok`. The control loop then silently ignores it:

```rust
Command::SetOverride { channel, ov } => {
    if channel == "pwm7" { ... } else if channel == "pwm1" { ... }
    // no else — dropped silently
}
```

Not a thermal hole: R6 remains structurally safe because qfan-hal's enums cannot
name pwm21, and no write occurs. But a client that POSTs
`{"op":"override","channel":"pwm21",...}` is told OK while nothing happens. For a
thermally dangerous operation, reporting success for a no-op is wrong. Fix: apply
the same allowlist in the override handlers, and make the control loop log
instead of silently dropping.

## Compile state note (not a defect)

While T4 was still editing, `cargo test -p qfand` failed with 3 errors, ALL in
T4-owned files: `DeciUnits is private` (control.rs:472) and `module sync is
private` ×2 (main.rs:91, control.rs:439 — the `tokio` `sync` feature is not
enabled in qfand's Cargo.toml). Attributed to the in-flight worker, not to T6,
and deliberately NOT fixed by the coordinator mid-edit.

## T4 (control loop) + T6 (socket API) — both verified, M3/M5 ready

26 tests in qfand (10 control + 16 ipc), 1 conditionally ignored. Full workspace:
qfan-core 22, qfan-hist 9, qfand 26; clippy -D warnings clean; fmt clean.

### Mutation-verified safety properties

| Mutation | Result |
|---|---|
| F7: compare against EC read-back instead of last-intended | CAUGHT |
| trailing max: `window_max(TRAILING_MAX_SLOTS)` → `window_max(1)` | CAUGHT |
| F11: remove channel guard from `handle_override` | CAUGHT |

The F7 mutation is the important one: swapping the comparison to a sysfs
read-back is exactly the permanent-rewrite treadmill, and the test fails when it
is introduced. `maybe_write` compares `intended == *last` where `last` is
`last_written_pwm7` — the intended value, never a read-back.

**Coordinator harness error repeated (3rd time).** `replace(old,new,1)` hit a
COMMENT containing `window_max` on line 247 instead of the call on line 278, so
the mutation was a no-op and reported SURVIVED. Anchoring on the unique call text
`.window_max(TRAILING_MAX_SLOTS)` caught it immediately. **This failure mode is
now predictable: always anchor mutations on text unique to the code path, and
treat an unexpected SURVIVED as harness-suspect before code-suspect.**

### R1/R6 verified structurally

- `decide()` is called in exactly two places, both in `control.rs::tick`, one per
  channel. `ipc.rs` never calls it (only a comment stating so).
- No `pwm21`/`fan22-35` reference anywhere in qfand except the F11 test strings
  that assert they are REJECTED.

### F12 — worker reported clippy clean; it was not (LOW, fixed)

`cargo clippy -p qfand --all-targets -- -D warnings` failed with 2 real errors:
`manual_is_multiple_of` at control.rs:256 and :270. The worker's self-report
claimed all three gates passed. Fixed by the coordinator
(`self.tick.is_multiple_of(self.fan_every_ticks)`). **Self-reported gate results
are not evidence; re-run every gate.**

### R2 panic-unwind test was unconditionally #[ignore]d — now macOS-only

The worker marked `failsafe_guard_panic_unwind_writes_255` as `#[ignore]` with an
honest diagnosis: `panic!()` aborts on this Darwin 25 + Nix toolchain
(`fatal runtime error: failed to initiate panic, error 5`). Coordinator confirmed
the abort independently — the diagnosis is correct, and the same quirk hit the
earlier qfan-core mutation runs.

But an unconditional `#[ignore]` means the test never runs ANYWHERE, including
Linux, which is the deployment target. Changed to:

```rust
#[cfg_attr(target_os = "macos", ignore = "...; runs in CI on Linux")]
```

CI runs ubuntu-latest, so the unwind path is now exercised on every push. R2 is
the reason this daemon exists; leaving its most critical test permanently
unexecuted and substituting an argument from Rust's Drop guarantee is not
acceptable when a real platform can run it.

### Dependency changes reviewed

`io-util` added to the workspace tokio features and `serde_json` to qfand — both
genuinely required. `tempfile` is dev-only. Notably the `sync` feature was NOT
added; the earlier `module sync is private` error was resolved properly rather
than by broadening features.

## T7 (ratatui TUI) — verified, M6 complete

42 tests in qfand (was 26), 2 conditionally ignored on macOS only. Workspace:
qfan-core 22, qfan-hist 9, qfand 42 = 73. clippy -D warnings clean, fmt clean,
`cargo build --release -p qfand` succeeds.

Worker got the things that have bitten this project before, unprompted by history
(the brief carried the lessons, not the incident list):
- `#[cfg_attr(target_os = "macos", ignore = ...)]` on the panic-unwind test, NOT
  plain `#[ignore]` — the F12/R2 lesson applied pre-emptively.
- Curve editor defers ALL validation to the daemon and shows its rejection
  verbatim; no client-side pre-validation that could disagree with the authority.
- R6: the only mention of pwm21/fan22-35 in tui.rs is a comment saying they are
  never shown.
- Terminal restore via BOTH `install_panic_hook` (chains to the previous hook) and
  an RAII `TermGuard`; `restore_terminal()` is idempotent.
- 1 Hz poll / 100 ms render / 3 s socket timeout. Reconnect is implicit: each poll
  reconnects, so a daemon restart self-heals without special-case code.

### F13 — gap tests proved the helper, not the screen (fixed by coordinator)

All six of the worker's gap tests exercised `history_to_segments`, a pure
function. Good logic coverage, but nothing asserted a gap never reaches the
rendered output — `history_with_gaps_renders_without_panic` only checked for
absence of panic. A renderer could plot the sentinel independently of the helper
and every test would still pass.

Added `gaps_never_reach_the_rendered_buffer`: renders a real outage
(valid, valid, NO_DATA, NO_DATA, count=0, valid, valid) to a 120x40 TestBackend,
asserts the buffer contains no "-32768"/"-3276.8"/"32768", and asserts the
plotted points exclude buckets 2..4 and are all > 0 °C.

### F14 — the sentinel branch of `is_gap` was never tested (fixed by coordinator)

Mutation testing of `is_gap`:

| Mutation | Before fix | After fix |
|---|---|---|
| drop `count == 0` signal | CAUGHT | CAUGHT |
| **drop the NO_DATA sentinel signal** | **SURVIVED** | CAUGHT |
| `is_gap` always false | CAUGHT | CAUGHT |

Root cause: EVERY `no_data_slot` fixture also set `count: 0`, so no test could
distinguish the two branches. Deleting the entire sentinel check left all 41
tests green. The daemon does currently write both together — but `is_gap`
promises to catch either signal independently, and an untested branch of a
safety-display rule is exactly the F8/F9 pattern: the invariant held by accident
of the fixtures, not by enforcement.

Added `sentinel_only_slot(bucket)` with `count: 7` plus
`sentinel_alone_is_a_gap_without_count_zero`. All three mutations now caught.

### Known limitation (accepted)

The terminal-restore tests use a local `MockGuard` with the same Drop shape
rather than the real `TermGuard`, because the real guard needs a live terminal.
Drop semantics are verified; the actual escape sequences are not. Will be
exercised for real during the M9 SSH session on the NAS.
