//! Persisting curve changes back to `config.toml`.
//!
//! A curve accepted over the socket used to live only in memory, so the next
//! daemon restart silently reverted it to whatever the file said. This module
//! writes the accepted breakpoints into the file the daemon loads at startup.
//!
//! The file is hand-edited and heavily commented, so it is edited in place with
//! `toml_edit` rather than re-serialised: only `[<channel>].breakpoints` is
//! replaced, and every other key and comment survives.

use anyhow::{bail, Context, Result};
use qfan_core::{ChannelConfig, Config};
use std::io::Write;
use std::path::Path;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// Write `ch.breakpoints` into `[<channel>]` of the config file at `path`.
///
/// - Existing `[<channel>]` table: only its `breakpoints` are replaced.
/// - No `[<channel>]` table (the file relied on compiled-in defaults): the whole
///   channel config is written, because a table holding only `breakpoints`
///   would fail to load (`source`, `slew_down_max` and `hysteresis_c` have no
///   per-field defaults).
/// - No file at all: one is created.
///
/// Before anything touches disk, the edited text is loaded back exactly as the
/// daemon would load it at startup, and must validate and produce these exact
/// breakpoints. A file the next start would reject or misread is never written.
/// The write itself is atomic (temp file + rename), so a power cut mid-write
/// leaves the old file, not a truncated one.
pub fn save_curve(path: &Path, channel: &str, ch: &ChannelConfig) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let out = with_curve(&text, channel, ch)?;
    write_atomic(path, &out)
}

/// Pure half of [`save_curve`]: return `text` with the channel's curve replaced.
fn with_curve(text: &str, channel: &str, ch: &ChannelConfig) -> Result<String> {
    let mut doc: DocumentMut = text
        .parse()
        .context("config.toml is not valid TOML; fix it by hand first")?;

    match doc.get_mut(channel) {
        Some(item) => {
            let table = item
                .as_table_mut()
                .with_context(|| format!("[{channel}] in config.toml is not a table"))?;
            table.insert("breakpoints", Item::ArrayOfTables(breakpoints_item(ch)));
        }
        None => {
            let mut table = channel_table(ch)?;
            table.insert("breakpoints", Item::ArrayOfTables(breakpoints_item(ch)));
            doc.insert(channel, Item::Table(table));
        }
    }
    let out = doc.to_string();

    // Self-check: the daemon's own loader must accept the result and read back
    // exactly what is about to start running.
    let reloaded: Config = toml::from_str(&out).context("edited config.toml would not load")?;
    reloaded
        .validate()
        .context("config.toml has a problem outside this curve; fix it by hand first")?;
    let got = match channel {
        "pwm7" => &reloaded.pwm7.breakpoints,
        "pwm1" => &reloaded.pwm1.breakpoints,
        _ => bail!("unknown channel {channel:?}"),
    };
    if *got != ch.breakpoints {
        bail!("edited config.toml reads back a different {channel} curve; not saved");
    }
    Ok(out)
}

fn breakpoints_item(ch: &ChannelConfig) -> ArrayOfTables {
    let mut arr = ArrayOfTables::new();
    for p in &ch.breakpoints {
        let mut t = Table::new();
        t.insert("temp_c", value(i64::from(p.temp_c)));
        t.insert("pwm", value(i64::from(p.pwm)));
        arr.push(t);
    }
    arr
}

/// The channel's non-breakpoint settings as a TOML table, via the same serde
/// representation the loader reads, so enum spellings like `source` match.
fn channel_table(ch: &ChannelConfig) -> Result<Table> {
    let mut bare = ch.clone();
    bare.breakpoints.clear();
    let doc: DocumentMut = toml::to_string(&bare)
        .context("serialise channel config")?
        .parse()
        .context("re-parse channel config")?;
    let mut table = doc.as_table().clone();
    table.remove("breakpoints");
    table.set_implicit(false);
    Ok(table)
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(contents.as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} over {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfan_core::CurvePoint;

    const SHIPPED: &str = include_str!("../../../plugin/config.default.toml");

    fn with_points(base: &ChannelConfig, pts: &[(i16, u8)]) -> ChannelConfig {
        ChannelConfig {
            breakpoints: pts
                .iter()
                .map(|&(temp_c, pwm)| CurvePoint { temp_c, pwm })
                .collect(),
            ..base.clone()
        }
    }

    /// The shipped file is the one users actually have. Saving a curve must
    /// change only that curve and keep the explanatory comments.
    #[test]
    fn shipped_config_keeps_comments_and_other_channel() {
        let d = Config::default();
        let ch = with_points(&d.pwm7, &[(50, 90), (60, 150), (72, 255)]);
        let out = with_curve(SHIPPED, "pwm7", &ch).unwrap();

        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.pwm7.breakpoints, ch.breakpoints);
        assert_eq!(cfg.pwm1.breakpoints, d.pwm1.breakpoints, "pwm1 untouched");
        assert_eq!(cfg.pwm7.slew_down_max, d.pwm7.slew_down_max);
        assert!(
            out.contains("# EC fan RPM reads are expensive"),
            "comments kept"
        );
        assert!(out.contains("# Deadband in °C"), "channel comments kept");
    }

    /// A file that leaves a channel to the compiled-in defaults has no table
    /// for it. Writing only `breakpoints` there would make the next start fail.
    #[test]
    fn missing_channel_table_gets_full_channel_config() {
        let d = Config::default();
        let ch = with_points(&d.pwm1, &[(30, 90), (55, 200)]);
        let out = with_curve("poll_coretemp_s = 2\n", "pwm1", &ch).unwrap();

        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.pwm1.breakpoints, ch.breakpoints);
        assert_eq!(
            format!("{:?}", cfg.pwm1.source),
            format!("{:?}", d.pwm1.source)
        );
        assert_eq!(cfg.pwm1.hysteresis_c, d.pwm1.hysteresis_c);
    }

    #[test]
    fn missing_file_is_created_and_loads() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let d = Config::default();
        let ch = with_points(&d.pwm7, &[(40, 100), (70, 255)]);

        save_curve(&path, "pwm7", &ch).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.pwm7.breakpoints, ch.breakpoints);
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "temp file renamed away"
        );
    }

    /// A file the daemon would reject on its next start must never be written,
    /// even when the curve itself is fine: the restart would fall back to
    /// defaults and silently undo every saved curve.
    #[test]
    fn unrelated_invalid_setting_blocks_the_save() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let bad =
            format!("{SHIPPED}\n").replace("emergency_release_c = 80", "emergency_release_c = 95");
        std::fs::write(&path, &bad).unwrap();
        let ch = with_points(&Config::default().pwm7, &[(40, 100), (70, 255)]);

        let err = save_curve(&path, "pwm7", &ch).unwrap_err();

        assert!(
            format!("{err:#}").contains("emergency_release_c"),
            "{err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bad,
            "file untouched"
        );
    }

    #[test]
    fn syntax_error_blocks_the_save() {
        let ch = with_points(&Config::default().pwm7, &[(40, 100), (70, 255)]);
        let err = with_curve("[pwm7\n", "pwm7", &ch).unwrap_err();
        assert!(format!("{err:#}").contains("not valid TOML"), "{err:#}");
    }
}
