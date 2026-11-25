//! The config file shipped in the plugin package must parse and validate.
//!
//! This guards a failure mode no unit test covers: `config.default.toml` is
//! copied verbatim into the `.txz` by the release workflow, so if it drifts out
//! of step with the schema or trips a safety rule, the daemon refuses to start
//! on a freshly installed plugin — on a NAS whose fans the daemon is supposed to
//! be controlling. Validation lives in the library; the shipped file is data,
//! and data is exactly what unit tests do not exercise.

use qfan_core::Config;

const SHIPPED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../plugin/config.default.toml"
);

#[test]
fn shipped_default_config_parses_and_validates() {
    let text = std::fs::read_to_string(SHIPPED).expect("plugin/config.default.toml must exist");

    let cfg: Config = toml::from_str(&text).unwrap_or_else(|e| {
        panic!("shipped config does not parse against the current schema: {e}")
    });

    if let Err(e) = cfg.validate() {
        panic!("shipped config fails validation — a fresh install would not start: {e}");
    }
}

/// The shipped curves must clear the duty floor across the whole validated
/// sweep, not merely satisfy `validate()` today. If the floor or the sweep range
/// is ever tightened, this says so in delivered units rather than failing with a
/// generic validation message.
#[test]
fn shipped_curves_clear_the_duty_floor_in_delivered_units() {
    let text = std::fs::read_to_string(SHIPPED).expect("plugin/config.default.toml must exist");
    let cfg: Config = toml::from_str(&text).expect("shipped config must parse");

    for (name, ch) in [("pwm1", &cfg.pwm1), ("pwm7", &cfg.pwm7)] {
        for t in qfan_core::SWEEP_FLOOR_C..=110 {
            let intended = ch.pwm_at(t);
            let delivered = qfan_core::ec_pwm_round(intended);
            assert!(
                delivered >= qfan_core::DELIVERED_DUTY_FLOOR,
                "{name} at {t}°C: intended {intended} delivers {delivered}, \
                 below the {} floor",
                qfan_core::DELIVERED_DUTY_FLOOR
            );
        }
    }
}

/// The emergency threshold must sit above the sweep floor and the release point
/// below it, or the latch could engage inside a band the curve validator never
/// checked.
#[test]
fn shipped_emergency_thresholds_are_ordered_sanely() {
    let text = std::fs::read_to_string(SHIPPED).expect("plugin/config.default.toml must exist");
    let cfg: Config = toml::from_str(&text).expect("shipped config must parse");

    assert!(
        cfg.emergency_release_c < cfg.emergency_temp_c,
        "release ({}) must be below engage ({}) or the latch oscillates",
        cfg.emergency_release_c,
        cfg.emergency_temp_c
    );
    assert!(
        cfg.emergency_temp_c > qfan_core::SWEEP_FLOOR_C,
        "emergency ({}) at or below the sweep floor ({}) would make the curve \
         validator's range meaningless",
        cfg.emergency_temp_c,
        qfan_core::SWEEP_FLOOR_C
    );
}
