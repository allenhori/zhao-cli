//! Daily-rotating mirror of each `check`/`diff`/`lineage` run's own
//! stdout, under `target/zhao/logs/<YYYY-MM-DD>.log` -- see issue #35.
//! Always on, no flag needed, same "unconditional machine-readable
//! output" precedent as `target/zhao/run-metadata.json`. Scope for this
//! ticket: a literal mirror of what's already printed to stdout, never
//! anything different from it -- a separate `debug` verbosity level
//! (richer, internal-only content) is accepted/parsed via
//! [`zhao_core::config::LogLevel`] but produces no different log content
//! yet, reserved for a later ticket.

use std::io::Write;
use std::path::Path;

/// Appends `content` verbatim to
/// `<project_dir>/target/zhao/logs/<today>.log`, creating the directory
/// and the day's file as needed. Multiple runs the same calendar day
/// append to the same file; a run on a new day starts a new one. Never
/// changes what's actually printed to the real stdout -- this is purely
/// a side effect alongside it. A failure to write here is a warning, not
/// a hard error: the run's real output already succeeded regardless of
/// whether its mirror did.
pub fn mirror(project_dir: &Path, content: &str) {
    if let Err(err) = try_mirror(project_dir, content) {
        eprintln!("warning: could not write to run log: {err}");
    }
}

fn try_mirror(project_dir: &Path, content: &str) -> std::io::Result<()> {
    let dir = project_dir.join("target").join("zhao").join("logs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.log", today()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(content.as_bytes())
}

/// Today's date (local system clock), as `YYYY-MM-DD`. Computed by hand
/// from `SystemTime` rather than pulling in a date/time crate for the
/// one thing zhao's log rotation actually needs.
fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`: converts a day count since the
/// Unix epoch (1970-01-01) into a proleptic-Gregorian (year, month, day).
/// See <http://howardhinnant.github.io/date_algorithms.html>. A
/// well-known, correct algorithm -- not a hand-rolled approximation.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_722), (2023, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2024 is a leap year -- Feb 29 exists and Mar 1 follows it, not
        // Feb 30/skipping straight to Mar 1 from Feb 28.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }

    #[test]
    fn mirror_appends_across_multiple_calls_the_same_day() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        mirror(dir.path(), "first run\n");
        mirror(dir.path(), "second run\n");

        let log_path = dir
            .path()
            .join("target")
            .join("zhao")
            .join("logs")
            .join(format!("{}.log", today()));
        let content = std::fs::read_to_string(&log_path).expect("should read log file");
        assert_eq!(content, "first run\nsecond run\n");
    }

    #[test]
    fn mirror_never_touches_other_target_zhao_artifacts() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        std::fs::create_dir_all(dir.path().join("target").join("zhao"))
            .expect("should create target/zhao");
        std::fs::write(
            dir.path()
                .join("target")
                .join("zhao")
                .join("run-metadata.json"),
            "{}",
        )
        .expect("should write sentinel file");

        mirror(dir.path(), "a run\n");

        let sentinel = dir
            .path()
            .join("target")
            .join("zhao")
            .join("run-metadata.json");
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("should still exist"),
            "{}"
        );
    }
}
