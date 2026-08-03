//! Integration tests for the `zhao diff` command boundary: invokes the
//! actual compiled binary via `assert_cmd`, same convention as
//! `tests/check.rs`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

/// Acceptance criterion 1: `zhao diff` produces the same Change/impact
/// data as `zhao check` on the same inputs -- compares the full JSON
/// payload of both commands run against the identical fixture pair,
/// rather than spot-checking a few fields.
#[test]
fn diff_produces_the_same_data_as_check() {
    let check_output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--format")
        .arg("json")
        .output()
        .expect("command should run");

    let diff_output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--format")
        .arg("json")
        .output()
        .expect("command should run");

    let check_json: serde_json::Value =
        serde_json::from_slice(&check_output.stdout).expect("check stdout should be valid JSON");
    let diff_json: serde_json::Value =
        serde_json::from_slice(&diff_output.stdout).expect("diff stdout should be valid JSON");

    assert_eq!(
        check_json, diff_json,
        "zhao diff's report payload should be byte-for-byte identical to zhao check's \
         on the same inputs, aside from exit code"
    );
}

/// Acceptance criterion 2: `zhao diff` always exits zero, regardless of
/// what Severity outcomes are present -- exercised against a fixture that
/// would make `zhao check` exit 1 (a real `error`-severity Finding).
#[test]
fn diff_always_exits_zero_even_with_a_breaking_change_present() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("\"severity\": \"error\""));
}

/// Acceptance criterion 2, the negative-control check: exits zero on a
/// clean fixture too, not just "clean fixtures happen to exit zero
/// anyway" -- proven together with the test above, which shows a real
/// breaking Finding is present yet the exit code is still zero.
#[test]
fn diff_exits_zero_on_a_clean_fixture_too() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest_clean.json"))
        .arg("--project-dir")
        .arg(fixture("clean_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0);
}

/// Acceptance criterion 2, isolating `warn` specifically: a fixture whose
/// only Finding is `warn`-severity (`join-cardinality-loosened`, no
/// `error`-severity Finding at all) still exits zero -- proves "regardless
/// of what Severity outcomes are present" isn't only true for `error`.
#[test]
fn diff_exits_zero_when_the_only_finding_is_warn_severity() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("\"severity\": \"warn\""));
}

/// `zhao diff` must still exit non-zero (2, matching `zhao check`'s own
/// "couldn't even run" exit code) when zhao itself fails to run --
/// "always exits zero" is about Severity outcomes the engine actually
/// produced, not about zhao crashing before it could produce any.
#[test]
fn diff_exits_with_error_code_on_a_missing_baseline_path() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("does_not_exist.json"))
        .arg("--project-dir")
        .arg(fixture("clean_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:"));
}

/// Acceptance criterion 3: `zhao diff` supports `--format json` output,
/// same shape as `zhao check`'s.
#[test]
fn diff_supports_json_output() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("\"changes\":").and(predicate::str::contains("\"findings\":")),
        );
}

/// Acceptance criterion 3: `zhao diff` supports human-readable output too
/// (the default, no `--format` flag), same three-part report shape as
/// `zhao check`'s.
#[test]
fn diff_supports_human_readable_output() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Changed:\n")
                .and(predicate::str::contains("Downstream impact:\n"))
                .and(predicate::str::contains("Summary:")),
        );
}
