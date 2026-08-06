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

/// Formats a `dbt compile`/`dbt deps` subprocess's captured stdout/stderr
/// for the run log -- see issue #36. Shared by both the success path
/// (routing output that used to be silently discarded) and the failure
/// path (a `DbtAdapterError::CompileFailed`/`DepsFailed`'s own
/// `stdout`/`stderr` fields), so a compile/deps entry always looks the
/// same in the log regardless of which one produced it. `command` is
/// the dbt subcommand run (`"compile"` or `"deps"`); `dbt_project_dir`
/// is where it ran -- for git-native Baseline resolution this is the
/// temporary worktree's path, not the real project directory the log
/// itself lives under, so it's recorded here for context even though
/// the worktree itself is gone by the time anyone reads the log.
pub fn format_dbt_output(
    command: &str,
    dbt_project_dir: &Path,
    stdout: &str,
    stderr: &str,
) -> String {
    format!(
        "$ dbt {command} (in {})\n{stdout}\n{stderr}\n",
        dbt_project_dir.display()
    )
}

/// Routes a `dbt compile`/`dbt deps` call's captured stdout/stderr into
/// `real_project_dir`'s daily run log -- both a successful run's output
/// (previously discarded entirely) and a failing one's (already
/// captured on the error for #30's terminal error message; this adds
/// the same content to the log for post-hoc inspection too) -- then
/// passes the `Result` straight through unchanged. Shared by every
/// call site that routes a `dbt compile`/`dbt deps` invocation's output
/// this way, whether or not it ran in a temporary git worktree (see
/// issue #36).
///
/// `dbt_project_dir` is where the subcommand actually ran -- for
/// git-native Baseline resolution this is the temporary worktree's
/// path, gone by the time anyone reads the log (recorded in the entry
/// for context regardless); for a direct `--compile` invocation it's
/// the same as `real_project_dir`. `real_project_dir` is where the log
/// itself lives, same as every other `target/zhao/` artifact.
pub fn log_dbt_result(
    command: &str,
    dbt_project_dir: &Path,
    real_project_dir: &Path,
    result: Result<
        zhao_core::adapters::dbt::DbtCommandOutput,
        zhao_core::adapters::dbt::DbtAdapterError,
    >,
) -> Result<zhao_core::adapters::dbt::DbtCommandOutput, zhao_core::adapters::dbt::DbtAdapterError> {
    use zhao_core::adapters::dbt::DbtAdapterError;

    let captured = match &result {
        Ok(output) => Some((output.stdout.as_str(), output.stderr.as_str())),
        Err(DbtAdapterError::CompileFailed { stdout, stderr, .. })
        | Err(DbtAdapterError::DepsFailed { stdout, stderr, .. }) => {
            Some((stdout.as_str(), stderr.as_str()))
        }
        // No captured output to log for anything else (e.g. the `dbt`
        // command couldn't even be spawned).
        Err(_) => None,
    };
    if let Some((stdout, stderr)) = captured {
        mirror(
            real_project_dir,
            &format_dbt_output(command, dbt_project_dir, stdout, stderr),
        );
    }
    result
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

/// Purges `target/zhao/logs/<date>.log` files older than
/// `retention_days` days before today -- see issue #37. A no-op if
/// `retention_days` is `None` (the default): with nothing configured,
/// no purging happens, matching the assumption that most environments
/// running zhao are disposable anyway. Only ever removes files under
/// `target/zhao/logs/` whose name matches the `<YYYY-MM-DD>.log`
/// pattern this module itself writes -- anything else in that
/// directory, or anywhere else under `target/zhao/`, is left alone. A
/// failure here is a warning, not a hard error, same as [`mirror`].
pub fn purge(project_dir: &Path, retention_days: Option<u32>) {
    let Some(retention_days) = retention_days else {
        return;
    };
    if let Err(err) = try_purge(project_dir, retention_days) {
        eprintln!("warning: could not purge old run logs: {err}");
    }
}

fn try_purge(project_dir: &Path, retention_days: u32) -> std::io::Result<()> {
    let dir = project_dir.join("target").join("zhao").join("logs");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // Nothing to purge if the directory doesn't exist yet (e.g. no
        // run has ever written to it) -- not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    let cutoff = today_days() - i64::from(retention_days);
    for entry in entries {
        let path = entry?.path();
        if let Some(file_days) = log_filename_days(&path) {
            if file_days < cutoff {
                // Best-effort per file -- one file's removal failing
                // (e.g. a permissions issue on that specific file)
                // shouldn't stop the rest from being purged.
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// Parses a `target/zhao/logs/<YYYY-MM-DD>.log` path's filename back
/// into a day count since the Unix epoch, via [`days_from_civil`].
/// `None` for anything that doesn't match that exact shape (a
/// non-`.log` file, a malformed or out-of-range date, ...) -- silently
/// skipped by [`try_purge`] rather than treated as an error, so a
/// stray file some other tool dropped in `target/zhao/logs/` is never
/// touched.
fn log_filename_days(path: &Path) -> Option<i64> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()?.to_str()? != "log" {
        return None;
    }
    let mut parts = stem.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Today's date (local system clock), as `YYYY-MM-DD`. Computed by hand
/// from `SystemTime` rather than pulling in a date/time crate for the
/// one thing zhao's log rotation actually needs.
fn today() -> String {
    let (y, m, d) = civil_from_days(today_days());
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date (local system clock) as a day count since the Unix
/// epoch -- the same unit [`civil_from_days`]/[`days_from_civil`]
/// convert to/from a calendar date.
fn today_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
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

/// Howard Hinnant's `days_from_civil`: the inverse of
/// [`civil_from_days`], converting a proleptic-Gregorian (year, month,
/// day) back into a day count since the Unix epoch. See
/// <http://howardhinnant.github.io/date_algorithms.html>.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + u64::from(doy); // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
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
    fn days_from_civil_is_the_exact_inverse_of_civil_from_days() {
        for days in [-1, 0, 1, 19_722, 19_723, 19_782, 19_783] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y:04}-{m:02}-{d:02}");
        }
    }

    #[test]
    fn log_filename_days_parses_a_valid_log_filename() {
        let days = log_filename_days(Path::new("/x/target/zhao/logs/2024-02-29.log"))
            .expect("should parse");
        assert_eq!(civil_from_days(days), (2024, 2, 29));
    }

    #[test]
    fn log_filename_days_ignores_non_log_files() {
        assert_eq!(
            log_filename_days(Path::new("/x/target/zhao/logs/2024-02-29.json")),
            None
        );
        assert_eq!(
            log_filename_days(Path::new("/x/target/zhao/logs/not-a-date.log")),
            None
        );
    }

    /// Acceptance criterion: with nothing configured, no purging
    /// happens.
    #[test]
    fn purge_with_no_retention_configured_removes_nothing() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let logs_dir = dir.path().join("target").join("zhao").join("logs");
        std::fs::create_dir_all(&logs_dir).expect("should create logs dir");
        std::fs::write(logs_dir.join("2000-01-01.log"), "old").expect("should write old log");

        purge(dir.path(), None);

        assert!(logs_dir.join("2000-01-01.log").exists());
    }

    /// Acceptance criterion: purging only ever removes files under
    /// `target/zhao/logs/` older than the configured window -- never
    /// touches any other `target/zhao/` artifact, and never a log file
    /// still within the window.
    #[test]
    fn purge_removes_only_logs_older_than_the_configured_window() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let logs_dir = dir.path().join("target").join("zhao").join("logs");
        std::fs::create_dir_all(&logs_dir).expect("should create logs dir");

        let (y, m, d) = civil_from_days(today_days());
        let today_name = format!("{y:04}-{m:02}-{d:02}.log");
        let (oy, om, od) = civil_from_days(today_days() - 100);
        let old_name = format!("{oy:04}-{om:02}-{od:02}.log");

        std::fs::write(logs_dir.join(&today_name), "today").expect("should write today's log");
        std::fs::write(logs_dir.join(&old_name), "old").expect("should write old log");
        let sentinel = dir
            .path()
            .join("target")
            .join("zhao")
            .join("run-metadata.json");
        std::fs::write(&sentinel, "{}").expect("should write sentinel file");

        purge(dir.path(), Some(30));

        assert!(
            logs_dir.join(&today_name).exists(),
            "a log within the retention window should survive"
        );
        assert!(
            !logs_dir.join(&old_name).exists(),
            "a log older than the retention window should be removed"
        );
        assert!(
            sentinel.exists(),
            "purging must never touch other target/zhao/ artifacts"
        );
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
