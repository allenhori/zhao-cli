//! Integration tests for the daily-rotating run log (issue #35): invokes
//! the actual compiled binary and asserts on `target/zhao/logs/<date>.log`.

use assert_cmd::Command;
use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    // Mirrors `zhao_cli::log`'s own civil-calendar computation -- kept as
    // a separate, independently-written calculation (via `chrono`-free
    // manual arithmetic) rather than calling the binary's private
    // function, since this is a black-box integration test.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Acceptance criterion: a `zhao lineage` run appends its stdout to
/// `target/zhao/logs/<today>.log`.
#[test]
fn a_lineage_run_appends_its_stdout_to_todays_log() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("rules_project")
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let log_path = project_dir
        .join("target")
        .join("zhao")
        .join("logs")
        .join(format!("{}.log", today()));
    assert!(
        log_path.exists(),
        "expected {} to exist",
        log_path.display()
    );
    let content = std::fs::read_to_string(&log_path).expect("should read log file");
    assert!(
        content.contains("model model.zhao_dbt_test.dim_customers"),
        "{content}"
    );
}

/// Acceptance criterion: multiple runs the same day append to the same
/// file rather than each overwriting or creating a new one.
#[test]
fn multiple_runs_the_same_day_append_to_the_same_file() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("rules_project")
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");

    for _ in 0..2 {
        Command::cargo_bin("zhao")
            .expect("binary should build")
            .arg("lineage")
            .arg("--text")
            .arg("stg_orders")
            .arg("--project-dir")
            .arg(project_dir)
            .assert()
            .code(0);
    }

    let log_path = project_dir
        .join("target")
        .join("zhao")
        .join("logs")
        .join(format!("{}.log", today()));
    let content = std::fs::read_to_string(&log_path).expect("should read log file");
    assert_eq!(
        content.matches("Upstream:").count(),
        2,
        "two runs should append two entries, not overwrite each other: {content}"
    );
}

/// Acceptance criterion: the log mirror never changes what's actually
/// printed to the real stdout.
#[test]
fn the_log_mirror_never_changes_real_stdout_output() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("rules_project")
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");

    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .output()
        .expect("command should run");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Upstream:\n"), "{stdout}");
    assert!(
        stdout.contains("model model.zhao_dbt_test.dim_customers"),
        "{stdout}"
    );
}

/// Acceptance criterion: `--log-level` is accepted and parsed without
/// error, even though it produces no different log content yet.
#[test]
fn a_log_level_flag_is_accepted_without_error() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("rules_project")
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
        .arg("--log-level")
        .arg("debug")
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);
}

/// Writes an executable stub script named `dbt` into `dir` -- `zhao
/// lineage --compile` always invokes the literal command `"dbt"`, so
/// putting `dir` first on `PATH` makes it the one actually run, without
/// needing a real dbt installation. Mirrors `zhao-core`'s own
/// `stub_dbt_command` test helper.
#[cfg(unix)]
fn stub_dbt_command(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("dbt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("should write stub script");
    let mut perms = std::fs::metadata(&path)
        .expect("should stat stub script")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("should chmod stub script");
    path
}

/// Acceptance criterion (issue #36): a successful internal `dbt
/// compile`'s captured stdout/stderr appears in that day's run log.
#[cfg(unix)]
#[test]
fn a_successful_compiles_output_is_routed_into_the_run_log() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    let stub_dir = tempfile::tempdir().expect("should create temp dir");
    stub_dbt_command(
        stub_dir.path(),
        "mkdir -p target && echo '{}' > target/manifest.json\n\
         echo 'ZHAO_TEST_COMPILE_STDOUT_MARKER'",
    );

    let path_with_stub = format!(
        "{}:{}",
        stub_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--compile")
        .arg("--project-dir")
        .arg(project_dir)
        .env("PATH", path_with_stub)
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));

    let log_path = project_dir
        .join("target")
        .join("zhao")
        .join("logs")
        .join(format!("{}.log", today()));
    let log_content = std::fs::read_to_string(&log_path).expect("should read log file");
    assert!(
        log_content.contains("ZHAO_TEST_COMPILE_STDOUT_MARKER"),
        "{log_content}"
    );

    // zhao's own real stdout is unaffected -- dbt's captured output
    // never gets inherited/printed directly, only mirrored to the log.
    let real_stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        !real_stdout.contains("ZHAO_TEST_COMPILE_STDOUT_MARKER"),
        "{real_stdout}"
    );
}
