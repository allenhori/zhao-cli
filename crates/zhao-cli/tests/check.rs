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

/// Acceptance criteria 1, 2, 4, 5 together: with no `--format json`,
/// `zhao check` produces the three-part human-readable report, the
/// "Changed" section lists exactly the Nodes that changed with the
/// precise change described, the summary line's counts match the
/// underlying data, and every reference uses dbt's vocabulary
/// ("model"), never zhao's internal "Node"/"Origin" terms.
#[test]
fn default_text_output_produces_the_three_part_human_readable_report() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");

    assert!(stdout.contains("Changed:\n"), "{stdout}");
    assert!(stdout.contains("Downstream impact:\n"), "{stdout}");
    assert!(stdout.contains("Summary:"), "{stdout}");

    // "Changed" lists exactly the two Nodes with real Changes, each with
    // its precise change described (not just a generic "something
    // changed").
    assert!(
        stdout.contains("model model.zhao_dbt_test.stg_customers:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("model model.zhao_dbt_test.dim_customers:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("~ column type changed: customer_id (bigint -> int)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ column added: marketing_source"),
        "{stdout}"
    );
    assert!(stdout.contains("- column removed: last_name"), "{stdout}");
    assert!(
        stdout.contains("~ join changed at position 0: left -> inner"),
        "{stdout}"
    );

    // Summary counts match the fixture's known 5 Changes / 3 Findings
    // exactly (see `all_applicable_rules_fire_together_on_a_fixture_with_simultaneous_changes`
    // for the same fixture's JSON-shaped equivalent of these counts): 2
    // Nodes changed, 4 of the 5 Changes are column-level (the join
    // change isn't), 1 error-severity Finding, 1 warn-severity Finding
    // (the pass-severity `column-added` Finding isn't counted here).
    assert!(
        stdout.contains("Summary: 2 model(s) changed, 4 column(s) changed, 1 breaking, 1 warning"),
        "{stdout}"
    );

    // Vocabulary: "model", never zhao's internal terms.
    assert!(!stdout.contains("Node "), "{stdout}");
    assert!(!stdout.contains("Origin "), "{stdout}");
}

/// Acceptance criterion 3: "Downstream impact" lists only Nodes actually
/// reached by a breaking/warning Finding (not the whole DAG, and not a
/// Node that merely changed without producing a Finding), each with the
/// specific reference and Rule name -- and a `pass`-severity Finding
/// (informational, not impact) must not appear there at all, even though
/// its underlying Change does appear in "Changed".
#[test]
fn downstream_impact_lists_only_nodes_actually_reached_with_reason_and_rule() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .output()
        .expect("command should run");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    let downstream_impact = stdout
        .split("Downstream impact:\n")
        .nth(1)
        .expect("Downstream impact section should be present")
        .split("\nSummary:")
        .next()
        .expect("Summary should follow Downstream impact");

    assert!(
        downstream_impact.contains(
            "[BREAKING] last_name removed from model model.zhao_dbt_test.stg_customers \
             breaks reference via last_name (column-removed-with-active-references)"
        ),
        "{downstream_impact}"
    );
    assert!(
        downstream_impact
            .contains("[WARN] customer_id type narrowed from bigint to int (column-type-narrowed)"),
        "{downstream_impact}"
    );
    // `marketing_source` (the pass-severity `column-added` Change) must
    // not appear in this section at all -- it's informational, not
    // downstream impact.
    assert!(
        !downstream_impact.contains("marketing_source"),
        "a pass-severity finding must not appear in Downstream impact: {downstream_impact}"
    );
}

/// Acceptance criterion 3's "not the whole DAG" half, proven directly:
/// `breaking_project`'s manifest has six models total, but only
/// `stg_customers` and `dim_customers` are actually changed or reached.
/// The other four -- `stg_payments`, `stg_orders`, `fct_orders`,
/// `fct_orders_incremental` -- are unrelated and must appear in neither
/// "Changed" nor "Downstream impact", even though they're real models in
/// the same project's dependency graph.
#[test]
fn unrelated_models_in_the_same_project_do_not_appear_in_either_section() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .output()
        .expect("command should run");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    // Scoped to "Changed"/"Downstream impact" (everything before the
    // Summary line) -- unlike those two sections, the later Defer plan
    // section legitimately names upstream dependencies of the build set
    // (e.g. stg_orders, which dim_customers depends on) that were never
    // changed or reached themselves, so it's expected to mention Nodes
    // this test's "unrelated" list intentionally excludes from Changed/
    // Downstream impact.
    let changed_and_downstream_impact = stdout
        .split("\nSummary:")
        .next()
        .expect("Summary should follow Changed/Downstream impact");

    for unrelated in [
        "stg_payments",
        "stg_orders",
        "fct_orders",
        "fct_orders_incremental",
    ] {
        assert!(
            !changed_and_downstream_impact.contains(unrelated),
            "{unrelated} is unrelated to this diff and must not appear in Changed or \
             Downstream impact, but it did: {changed_and_downstream_impact}"
        );
    }
}

/// `--no-color`, verified byte-for-byte against a plain-text snapshot (not
/// just "doesn't contain an escape somewhere") -- the exact stdout must be
/// exactly the plain-text rendering, nothing more. Deliberately uses
/// `breaking_project` (a fixture with real `BREAKING`/`WARN` findings,
/// same as the other text-report tests above) rather than a no-changes
/// fixture: the no-changes path returns early before ever reaching the
/// only code that calls `colorize()`, so a snapshot of *that* path would
/// pass identically even if `--no-color` were silently ignored. This one
/// actually exercises the colored code path and proves color was
/// genuinely suppressed on it, not merely absent because it was never
/// going to be there.
#[test]
fn no_color_flag_produces_byte_for_byte_plain_text() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            "Changed:\n\
             \x20 model model.zhao_dbt_test.stg_customers:\n\
             \x20   ~ column type changed: customer_id (bigint -> int)\n\
             \x20   + column added: marketing_source\n\
             \x20   - column removed: last_name\n\
             \x20 model model.zhao_dbt_test.dim_customers:\n\
             \x20   - column removed: last_name\n\
             \x20   ~ join changed at position 0: left -> inner\n\
             \n\
             Downstream impact:\n\
             \x20 model model.zhao_dbt_test.stg_customers:\n\
             \x20   [WARN] customer_id type narrowed from bigint to int (column-type-narrowed)\n\
             \x20 model model.zhao_dbt_test.dim_customers:\n\
             \x20   [BREAKING] last_name removed from model model.zhao_dbt_test.stg_customers \
             breaks reference via last_name (column-removed-with-active-references)\n\
             \n\
             Summary: 2 model(s) changed, 4 column(s) changed, 1 breaking, 1 warning\n\
             \n\
             Recommended: dbt build --select stg_customers dim_customers\n\
             \n\
             Defer plan:\n\
             \x20 Build: stg_customers, dim_customers\n\
             \x20 Defer (assumed available): stg_orders\n",
        );
}

/// Auto-detection: with no `--no-color` flag and no CI environment
/// variable forcing color on, stdout being piped (as it always is when
/// captured by `assert_cmd`, exactly like being piped to a file) must
/// suppress color on its own. `GITHUB_ACTIONS`/`NO_COLOR` are explicitly
/// removed from the child's environment first, since this test itself may
/// be running inside zhao-cli's own GitHub Actions CI, which would
/// otherwise force color on and mask a real auto-detection regression.
#[test]
fn auto_detection_suppresses_color_when_stdout_is_not_a_tty() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .output()
        .expect("command should run");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        !stdout.contains('\u{1b}'),
        "piped (non-TTY) stdout outside any CI environment should auto-suppress color: \
         {stdout:?}"
    );
}

/// Acceptance criterion 1: color codes are emitted in a color-capable
/// environment -- simulated via `GITHUB_ACTIONS=true` (assert_cmd's
/// captured stdout is never a real TTY, so this is the only reliable way
/// to exercise the "color enabled" path through the actual binary rather
/// than only through `report.rs`'s unit tests).
#[test]
fn color_codes_are_emitted_in_a_color_capable_environment() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .env("GITHUB_ACTIONS", "true")
        .env_remove("NO_COLOR")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .output()
        .expect("command should run");

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains('\u{1b}'),
        "a color-capable environment (GITHUB_ACTIONS=true) should emit ANSI escapes: {stdout:?}"
    );
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

/// Acceptance criterion 1: the generated selector's set of Nodes exactly
/// matches the Nodes named in the Downstream impact section.
#[test]
fn recommended_command_matches_the_downstream_impact_nodes() {
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
        .stdout(predicate::str::contains(
            "\"recommended_command\": \"dbt build --select stg_customers dim_customers\"",
        ));
}

/// Acceptance criterion 2: a run with zero impacted Nodes produces no
/// recommended command -- covers both "zero Changes at all" and "a Change
/// exists but its only Finding is pass-severity" (not Downstream impact).
#[test]
fn no_recommended_command_when_nothing_is_impactful() {
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
        .stdout(predicate::str::contains("recommended_command").not());

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
        .stdout(predicate::str::contains("recommended_command").not());
}

/// Acceptance criterion 1: given a fixture project and a Change reaching a
/// subset of Nodes, the computed `--defer` plan correctly identifies
/// which Nodes need building (`stg_customers`, `dim_customers` -- the same
/// set the recommended command selects) versus which can be deferred to
/// an existing state (`stg_orders`, a real upstream dependency of
/// `dim_customers` that was never itself changed or reached).
#[test]
fn defer_plan_identifies_build_and_defer_sets_correctly() {
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
            predicate::str::contains("\"defer_plan\":")
                .and(predicate::str::contains("\"stg_customers\","))
                .and(predicate::str::contains("\"dim_customers\""))
                .and(predicate::str::contains(
                    "\"defer\": [\n      \"stg_orders\"\n    ]",
                )),
        );
}

/// Acceptance criterion 2: exposed in human-readable output too, same as
/// `--format json`.
#[test]
fn defer_plan_appears_in_human_readable_output() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("Defer plan:\n")
                .and(predicate::str::contains(
                    "Build: stg_customers, dim_customers",
                ))
                .and(predicate::str::contains(
                    "Defer (assumed available): stg_orders",
                )),
        );
}

/// `--defer-target`/`--defer-state` (no `zhao.yml` involved -- the
/// config-cascading behavior itself is covered at the `zhao_core::config`
/// unit level) produce a ready-to-run `--defer --state <path>` command on
/// the plan, in both output formats.
#[test]
fn defer_target_and_state_flags_produce_a_ready_to_run_command() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--defer-target")
        .arg("prod")
        .arg("--defer-state")
        .arg("artifacts/prod/manifest.json")
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("Target: prod").and(predicate::str::contains(
                "Command: dbt build --select stg_customers dim_customers --defer --state artifacts/prod/manifest.json",
            )),
        );
}

/// The same flags in `--format json` land on the `defer_plan.command`/
/// `defer_plan.target` keys.
#[test]
fn defer_target_and_state_flags_appear_in_json_output() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--defer-target")
        .arg("prod")
        .arg("--defer-state")
        .arg("artifacts/prod/manifest.json")
        .arg("--format")
        .arg("json")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("\"target\": \"prod\"").and(predicate::str::contains(
                "\"command\": \"dbt build --select stg_customers dim_customers --defer --state artifacts/prod/manifest.json\"",
            )),
        );
}

/// Without either flag (and no `zhao.yml` `defer:` section in this
/// fixture), the plan carries neither a target label nor a command --
/// exactly the pre-existing behavior.
#[test]
fn no_defer_flags_produce_no_target_or_command() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("breaking_project"))
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("Defer plan:\n")
                .and(predicate::str::contains("Target:").not())
                .and(predicate::str::contains("Command:").not()),
        );
}

/// `zhao.yml`'s own `defer:` section (with no CLI flag given) surfaces
/// its configured target/state, proving the config path itself -- not
/// just the CLI-flag path -- reaches the generated command.
#[test]
fn zhao_yml_defer_config_surfaces_without_any_cli_flag() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("defer_config_project"))
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("Target: staging").and(predicate::str::contains(
                "Command: dbt build --select stg_customers dim_customers --defer --state artifacts/staging/manifest.json",
            )),
        );
}

/// A `--defer-target`/`--defer-state` CLI flag overrides a *conflicting*
/// `zhao.yml` `defer:` value -- not just producing a command when
/// `zhao.yml` has none at all (`defer_target_and_state_flags_produce_a_ready_to_run_command`
/// already covers that weaker case).
#[test]
fn defer_flags_override_a_conflicting_zhao_yml_defer_config() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(fixture("defer_config_project"))
        .arg("--defer-target")
        .arg("prod")
        .arg("--defer-state")
        .arg("artifacts/prod/manifest.json")
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("Target: prod")
                .and(predicate::str::contains(
                    "Command: dbt build --select stg_customers dim_customers --defer --state artifacts/prod/manifest.json",
                ))
                .and(predicate::str::contains("staging").not())
                .and(predicate::str::contains("artifacts/staging").not()),
        );
}

/// A run with zero impacted Nodes produces no defer plan (nothing to
/// build, so no plan makes sense) -- mirrors
/// `no_recommended_command_when_nothing_is_impactful`.
#[test]
fn no_defer_plan_when_nothing_is_impactful() {
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
        .stdout(predicate::str::contains("defer_plan").not());
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

// ---------------------------------------------------------------------
// --check-relations: upgrades or drops the conditional schema-evolution
// flag by actually checking relation existence, via a stub `dbt` on
// `PATH` standing in for `dbt run-operation` -- exercised via `--state`
// (no git needed), since the live check runs against the *current*
// project regardless of how the Baseline was resolved.
// ---------------------------------------------------------------------

#[cfg(unix)]
mod check_relations {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn incremental_manifest(select_sql: &str) -> String {
        format!(
            r#"{{
                "metadata": {{"adapter_type": "duckdb"}},
                "sources": {{}},
                "nodes": {{
                    "model.p.m": {{
                        "unique_id": "model.p.m",
                        "resource_type": "model",
                        "name": "m",
                        "database": "db",
                        "schema": "public",
                        "alias": "m",
                        "compiled_code": "{select_sql}",
                        "config": {{"materialized": "incremental"}}
                    }}
                }}
            }}"#
        )
    }

    /// A throwaway project directory: a baseline manifest file (single
    /// column) plus a current `target/manifest.json` (baseline's column
    /// plus one added) -- the schema-changing Change every test in this
    /// module exercises.
    fn project_with_a_schema_change() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let baseline_path = dir.path().join("baseline_manifest.json");
        std::fs::write(&baseline_path, incremental_manifest("select 1 as id"))
            .expect("should write baseline manifest");

        std::fs::create_dir_all(dir.path().join("target")).expect("should create target dir");
        std::fs::write(
            dir.path().join("target").join("manifest.json"),
            incremental_manifest("select 1 as id, 2 as new_col"),
        )
        .expect("should write current manifest");

        (dir, baseline_path)
    }

    /// A stub `dbt` whose `run-operation` subcommand always echoes the
    /// given result marker line, standing in for a real
    /// `zhao_relation_exists` macro run against a live warehouse.
    fn stub_dbt_run_operation(result: &str) -> tempfile::TempDir {
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let path = stub_dir.path().join("dbt");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho 'ZHAO_RELATION_EXISTS_RESULT:{result}'\n"),
        )
        .expect("should write stub dbt script");
        let mut perms = std::fs::metadata(&path)
            .expect("should stat stub script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("should chmod stub script");
        stub_dir
    }

    /// Acceptance criterion: confirmed existing upgrades the flag to
    /// definitive wording.
    #[test]
    fn upgrades_the_flag_to_definitive_when_the_relation_is_confirmed_to_exist() {
        let (project, baseline) = project_with_a_schema_change();
        let stub_dir = stub_dbt_run_operation("true");

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    stub_dir.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .arg("check")
            .arg("--state")
            .arg(&baseline)
            .arg("--project-dir")
            .arg(project.path())
            .arg("--check-relations")
            .arg("--no-color")
            .output()
            .expect("command should run");

        let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
        assert!(stdout.contains("Schema evolution:"), "{stdout}");
        assert!(!stdout.contains("if this"), "{stdout}");
        assert!(
            stdout.contains("exists in your target environment"),
            "{stdout}"
        );
    }

    /// Acceptance criterion: confirmed absent drops the flag entirely.
    #[test]
    fn drops_the_flag_when_the_relation_is_confirmed_absent() {
        let (project, baseline) = project_with_a_schema_change();
        let stub_dir = stub_dbt_run_operation("false");

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    stub_dir.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .arg("check")
            .arg("--state")
            .arg(&baseline)
            .arg("--project-dir")
            .arg(project.path())
            .arg("--check-relations")
            .arg("--no-color")
            .output()
            .expect("command should run");

        let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
        assert!(!stdout.contains("Schema evolution:"), "{stdout}");
    }

    /// Acceptance criterion: without `--check-relations`, behavior is
    /// unchanged -- the flag stays conditionally worded.
    #[test]
    fn without_the_flag_the_wording_stays_conditional() {
        let (project, baseline) = project_with_a_schema_change();

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .arg("check")
            .arg("--state")
            .arg(&baseline)
            .arg("--project-dir")
            .arg(project.path())
            .arg("--no-color")
            .output()
            .expect("command should run");

        let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
        assert!(stdout.contains("Schema evolution:"), "{stdout}");
        assert!(stdout.contains("if this"), "{stdout}");
    }
}

// ---------------------------------------------------------------------
// Git-native Baseline resolution (no `--state`): a throwaway git repo
// with a real merge-base, and a stub `dbt` on `PATH` standing in for a
// real dbt install -- these tests exercise zhao's own merge-base
// resolution, worktree creation, and `dbt compile` invocation end to end,
// without depending on whether a real `dbt` happens to be installed
// wherever they run.
// ---------------------------------------------------------------------

#[cfg(unix)]
mod git_native_baseline {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command as StdCommand;

    /// A throwaway git repository, with helpers for the handful of git
    /// operations these tests need.
    struct TestRepo {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl TestRepo {
        fn git(&self, args: &[&str]) {
            let output = StdCommand::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()
                .expect("git should be runnable in tests");
            assert!(
                output.status.success(),
                "git {args:?} should succeed: {output:?}"
            );
        }

        fn write(&self, relative_path: &str, contents: &str) {
            let path = self.path.join(relative_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("should create parent dir");
            }
            std::fs::write(path, contents).expect("should write file");
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "."]);
            self.git(&["commit", "-m", message]);
        }
    }

    fn new_test_repo() -> TestRepo {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().to_path_buf();
        let repo = TestRepo { _dir: dir, path };

        repo.git(&["init", "--initial-branch=master"]);
        repo.git(&["config", "user.email", "test@zhao.invalid"]);
        repo.git(&["config", "user.name", "zhao test"]);
        repo
    }

    /// Writes an executable stub `dbt` to a fresh temp dir: `dbt compile`
    /// just copies `dbt_manifest_source.json` (committed alongside the
    /// project, so its content differs per commit) into
    /// `target/manifest.json` -- close enough to real `dbt compile`'s
    /// contract (produce `target/manifest.json` reflecting the checked-out
    /// state) for these tests, without needing a real dbt install.
    fn stub_dbt_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().join("dbt");
        std::fs::write(
            &path,
            "#!/bin/sh\nmkdir -p target\ncp dbt_manifest_source.json target/manifest.json\n",
        )
        .expect("should write stub dbt script");
        let mut perms = std::fs::metadata(&path)
            .expect("should stat stub script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("should chmod stub script");
        dir
    }

    fn path_with_stub_dbt_prepended(stub_dir: &tempfile::TempDir) -> String {
        let existing = std::env::var("PATH").unwrap_or_default();
        format!("{}:{existing}", stub_dir.path().display())
    }

    /// A stub `dbt` that additionally records every invocation (subcommand
    /// plus args, one line per call) to `invocation_log`, an ABSOLUTE path
    /// outside the git worktree Baseline resolution runs inside --
    /// `git::create_worktree`'s `Worktree` is cleaned up (`Drop`) before
    /// `resolve()` returns, so a log file written to a path relative to the
    /// worktree wouldn't survive for the test to inspect afterwards.
    ///
    /// Only `compile` produces `target/manifest.json` (from
    /// `dbt_manifest_source.json`, as `stub_dbt_dir` does); `deps` and any
    /// other subcommand just log and exit 0.
    fn logging_stub_dbt_dir(invocation_log: &Path) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().join("dbt");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\nif [ \"$1\" = \"compile\" ]; then\n  mkdir -p target\n  cp dbt_manifest_source.json target/manifest.json\nfi\n",
                log = invocation_log.display()
            ),
        )
        .expect("should write logging stub dbt script");
        let mut perms = std::fs::metadata(&path)
            .expect("should stat stub script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("should chmod stub script");
        dir
    }

    /// Builds a repo with one commit on `master` (whose
    /// `dbt_manifest_source.json` is `baseline_manifest`) and a `feature`
    /// branch one commit ahead (so `master`'s tip is the merge-base), with
    /// `current_manifest` written directly as the working tree's
    /// `target/manifest.json` -- the file `zhao check` reads for "current"
    /// state regardless of Baseline resolution mode.
    fn repo_with_baseline_and_current(baseline_manifest: &str, current_manifest: &str) -> TestRepo {
        let repo = new_test_repo();
        repo.write("dbt_manifest_source.json", baseline_manifest);
        repo.commit("baseline state");

        repo.git(&["checkout", "-b", "feature"]);
        repo.write("README.md", "an unrelated change on the feature branch\n");
        repo.commit("feature work, ahead of master");

        repo.write("target/manifest.json", current_manifest);
        repo
    }

    fn rules_baseline_manifest_json() -> String {
        std::fs::read_to_string(fixture("rules_baseline_manifest.json"))
            .expect("should read fixture")
    }

    fn rules_project_current_manifest_json() -> String {
        std::fs::read_to_string(
            fixture("rules_project")
                .join("target")
                .join("manifest.json"),
        )
        .expect("should read fixture")
    }

    /// Acceptance criterion 1 & 2: with no `--state`, inside a git repo
    /// with a real merge-base, `zhao check` resolves and compiles that
    /// commit as the Baseline, and the resulting output matches what an
    /// equivalent `--state <manifest>` run produces for the same diff --
    /// this is the same fixture pair `type_widening_does_not_fire_while_join_loosening_does`
    /// uses via `--state`, so the two tests' assertions being identical
    /// *is* the equivalence proof.
    #[test]
    fn resolves_and_compiles_the_merge_base_commit_as_the_baseline() {
        let stub_dir = stub_dbt_dir();
        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--format")
            .arg("json")
            .output()
            .expect("command should run");

        assert!(
            output.status.success() || output.status.code() == Some(1),
            "expected exit 0 or 1, got {:?}; stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

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

    /// Same shape as `repo_with_baseline_and_current`, but the default
    /// branch is named `main` (not `master`) -- since zhao's own
    /// hardcoded default is `"master"`, a merge-base resolution that
    /// only succeeds against `main` can only be working because
    /// `zhao.yml`'s `against` (or an explicit `--against`) was actually
    /// read, not because it happened to coincide with the built-in
    /// default.
    fn repo_with_main_branch_and_current(
        baseline_manifest: &str,
        current_manifest: &str,
    ) -> TestRepo {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().to_path_buf();
        let repo = TestRepo { _dir: dir, path };
        repo.git(&["init", "--initial-branch=main"]);
        repo.git(&["config", "user.email", "test@zhao.invalid"]);
        repo.git(&["config", "user.name", "zhao test"]);

        repo.write("dbt_manifest_source.json", baseline_manifest);
        repo.commit("baseline state");

        repo.git(&["checkout", "-b", "feature"]);
        repo.write("README.md", "an unrelated change on the feature branch\n");
        repo.commit("feature work, ahead of main");

        repo.write("target/manifest.json", current_manifest);
        repo
    }

    /// Acceptance criterion: `zhao.yml`'s `against` is honored for
    /// git-native Baseline resolution when no `--against` flag is given.
    #[test]
    fn zhao_yml_against_is_honored_with_no_cli_flag() {
        let stub_dir = stub_dbt_dir();
        let repo = repo_with_main_branch_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );
        repo.write("zhao.yml", "against: main\n");

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--format")
            .arg("json")
            .output()
            .expect("command should run");

        assert!(
            output.status.success() || output.status.code() == Some(1),
            "expected exit 0 or 1 (a real merge-base was found and diffed), got {:?}; stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Acceptance criterion: an explicit `--against` flag overrides a
    /// conflicting `zhao.yml` value.
    #[test]
    fn cli_against_flag_overrides_a_conflicting_zhao_yml_value() {
        let stub_dir = stub_dbt_dir();
        let repo = repo_with_main_branch_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );
        // A deliberately wrong zhao.yml value -- the CLI flag should win,
        // not this.
        repo.write("zhao.yml", "against: does-not-exist\n");

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--against")
            .arg("main")
            .arg("--format")
            .arg("json")
            .output()
            .expect("command should run");

        assert!(
            output.status.success() || output.status.code() == Some(1),
            "expected exit 0 or 1 (--against main should win over zhao.yml's bogus value), got {:?}; stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Acceptance criterion 3: a clear, actionable error (exit 2) when
    /// `dbt` isn't invokable. `PATH` is overridden to a directory holding
    /// only a `git` symlink -- deterministic regardless of whether a real
    /// `dbt` happens to be installed on the machine running this test.
    #[test]
    fn produces_a_clear_error_when_dbt_is_not_invokable() {
        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );

        let git_path = String::from_utf8(
            StdCommand::new("sh")
                .arg("-c")
                .arg("command -v git")
                .output()
                .expect("should locate git")
                .stdout,
        )
        .expect("git path should be utf8")
        .trim()
        .to_string();
        let git_only_dir = tempfile::tempdir().expect("should create temp dir");
        std::os::unix::fs::symlink(&git_path, git_only_dir.path().join("git"))
            .expect("should symlink git");

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", git_only_dir.path())
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("dbt").and(predicate::str::contains("PATH")));
    }

    /// Acceptance criterion 4: a clear, actionable error (exit 2) when a
    /// merge-base can't be determined -- here, because `--against` names a
    /// ref that doesn't exist at all.
    #[test]
    fn produces_a_clear_error_when_a_merge_base_cannot_be_determined() {
        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--against")
            .arg("this-branch-does-not-exist")
            .assert()
            .code(2)
            .stderr(predicate::str::contains("merge-base"));
    }

    /// Acceptance criterion 1 (issue #26): `dbt deps` runs, before `dbt
    /// compile`, whenever the merge-base commit's project directory has a
    /// `packages.yml`.
    #[test]
    fn dbt_deps_runs_before_compile_when_packages_yml_is_present_at_the_baseline() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = new_test_repo();
        repo.write("packages.yml", "packages: []\n");
        repo.write("dbt_manifest_source.json", &rules_baseline_manifest_json());
        repo.commit("baseline state, with packages.yml");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("README.md", "an unrelated change on the feature branch\n");
        repo.commit("feature work, ahead of master");
        repo.write(
            "target/manifest.json",
            &rules_project_current_manifest_json(),
        );

        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .output()
            .expect("command should run");
        assert!(
            output.status.code() == Some(0) || output.status.code() == Some(1),
            "zhao itself should run to completion: {output:?}"
        );

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        let subcommands: Vec<&str> = log
            .lines()
            .map(|line| line.split(' ').next().unwrap_or(""))
            .collect();
        assert_eq!(
            subcommands,
            vec!["deps", "compile"],
            "dbt deps should run, before dbt compile, when packages.yml is present: {log}"
        );
    }

    /// Acceptance criterion 2 (issue #26): no `packages.yml` at the
    /// merge-base commit means `dbt deps` never runs at all.
    #[test]
    fn dbt_deps_is_skipped_entirely_when_no_packages_yml_exists() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );
        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .output()
            .expect("command should run");
        assert!(
            output.status.code() == Some(0) || output.status.code() == Some(1),
            "zhao itself should run to completion: {output:?}"
        );

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        let subcommands: Vec<&str> = log
            .lines()
            .map(|line| line.split(' ').next().unwrap_or(""))
            .collect();
        assert_eq!(
            subcommands,
            vec!["compile"],
            "dbt deps should not run at all when no packages.yml exists: {log}"
        );
    }

    /// Acceptance criterion 1 (issue #26), `dependencies.yml` variant: the
    /// same trigger condition, but via the other filename `resolve()`
    /// checks for.
    #[test]
    fn dbt_deps_runs_before_compile_when_dependencies_yml_is_present_at_the_baseline() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = new_test_repo();
        repo.write("dependencies.yml", "packages: []\n");
        repo.write("dbt_manifest_source.json", &rules_baseline_manifest_json());
        repo.commit("baseline state, with dependencies.yml");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("README.md", "an unrelated change on the feature branch\n");
        repo.commit("feature work, ahead of master");
        repo.write(
            "target/manifest.json",
            &rules_project_current_manifest_json(),
        );

        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .output()
            .expect("command should run");
        assert!(
            output.status.code() == Some(0) || output.status.code() == Some(1),
            "zhao itself should run to completion: {output:?}"
        );

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        let subcommands: Vec<&str> = log
            .lines()
            .map(|line| line.split(' ').next().unwrap_or(""))
            .collect();
        assert_eq!(
            subcommands,
            vec!["deps", "compile"],
            "dbt deps should run, before dbt compile, when dependencies.yml is present: {log}"
        );
    }

    /// Acceptance criteria 1 & 3 (issue #26) together: `--dbt-arg` values
    /// reach `dbt deps` too, not just `dbt compile` -- exercised with
    /// `packages.yml` present so `deps` actually runs.
    #[test]
    fn dbt_arg_values_are_appended_to_both_deps_and_compile_invocations() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = new_test_repo();
        repo.write("packages.yml", "packages: []\n");
        repo.write("dbt_manifest_source.json", &rules_baseline_manifest_json());
        repo.commit("baseline state, with packages.yml");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("README.md", "an unrelated change on the feature branch\n");
        repo.commit("feature work, ahead of master");
        repo.write(
            "target/manifest.json",
            &rules_project_current_manifest_json(),
        );

        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--dbt-arg")
            .arg("--target=ci")
            .assert()
            .code(predicate::in_iter([0, 1]));

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            vec!["deps --target=ci", "compile --target=ci"],
            "--dbt-arg values should be appended to both the deps and compile invocations: {log}"
        );
    }

    /// Acceptance criterion 3 (issue #26): repeated `--dbt-arg` values are
    /// appended, in order, to the `dbt compile` invocation (and would be to
    /// `dbt deps` too, were it running -- there's no `packages.yml` here,
    /// so it isn't, matching `dbt_deps_is_skipped_entirely_when_no_packages_yml_exists`
    /// above).
    #[test]
    fn dbt_arg_values_are_appended_in_order_to_the_compile_invocation() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );
        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--dbt-arg")
            .arg("--target=ci")
            .arg("--dbt-arg")
            .arg("--vars={\"foo\": \"bar\"}")
            .assert()
            .code(predicate::in_iter([0, 1]));

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        assert_eq!(
            log.trim(),
            "compile --target=ci --vars={\"foo\": \"bar\"}",
            "both --dbt-arg values should be appended, in order, to the compile invocation"
        );
    }

    /// Acceptance criterion 4 (issue #26): `--dbt-args` produces the
    /// identical result via `shell-words` splitting -- proving the quoted
    /// `--vars` value survives as one argument, not split on its internal
    /// whitespace.
    #[test]
    fn dbt_args_shell_splits_into_the_identical_result_as_dbt_arg() {
        let log_dir = tempfile::tempdir().expect("should create temp dir");
        let invocation_log = log_dir.path().join("invocations.log");

        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );
        let stub_dir = logging_stub_dbt_dir(&invocation_log);

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .env("PATH", path_with_stub_dbt_prepended(&stub_dir))
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--dbt-args")
            .arg("--target ci --vars '{\"foo\": \"bar\"}'")
            .assert()
            .code(predicate::in_iter([0, 1]));

        let log = std::fs::read_to_string(&invocation_log).expect("should read invocation log");
        assert_eq!(
            log.trim(),
            "compile --target ci --vars {\"foo\": \"bar\"}",
            "--dbt-args should shell-word-split into the same argument boundaries \
             --dbt-arg would have produced by hand"
        );
    }

    /// Acceptance criterion 5 (issue #26): using both `--dbt-arg` and
    /// `--dbt-args` together is a clap-level usage error (exit 2) --
    /// nothing runs at all, not even the merge-base resolution, let alone
    /// `dbt`.
    #[test]
    fn using_both_dbt_arg_and_dbt_args_together_is_a_clear_cli_error() {
        let repo = repo_with_baseline_and_current(
            &rules_baseline_manifest_json(),
            &rules_project_current_manifest_json(),
        );

        Command::cargo_bin("zhao")
            .expect("binary should build")
            .arg("check")
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--dbt-arg")
            .arg("--target=ci")
            .arg("--dbt-args")
            .arg("--target ci")
            .assert()
            .code(2)
            .stderr(predicate::str::contains("cannot be used with"));
    }
}

// ---------------------------------------------------------------------
// Staleness warning: a non-blocking notice when the target branch has
// moved on since the Baseline's merge-base. Exercised via `--state` (not
// git-native Baseline resolution) so these tests only need a real git
// repo, not a `dbt` install of any kind -- staleness is orthogonal to how
// the Baseline itself was resolved.
// ---------------------------------------------------------------------

#[cfg(unix)]
mod staleness_warning {
    use super::*;
    use std::process::Command as StdCommand;

    struct TestRepo {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl TestRepo {
        fn git(&self, args: &[&str]) {
            let output = StdCommand::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()
                .expect("git should be runnable in tests");
            assert!(
                output.status.success(),
                "git {args:?} should succeed: {output:?}"
            );
        }

        fn commit(&self, relative_path: &str, contents: &str, message: &str) {
            std::fs::write(self.path.join(relative_path), contents).expect("should write file");
            self.git(&["add", "."]);
            self.git(&["commit", "-m", message]);
        }
    }

    /// A repo with one commit on `master`, and a `feature` branch (left
    /// checked out) one commit ahead -- not yet stale, since `master`
    /// hasn't moved since `feature` diverged from it.
    fn up_to_date_repo() -> TestRepo {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().to_path_buf();
        let repo = TestRepo { _dir: dir, path };

        repo.git(&["init", "--initial-branch=master"]);
        repo.git(&["config", "user.email", "test@zhao.invalid"]);
        repo.git(&["config", "user.name", "zhao test"]);
        // Ignored up front so a later `git add .` (e.g. when committing on
        // `master` in `stale_repo`) never accidentally sweeps up the
        // untracked `target/manifest.json` written below -- a real dbt
        // project gitignores `target/` for the same reason.
        repo.commit(".gitignore", "target/\n", "ignore target/");
        repo.commit("README.md", "on master\n", "on master");
        repo.git(&["checkout", "-b", "feature"]);
        repo.commit("README.md", "on feature\n", "feature work, ahead of master");

        std::fs::create_dir_all(repo.path.join("target")).expect("should create target dir");
        std::fs::copy(
            fixture("clean_project")
                .join("target")
                .join("manifest.json"),
            repo.path.join("target").join("manifest.json"),
        )
        .expect("should copy fixture manifest");

        repo
    }

    /// Same as [`up_to_date_repo`], but with an extra commit landed on
    /// `master` after `feature` branched off -- `feature`'s merge-base
    /// with `master` is now behind `master`'s tip, i.e. stale.
    fn stale_repo() -> TestRepo {
        let repo = up_to_date_repo();
        repo.git(&["checkout", "master"]);
        repo.commit(
            "README.md",
            "on master, updated after feature branched off\n",
            "a new commit landed on master after feature branched off",
        );
        repo.git(&["checkout", "feature"]);
        repo
    }

    fn check_command(repo: &TestRepo) -> Command {
        let mut cmd = Command::cargo_bin("zhao").expect("binary should build");
        cmd.arg("check")
            .arg("--state")
            .arg(fixture("diff_baseline_manifest_clean.json"))
            .arg("--project-dir")
            .arg(&repo.path)
            // Determinism: without this, whether the report's text output
            // contains ANSI color codes would depend on the environment
            // these tests happen to run in (e.g. GitHub Actions, which
            // zhao-cli's own CI runs on, enables color even though stdout
            // isn't a real TTY there).
            .arg("--no-color");
        cmd
    }

    /// Acceptance criterion 1: a branch whose merge-base matches the
    /// target branch's current tip produces no staleness warning, in
    /// either output format.
    #[test]
    fn no_warning_when_the_merge_base_matches_the_target_branchs_tip() {
        let repo = up_to_date_repo();

        check_command(&repo)
            .arg("--format")
            .arg("json")
            .assert()
            .code(0)
            .stdout(predicate::str::contains("staleness_warning").not());

        check_command(&repo)
            .assert()
            .code(0)
            .stdout(predicate::str::contains("warning:").not());
    }

    /// Acceptance criterion 2: a branch whose merge-base is behind the
    /// target branch's current tip produces the warning, in both JSON and
    /// human-readable output.
    #[test]
    fn warns_when_the_merge_base_is_behind_the_target_branchs_tip() {
        let repo = stale_repo();

        check_command(&repo)
            .arg("--format")
            .arg("json")
            .assert()
            .code(0)
            .stdout(predicate::str::contains(
                "\"staleness_warning\": \"analysis may be stale, consider rebasing\"",
            ));

        check_command(&repo)
            .assert()
            .code(0)
            .stdout(predicate::str::contains(
                "warning: analysis may be stale, consider rebasing",
            ));
    }

    /// Acceptance criterion 3: the staleness warning never changes the
    /// exit code -- exercised with a fixture pair that has a *real*
    /// breaking Change (`join-cardinality-loosened`, `warn` by default but
    /// escalated to `error` under a `strict` Preset), so this test proves
    /// the exit code still tracks that finding's severity, staleness
    /// warning notwithstanding, rather than merely observing "nothing
    /// changed, so of course the exit code stayed 0" (which a fixture pair
    /// with zero Changes -- as used by the other two tests in this module
    /// -- would prove regardless of whether this feature worked at all).
    #[test]
    fn the_warning_never_changes_the_exit_code_even_under_a_strict_preset() {
        let repo = stale_repo();
        std::fs::create_dir_all(repo.path.join("target")).expect("should create target dir");
        std::fs::copy(
            fixture("rules_project")
                .join("target")
                .join("manifest.json"),
            repo.path.join("target").join("manifest.json"),
        )
        .expect("should overwrite with a fixture that has a real breaking change");
        std::fs::write(repo.path.join("zhao.yml"), "preset: strict\n")
            .expect("should write zhao.yml");

        let output = Command::cargo_bin("zhao")
            .expect("binary should build")
            .arg("check")
            .arg("--state")
            .arg(fixture("rules_baseline_manifest.json"))
            .arg("--project-dir")
            .arg(&repo.path)
            .arg("--format")
            .arg("json")
            .output()
            .expect("command should run");

        assert_eq!(
            output.status.code(),
            Some(1),
            "join-cardinality-loosened should be escalated to error by the strict Preset, \
             regardless of the simultaneous staleness warning; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
        assert_eq!(
            parsed["staleness_warning"], "analysis may be stale, consider rebasing",
            "the staleness warning should still be present alongside the breaking finding"
        );
        assert_eq!(
            parsed["findings"][0]["rule"], "join-cardinality-loosened",
            "the exit code should come from this finding's severity, not the staleness warning"
        );
        assert_eq!(parsed["findings"][0]["severity"], "error");
    }
}
