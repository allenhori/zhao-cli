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

/// A project with no `zhao.yml` at all must behave identically to the v1
/// hardcoded defaults -- this fixture is the same manifest pair as
/// `type_widening_does_not_fire_while_join_loosening_does`, just without
/// a config file, so the two tests together prove the "no config" and
/// "with config" paths diverge only when a `zhao.yml` is actually present.
#[test]
fn no_zhao_yml_behaves_identically_to_v1_defaults() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(0) // join-cardinality-loosened defaults to `warn`, not `error`
        .stdout(
            predicate::str::contains("\"rule\": \"join-cardinality-loosened\"")
                .and(predicate::str::contains("\"severity\": \"warn\"")),
        );
}

/// A `zhao.yml` selecting the `strict` Preset must change at least one
/// Rule's outcome from `warn` to `error` versus no config at all -- same
/// fixture as the no-config test above, but with `preset: strict` in
/// `zhao.yml`, changing the exit code from 0 to 1.
#[test]
fn strict_preset_changes_a_warn_rule_to_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("config_strict_project"))
        .arg("--format")
        .arg("json")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("\"rule\": \"join-cardinality-loosened\"")
                .and(predicate::str::contains("\"severity\": \"error\"")),
        );
}

/// A per-Rule override in `zhao.yml` must win over the Preset for that
/// Rule only, leaving every other Rule at the Preset's value: `preset:
/// strict` plus an override pinning `column-added` back to `pass` --
/// `column-type-narrowed` (not overridden) must still become `error`
/// under strict, while `column-added` stays `pass` despite strict.
#[test]
fn per_rule_override_wins_only_for_that_rule() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("config_override_project"))
        .arg("--format")
        .arg("json")
        .output()
        .expect("command should run");

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let findings = parsed["findings"]
        .as_array()
        .expect("findings should be an array");

    let type_narrowed = findings
        .iter()
        .find(|f| f["rule"] == "column-type-narrowed")
        .expect("column-type-narrowed should fire");
    assert_eq!(
        type_narrowed["severity"], "error",
        "not overridden -- should follow the strict Preset"
    );

    let column_added = findings
        .iter()
        .find(|f| f["rule"] == "column-added")
        .expect("column-added should fire");
    assert_eq!(
        column_added["severity"], "pass",
        "overridden -- should stay pass despite the strict Preset"
    );
}

/// Builds a fake monorepo under a fresh temp dir: a `.git` marker at the
/// root (so `Config::load_for_project` recognizes it as a repo root) and a
/// nested dbt project directory, with the given fixture's manifest copied
/// in as the project's `target/manifest.json`.
fn fake_monorepo(project_manifest_fixture: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("should create .git marker");

    let project_dir = dir.path().join("services").join("analytics");
    std::fs::create_dir_all(project_dir.join("target")).expect("should create project target dir");
    std::fs::copy(
        fixture(project_manifest_fixture)
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy fixture manifest");

    (dir, project_dir)
}

/// A root-level `zhao.yml`, with no project-local file at all, must still
/// apply to a nested dbt project -- the monorepo case with only one policy
/// for the whole repo.
#[test]
fn a_root_only_zhao_yml_applies_to_a_nested_dbt_project() {
    let (repo, project_dir) = fake_monorepo("rules_project");
    std::fs::write(repo.path().join("zhao.yml"), "preset: strict\n")
        .expect("should write root zhao.yml");

    // Under the default Preset this Rule is `warn` (see
    // `strict_preset_changes_a_warn_rule_to_error`); the root `zhao.yml`'s
    // `strict` Preset must still raise it to `error` for the nested
    // project, purely from the root file.
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("\"rule\": \"join-cardinality-loosened\"")
                .and(predicate::str::contains("\"severity\": \"error\"")),
        );
}

/// A project-local `zhao.yml` must win over the root `zhao.yml` for the
/// keys it sets, while the root's Preset still governs everything else --
/// the same override relationship a Preset already has to an individual
/// Rule override, one layer higher.
#[test]
fn a_project_local_zhao_yml_overrides_the_root_one_per_key() {
    let (repo, project_dir) = fake_monorepo("rules_project");
    std::fs::write(repo.path().join("zhao.yml"), "preset: strict\n")
        .expect("should write root zhao.yml");
    std::fs::write(
        project_dir.join("zhao.yml"),
        "rules:\n  join-cardinality-loosened: warn\n",
    )
    .expect("should write project-local zhao.yml");

    // The project-local override pins this one Rule back to `warn` despite
    // the root's `strict` Preset, so the gate passes.
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("\"rule\": \"join-cardinality-loosened\"")
                .and(predicate::str::contains("\"severity\": \"warn\"")),
        );
}

/// A single-project repo (a `.git` at the same level as the dbt project,
/// no monorepo nesting) must keep behaving exactly as it did in #8: its
/// own `zhao.yml` applies, nothing more.
#[test]
fn a_single_project_repo_behaves_exactly_as_before_monorepo_support() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("should create .git marker");
    std::fs::create_dir_all(dir.path().join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("rules_project")
            .join("target")
            .join("manifest.json"),
        dir.path().join("target").join("manifest.json"),
    )
    .expect("should copy fixture manifest");
    std::fs::write(dir.path().join("zhao.yml"), "preset: strict\n").expect("should write zhao.yml");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("rules_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .arg("--format")
        .arg("json")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("\"rule\": \"join-cardinality-loosened\"")
                .and(predicate::str::contains("\"severity\": \"error\"")),
        );
}

/// An unknown Rule name or invalid Severity value in `zhao.yml` must
/// produce a clear, actionable error (exit code 2, the same "zhao itself
/// failed" code other config/IO errors use) rather than silently
/// ignoring the mistake.
#[test]
fn invalid_zhao_yml_produces_a_clear_error() {
    // A guaranteed-unique, RAII-cleaned-up temp directory via `tempfile`,
    // not a hand-rolled PID-based name -- the same fix applied to
    // zhao-core's own config tests after a hand-rolled name collided
    // under parallel test execution; this test needs the identical fix,
    // not just a similar one.
    let dir = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(dir.path().join("target")).expect("should create target dir");
    std::fs::copy(
        fixture("clean_project")
            .join("target")
            .join("manifest.json"),
        dir.path().join("target").join("manifest.json"),
    )
    .expect("should copy fixture manifest");
    std::fs::write(
        dir.path().join("zhao.yml"),
        "rules:\n  not-a-real-rule: error\n",
    )
    .expect("should write zhao.yml");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest_clean.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("unknown rule")
                .and(predicate::str::contains("not-a-real-rule")),
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
