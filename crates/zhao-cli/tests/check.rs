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

/// Exercises all four v1 Rules together against one realistic fixture:
/// `column-removed-with-active-references` (error), `column-type-narrowed`
/// (warn, `bigint` -> `int`), and `column-added` (pass) all fire; the
/// fixture's join change (`LEFT` -> `INNER`) is a *tightening*, so
/// `join-cardinality-loosened` correctly produces no Finding at all --
/// proving the negative case alongside the three positive ones, not just
/// asserting a count.
#[test]
fn all_applicable_rules_fire_together_on_a_fixture_with_simultaneous_changes() {
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
    let changes = parsed["changes"]
        .as_array()
        .expect("changes should be an array");
    let findings = parsed["findings"]
        .as_array()
        .expect("findings should be an array");

    // 5 Changes: type change, column added, two column removals, one join change.
    assert_eq!(changes.len(), 5);

    let rules: Vec<&str> = findings
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert_eq!(
        rules.len(),
        3,
        "expected exactly 3 findings, got: {findings:#?}"
    );
    assert!(rules.contains(&"column-removed-with-active-references"));
    assert!(rules.contains(&"column-type-narrowed"));
    assert!(rules.contains(&"column-added"));
    assert!(
        !rules.contains(&"join-cardinality-loosened"),
        "the fixture's join change is LEFT -> INNER, a tightening -- it must not fire"
    );

    let severities: Vec<&str> = findings
        .iter()
        .map(|f| f["severity"].as_str().unwrap())
        .collect();
    assert!(severities.contains(&"error"));
    assert!(severities.contains(&"warn"));
    assert!(severities.contains(&"pass"));
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
/// that does produce a Finding -- just a `pass`-severity, informational
/// one -- confirming a non-`error` Finding doesn't fail the gate, rather
/// than exiting zero only because nothing happened.
#[test]
fn exits_zero_when_the_only_finding_is_pass_severity() {
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
                .and(predicate::str::contains("\"rule\": \"column-added\""))
                .and(predicate::str::contains("\"severity\": \"pass\"")),
        );
}

/// A dedicated golden fixture pair (a real dbt-compiled manifest, not
/// synthetic data) for `column-type-narrowed`'s and
/// `join-cardinality-loosened`'s *positive*/*negative* cases the other
/// fixtures don't happen to cover: `customer_id` documented type widens
/// (`int` -> `bigint`, must NOT fire the narrowing Rule) while
/// `dim_customers`' join loosens (`INNER` -> `LEFT`, must fire).
#[test]
fn type_widening_does_not_fire_while_join_loosening_does() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .arg("--format")
        .arg("json")
        .output()
        .expect("command should run");

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let findings = parsed["findings"]
        .as_array()
        .expect("findings should be an array");

    assert_eq!(
        findings.len(),
        1,
        "expected only the join-loosening finding, got: {findings:#?}"
    );
    assert_eq!(findings[0]["rule"], "join-cardinality-loosened");
    assert_eq!(findings[0]["from_kind"], "inner");
    assert_eq!(findings[0]["to_kind"], "left");
    assert!(
        !findings.iter().any(|f| f["rule"] == "column-type-narrowed"),
        "a type widening (int -> bigint) must not fire column-type-narrowed"
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
