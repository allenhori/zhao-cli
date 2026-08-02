//! Integration tests for the `zhao check` command boundary: invokes the
//! actual compiled binary (via `assert_cmd`) against fixture dbt project
//! states and asserts on stdout and exit code -- the seam the rest of the
//! test suite is built on.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

#[test]
fn exits_non_zero_and_reports_the_breaking_change_as_json() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("\"rule\": \"column-removed-with-active-references\"")
                .and(predicate::str::contains("\"severity\": \"error\""))
                .and(predicate::str::contains(
                    "\"reached\": \"model.zhao_dbt_test.dim_customers\"",
                )),
        );
}

#[test]
fn produced_json_is_well_formed() {
    let output = Command::cargo_bin("zhao")
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

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    assert!(parsed["changes"].is_array());
    assert!(parsed["findings"].is_array());
    assert_eq!(parsed["changes"].as_array().unwrap().len(), 5);
    assert_eq!(parsed["findings"].as_array().unwrap().len(), 1);
}

#[test]
fn exits_zero_when_nothing_breaking_is_found() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest_clean.json"))
        .arg("--project-dir")
        .arg(fixture("clean_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("\"findings\": []"));
}

/// Distinct from `exits_zero_when_nothing_breaking_is_found`: that test
/// has zero Changes at all. This one has a real Change (a column added)
/// that simply doesn't match the shipped Rule, to confirm the Rule
/// correctly declines to fire rather than exiting zero only because
/// nothing happened.
#[test]
fn exits_zero_when_a_change_does_not_match_the_rule() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest_clean.json"))
        .arg("--project-dir")
        .arg(fixture("non_matching_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("\"type\": \"column_added\"")
                .and(predicate::str::contains("\"findings\": []")),
        );
}

#[test]
fn exits_with_error_code_on_a_missing_baseline_path() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("does_not_exist.json"))
        .arg("--project-dir")
        .arg(fixture("clean_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:"));
}
