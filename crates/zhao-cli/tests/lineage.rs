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

/// Writes the marker file adapter auto-detection looks for
/// (`dbt_project.yml`) into `dir`, with its mtime pinned to the Unix
/// epoch -- far older than any manifest a test copies in afterward, so
/// it can never trip the unrelated current-manifest-freshness check.
/// Pinning matters specifically because `std::fs::copy` (used below to
/// bring in a real fixture manifest) can preserve the *source* file's
/// original mtime on some platforms/filesystems (e.g. an APFS
/// `clonefile` copy) rather than stamping "now" -- writing the marker
/// first isn't reliably enough on its own to guarantee it looks older.
fn write_dbt_project_marker(dir: &std::path::Path) {
    let path = dir.join("dbt_project.yml");
    std::fs::write(&path, "name: fixture\nversion: '1.0.0'\n")
        .expect("should write dbt_project.yml marker");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("should reopen dbt_project.yml")
        .set_modified(std::time::SystemTime::UNIX_EPOCH)
        .expect("should set an old mtime on dbt_project.yml");
}

/// Acceptance criterion: a bare target shows both upstream and downstream.
#[test]
fn bare_target_shows_both_upstream_and_downstream() {
    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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

/// `--package` disambiguates a `model.column` target too, not just a
/// bare model target -- both `run_text` and `run_html` thread it into
/// `trace_column`, not just `trace`.
#[test]
fn a_package_flag_disambiguates_a_column_level_target() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers.id")
        .arg("--package")
        .arg("pkg_b")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(0);
}

/// `--package` disambiguates `--html`'s initial target too, scoping the
/// export to the correct package's model -- not just text output.
#[test]
fn a_package_flag_disambiguates_the_html_export_initial_target() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("out.html");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--html")
        .arg(&out)
        .arg("customers")
        .arg("--package")
        .arg("pkg_a")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(0);

    let html = std::fs::read_to_string(&out).expect("should read generated file");
    assert!(html.contains("\"initial_target\":\"model.pkg_a.customers\""));
}

/// An empty `--package ""` never matches a real package segment (dbt
/// never produces one), so it behaves the same as a package that
/// doesn't apply at all -- `UnknownTarget`, not a silent fallback to
/// unfiltered (still-ambiguous) matching.
#[test]
fn an_empty_package_flag_produces_unknown_target_not_a_silent_fallback() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers")
        .arg("--package")
        .arg("")
        .arg("--project-dir")
        .arg(fixture("ambiguous_package_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("no model named")));
}

/// Acceptance criterion: a model with no upstream/downstream connections
/// produces a clear "nothing found" result, not an error -- exercised
/// against a throwaway single-model project built for this test (no
/// existing fixture happens to have a truly isolated model).
#[test]
fn a_model_with_no_connections_produces_a_clear_nothing_found_result_not_an_error() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(dir.path().join("target")).expect("should create target dir");
    write_dbt_project_marker(dir.path());
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
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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
        .arg("--text")
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

/// Acceptance criterion: `--text` still requires a target -- HTML being
/// the default doesn't change `--text`'s own contract.
#[test]
fn text_with_no_target_produces_a_clear_error() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
        .arg("--project-dir")
        .arg(fixture("rules_project"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("error:"));
}

// ---------------------------------------------------------------------
// HTML as the default output (issue #38): no `--text`/`--html` at all.
// ---------------------------------------------------------------------

/// Acceptance criterion: with no `--text`/`--html`, `zhao lineage`
/// produces HTML (not text) at the computed default path under
/// `target/zhao/lineage_graphs/` -- a bare invocation with no target at
/// all now succeeds (HTML mode needs no target), rather than the old
/// "target required" error.
#[test]
fn default_mode_with_no_target_writes_full_lineage_html() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("full_lineage.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
    let html = std::fs::read_to_string(&expected).expect("should read generated file");
    assert!(html.contains("model.zhao_dbt_test.dim_customers"));
}

/// Acceptance criterion: the default-mode filename table -- a bare model
/// target.
#[test]
fn default_mode_with_a_bare_target_uses_the_partial_lineage_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_stg_customers.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: the default-mode filename table -- `+<model>`
/// gets the `_upstream_only` suffix.
#[test]
fn default_mode_with_a_plus_prefix_target_uses_the_upstream_only_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("+dim_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_dim_customers_upstream_only.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: the default-mode filename table -- `<model>+`
/// gets the `_downstream_only` suffix.
#[test]
fn default_mode_with_a_plus_suffix_target_uses_the_downstream_only_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("stg_orders+")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_stg_orders_downstream_only.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: the default-mode filename table -- a bare
/// `<model>.<column>` target appends the column name, no direction
/// suffix.
#[test]
fn default_mode_with_a_column_target_uses_the_column_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("stg_customers.customer_id")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_stg_customers_customer_id.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: the default-mode filename table --
/// `+<model>.<column>` appends the column name, then the
/// `_upstream_only` suffix.
#[test]
fn default_mode_with_a_plus_prefix_column_target_uses_the_column_upstream_only_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
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
        .arg("+stg_customers.customer_id")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_stg_customers_customer_id_upstream_only.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: the default-mode filename table -- a
/// `--package`-qualified target prepends the package name.
#[test]
fn default_mode_with_a_package_flag_prepends_the_package_to_the_filename() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
    std::fs::copy(
        fixture("ambiguous_package_project")
            .join("target")
            .join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("customers")
        .arg("--package")
        .arg("pkg_b")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("partial_lineage_pkg_b_customers.html");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: `--html <path>` still works as an explicit
/// override of the computed default path.
#[test]
fn an_explicit_html_path_overrides_the_computed_default() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let out = dir.path().join("custom.html");

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

    assert!(out.exists(), "expected {} to exist", out.display());
}

// ---------------------------------------------------------------------
// `target/zhao/full_lineage.json` -- always written, whole project
// (issue #39).
// ---------------------------------------------------------------------

fn copy_manifest_into(project_dir: &std::path::Path, source_fixture: &str) {
    std::fs::create_dir_all(project_dir.join("target")).expect("should create target dir");
    write_dbt_project_marker(project_dir);
    std::fs::copy(
        fixture(source_fixture).join("target").join("manifest.json"),
        project_dir.join("target").join("manifest.json"),
    )
    .expect("should copy manifest");
}

/// Acceptance criterion: every invocation writes/overwrites
/// `target/zhao/full_lineage.json`, regardless of `--text`/default HTML.
#[test]
fn full_lineage_json_is_written_in_text_mode() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    copy_manifest_into(project_dir, "rules_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--text")
        .arg("stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("full_lineage.json");
    assert!(
        expected.exists(),
        "expected {} to exist",
        expected.display()
    );
}

/// Acceptance criterion: written regardless of a target being given at
/// all, and its content is the whole project's graph either way -- not a
/// per-target variant.
#[test]
fn full_lineage_json_content_is_independent_of_target_scoping() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    copy_manifest_into(project_dir, "rules_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("+stg_customers")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let path = project_dir
        .join("target")
        .join("zhao")
        .join("full_lineage.json");
    let scoped_json = std::fs::read_to_string(&path).expect("should read full_lineage.json");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let whole_project_json = std::fs::read_to_string(&path).expect("should read full_lineage.json");

    // Compared as sets, not raw strings: `edges`' iteration order isn't
    // guaranteed stable across separate process invocations (unrelated,
    // pre-existing nondeterminism upstream in how the adapter builds the
    // edge list) -- what #39 actually promises is the same *content*
    // regardless of target, not the same byte-for-byte serialization.
    fn as_sorted_json_strings(json: &str) -> (Vec<String>, Vec<String>) {
        let value: serde_json::Value = serde_json::from_str(json).expect("should parse as JSON");
        let mut nodes: Vec<String> = value["nodes"]
            .as_array()
            .expect("nodes should be an array")
            .iter()
            .map(|n| n.to_string())
            .collect();
        let mut edges: Vec<String> = value["edges"]
            .as_array()
            .expect("edges should be an array")
            .iter()
            .map(|e| e.to_string())
            .collect();
        nodes.sort();
        edges.sort();
        (nodes, edges)
    }

    assert_eq!(
        as_sorted_json_strings(&scoped_json),
        as_sorted_json_strings(&whole_project_json),
        "full_lineage.json must contain the same nodes/edges regardless of what target was requested"
    );

    for model in [
        "model.zhao_dbt_test.stg_customers",
        "model.zhao_dbt_test.stg_orders",
        "model.zhao_dbt_test.stg_payments",
        "model.zhao_dbt_test.dim_customers",
        "model.zhao_dbt_test.fct_orders",
        "model.zhao_dbt_test.fct_orders_incremental",
    ] {
        assert!(
            whole_project_json.contains(model),
            "{model} missing from full_lineage.json"
        );
    }
}

/// `full_lineage.json` is a genuinely separate file from whatever HTML
/// export was requested -- not the same blob embedded in the HTML page's
/// own JS.
#[test]
fn full_lineage_json_is_separate_from_the_html_export() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    copy_manifest_into(project_dir, "rules_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(0);

    let json_path = project_dir
        .join("target")
        .join("zhao")
        .join("full_lineage.json");
    let html_path = project_dir
        .join("target")
        .join("zhao")
        .join("lineage_graphs")
        .join("full_lineage.html");

    assert!(
        json_path.exists(),
        "expected {} to exist",
        json_path.display()
    );
    assert!(
        html_path.exists(),
        "expected {} to exist",
        html_path.display()
    );

    let json = std::fs::read_to_string(&json_path).expect("should read json");
    // Genuinely valid, standalone JSON -- not merely a fragment cut out
    // of the HTML page.
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("should parse as JSON");
    assert!(parsed.get("nodes").is_some());
    assert!(parsed.get("edges").is_some());
}

/// Written even when the requested target fails to resolve -- it's
/// target-independent, so there's no reason to withhold it just because
/// what was asked for on top of it failed.
#[test]
fn full_lineage_json_is_written_even_when_the_target_fails_to_resolve() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let project_dir = dir.path();
    copy_manifest_into(project_dir, "rules_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("lineage")
        .arg("does_not_exist")
        .arg("--project-dir")
        .arg(project_dir)
        .assert()
        .code(2);

    let expected = project_dir
        .join("target")
        .join("zhao")
        .join("full_lineage.json");
    assert!(
        expected.exists(),
        "expected {} to exist even on a failed target resolution",
        expected.display()
    );
}
