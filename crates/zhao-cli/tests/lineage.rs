//! Integration tests for the `zhao lineage` command boundary: invokes the
//! actual compiled binary against a real fixture project's compiled
//! manifest (`rules_project`) and asserts on stdout and exit code.
//!
//! `rules_project`'s dependency shape: `raw_customers`/`raw_orders`/
//! `raw_payments` (sources) -> `stg_customers`/`stg_orders`/`stg_payments`
//! -> `dim_customers` (from `stg_customers` + `stg_orders`) and
//! `fct_orders`/`fct_orders_incremental` (from `stg_orders` +
//! `stg_payments`).

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

/// Acceptance criterion: a bare target shows both upstream and downstream.
#[test]
fn bare_target_shows_both_upstream_and_downstream() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_orders")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");

    assert!(stdout.contains("Upstream:\n"), "{stdout}");
    assert!(
        stdout.contains("source source.zhao_dbt_test.raw.raw_orders"),
        "{stdout}"
    );
    assert!(stdout.contains("Downstream:\n"), "{stdout}");
    assert!(
        stdout.contains("model model.zhao_dbt_test.dim_customers"),
        "{stdout}"
    );
    assert!(
        stdout.contains("model model.zhao_dbt_test.fct_orders"),
        "{stdout}"
    );
}

/// Acceptance criterion: `+<model>` shows only upstream.
#[test]
fn plus_prefix_shows_only_upstream() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("+dim_customers")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Upstream:\n")
                .and(predicate::str::contains(
                    "model model.zhao_dbt_test.stg_customers",
                ))
                .and(predicate::str::contains(
                    "model model.zhao_dbt_test.stg_orders",
                ))
                .and(predicate::str::contains("Downstream:\n").not()),
        );
}

/// Acceptance criterion: `<model>+` shows only downstream.
#[test]
fn plus_suffix_shows_only_downstream() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_orders+")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Downstream:\n")
                .and(predicate::str::contains(
                    "model model.zhao_dbt_test.dim_customers",
                ))
                .and(predicate::str::contains("Upstream:\n").not()),
        );
}

/// Acceptance criterion: an unknown model name produces a clear,
/// actionable error, not a silent empty result.
#[test]
fn unknown_target_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("does_not_exist")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("does_not_exist")));
}

/// Acceptance criterion: a target ambiguous by bare name alone (two
/// same-named models across different dbt packages) is reported clearly,
/// and the error names every candidate's package -- exactly what
/// `--package` expects.
#[test]
fn an_ambiguous_target_without_a_package_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("more than one model"))
                .and(predicate::str::contains("model.pkg_a.customers"))
                .and(predicate::str::contains("model.pkg_b.customers")),
        );
}

/// Acceptance criterion: `--package` disambiguates an otherwise-ambiguous
/// target and resolves successfully.
#[test]
fn a_package_flag_disambiguates_an_ambiguous_target() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers")
        .arg("--package")
        .arg("pkg_b")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Upstream:\n  (none)")
                .and(predicate::str::contains("Downstream:\n  (none)")),
        );
}

/// A `--package` that matches no candidate for the given name is reported
/// the same as an unknown target, not silently ignored.
#[test]
fn a_package_flag_matching_no_candidate_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers")
        .arg("--package")
        .arg("does_not_exist")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("no model named")));
}

/// `--package` on an already-unambiguous target has no effect -- it's
/// purely a disambiguator, never a requirement.
#[test]
fn a_package_flag_on_an_unambiguous_target_still_resolves() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_customers")
        .arg("--package")
        .arg("zhao_dbt_test")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0);
}

/// Acceptance criterion: a model with no upstream/downstream connections
/// produces a clear "nothing found" result, not an error -- exercised
/// against a throwaway single-model project built for this test (no
/// existing fixture happens to have a truly isolated model).
#[test]
fn a_model_with_no_connections_produces_a_clear_nothing_found_result_not_an_error() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(dir.path().join("target")).expect("should create target dir");
    std::fs::write(
        dir.path().join("target").join("manifest.json"),
        r#"{
            "metadata": {},
            "sources": {},
            "nodes": {
                "model.p.isolated": {
                    "unique_id": "model.p.isolated",
                    "resource_type": "model",
                    "name": "isolated",
                    "database": "db",
                    "schema": "public",
                    "alias": "isolated",
                    "compiled_code": "select 1 as id",
                    "config": {"materialized": "view"}
                }
            }
        }"#,
    )
    .expect("should write manifest");

    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("isolated")
        .arg("--project-dir")
        .arg(dir.path())
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("(none)"), "{stdout}");
}

// ---------------------------------------------------------------------
// Column-level lineage (`model.column` targets).
// ---------------------------------------------------------------------

/// Acceptance criterion: a bare `model.column` target shows both
/// upstream and downstream columns actually connected via resolved
/// column-level lineage -- `stg_customers.customer_id` traces to
/// `raw_customers.id` upstream (a real rename, not just "some column on
/// the source") and `dim_customers.customer_id` downstream.
#[test]
fn bare_column_target_shows_resolved_upstream_and_downstream_columns() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_customers.customer_id")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");

    assert!(stdout.contains("Upstream:\n"), "{stdout}");
    assert!(
        stdout.contains("source source.zhao_dbt_test.raw.raw_customers.id"),
        "{stdout}"
    );
    assert!(stdout.contains("Downstream:\n"), "{stdout}");
    assert!(
        stdout.contains("model model.zhao_dbt_test.dim_customers.customer_id"),
        "{stdout}"
    );
}

/// Acceptance criterion: `+<model>.<column>` restricts to upstream only.
#[test]
fn plus_prefix_on_a_column_target_shows_only_upstream() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("+stg_customers.customer_id")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Upstream:\n")
                .and(predicate::str::contains(
                    "source.zhao_dbt_test.raw.raw_customers.id",
                ))
                .and(predicate::str::contains("Downstream:\n").not()),
        );
}

/// Acceptance criterion: `<model>.<column>+` restricts to downstream
/// only.
#[test]
fn plus_suffix_on_a_column_target_shows_only_downstream() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_customers.customer_id+")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("Downstream:\n")
                .and(predicate::str::contains(
                    "model.zhao_dbt_test.dim_customers.customer_id",
                ))
                .and(predicate::str::contains("Upstream:\n").not()),
        );
}

/// Acceptance criterion: a calculated column that wraps a single upstream
/// column reference through nested function calls and CTE hops
/// (`dim_customers.number_of_orders` is `coalesce(customer_orders.number_of_orders,
/// 0)`, where `customer_orders.number_of_orders` is itself `count(order_id)`
/// inside an earlier CTE) is traced all the way back to the real upstream
/// column, not reported as unresolved.
#[test]
fn a_calculated_column_traces_through_nested_functions_and_cte_hops() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("dim_customers.number_of_orders")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("model.zhao_dbt_test.stg_orders.order_id"),
        "{stdout}"
    );
    assert!(!stdout.contains("(unresolved)"), "{stdout}");
}

/// Acceptance criterion: an unknown `model.column` target produces a
/// clear, actionable error.
#[test]
fn an_unknown_column_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_customers.does_not_exist")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("stg_customers"))
                .and(predicate::str::contains("does_not_exist")),
        );
}

/// Acceptance criterion: model-level targets (the tracer bullet)
/// continue to work unchanged alongside the new column-level capability.
#[test]
fn model_level_targets_still_work_unchanged() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .output()
        .expect("command should run");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("model model.zhao_dbt_test.dim_customers\n"),
        "{stdout}"
    );
}

// ---------------------------------------------------------------------
// `--html`: self-contained interactive lineage export.
// ---------------------------------------------------------------------

/// Acceptance criterion: `zhao lineage --html out.html` with no target
/// embeds the whole project's lineage graph.
#[test]
fn html_with_no_target_embeds_the_whole_project() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0);

    let html = std::fs::read_to_string(&out).expect("should read generated file");
    for model in [
        "model.zhao_dbt_test.stg_customers",
        "model.zhao_dbt_test.stg_orders",
        "model.zhao_dbt_test.stg_payments",
        "model.zhao_dbt_test.dim_customers",
        "model.zhao_dbt_test.fct_orders",
        "model.zhao_dbt_test.fct_orders_incremental",
    ] {
        assert!(html.contains(model), "{model} missing from export");
    }
}

/// Acceptance criterion: `zhao lineage --html out.html <model>` scopes
/// the initial view to that target.
#[test]
fn html_with_a_model_target_scopes_the_initial_view() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0);

    let html = std::fs::read_to_string(&out).expect("should read generated file");
    assert!(html.contains("\"initial_target\":\"model.zhao_dbt_test.stg_customers\""));
    assert!(!html.contains("\"initial_column\":"));
}

/// Acceptance criterion: `zhao lineage --html out.html <model>.<column>`
/// scopes the initial view at the column grain too.
#[test]
fn html_with_a_column_target_scopes_the_initial_view_at_column_grain() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("stg_customers.customer_id")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0);

    let html = std::fs::read_to_string(&out).expect("should read generated file");
    assert!(html.contains("\"initial_target\":\"model.zhao_dbt_test.stg_customers\""));
    assert!(html.contains("\"initial_column\":\"customer_id\""));
}

/// Acceptance criterion: an unknown/ambiguous/unknown-column target
/// fails the same way `--html` mode as it does for text output, rather
/// than silently generating a file with nothing pre-selected.
#[test]
fn html_with_an_unknown_target_produces_a_clear_error_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("does_not_exist")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does_not_exist"));

    assert!(
        !out.exists(),
        "no file should be written on a failed target resolution"
    );
}

/// Acceptance criterion: the generated file is fully self-contained --
/// no `http://`/`https://` reference anywhere in its output (beyond the
/// SVG namespace URI, which is never fetched over the network).
#[test]
fn html_export_is_fully_self_contained() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(0);

    let html = std::fs::read_to_string(&out).expect("should read generated file");
    let without_svg_namespace = html.replace("http://www.w3.org/2000/svg", "");
    assert!(
        !without_svg_namespace.contains("http://") && !without_svg_namespace.contains("https://"),
        "export must be fully self-contained: {without_svg_namespace}"
    );
}

/// Acceptance criterion (implied, shared with text output): a target is
/// required unless `--html` is given -- bare `zhao lineage` with neither
/// produces a clear error, not a panic or silent no-op.
#[test]
fn no_target_and_no_html_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:"));
}
