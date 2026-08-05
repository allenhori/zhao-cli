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

/// Acceptance criterion: a column whose lineage couldn't be resolved
/// (`dim_customers.number_of_orders`, a `coalesce(...)`-computed
/// aggregate with no single upstream column) is reported as
/// "unresolved," not silently omitted or shown as if fully traced.
#[test]
fn an_unresolved_column_is_reported_as_unresolved_not_omitted() {
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
    assert!(stdout.contains("(unresolved)"), "{stdout}");
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
