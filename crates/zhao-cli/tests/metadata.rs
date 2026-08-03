//! Integration tests for `target/zhao/run-metadata.json`: invokes the
//! actual compiled binary via `assert_cmd`, same convention as
//! `tests/check.rs`/`tests/diff.rs`.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

/// Copies `fixture_name` into a fresh temp dir so a test can run `zhao
/// check`/`zhao diff` against it and inspect (or clean up) the generated
/// `target/zhao/run-metadata.json` without mutating the checked-in
/// fixture directory itself.
fn copy_fixture_to_temp_dir(fixture_name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    copy_dir_recursive(&fixture(fixture_name), dir.path());
    // Other tests in this crate (`tests/check.rs`, `tests/diff.rs`) run
    // `zhao` directly against these same checked-in fixture directories,
    // so a prior test run may have already left a target/zhao/ behind on
    // the *source* fixture -- copied along with everything else above.
    // Strip it so every test here starts from a guaranteed-clean slate
    // regardless of what other tests happened to run first.
    let _ = std::fs::remove_dir_all(dir.path().join("target").join("zhao"));
    dir
}

fn copy_dir_recursive(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("should create dir");
    for entry in std::fs::read_dir(from).expect("should read dir") {
        let entry = entry.expect("should read dir entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("should get file type").is_dir() {
            copy_dir_recursive(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("should copy file");
        }
    }
}

/// Acceptance criterion 1: `target/zhao/run-metadata.json` is written on
/// every `zhao check`/`zhao diff` run.
#[test]
fn run_metadata_is_written_on_a_check_run() {
    let dir = copy_fixture_to_temp_dir("breaking_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .assert()
        .code(1);

    assert!(
        dir.path()
            .join("target")
            .join("zhao")
            .join("run-metadata.json")
            .exists(),
        "run-metadata.json should exist after a zhao check run"
    );
}

#[test]
fn run_metadata_is_written_on_a_diff_run() {
    let dir = copy_fixture_to_temp_dir("clean_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("diff")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest_clean.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .assert()
        .code(0);

    assert!(
        dir.path()
            .join("target")
            .join("zhao")
            .join("run-metadata.json")
            .exists(),
        "run-metadata.json should exist after a zhao diff run"
    );
}

/// Acceptance criterion 2: its contents match exactly what `--format
/// json` output describes -- no drift between the two.
#[test]
fn run_metadata_changes_and_findings_match_the_stdout_json_output_exactly() {
    let dir = copy_fixture_to_temp_dir("breaking_project");

    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .arg("--format")
        .arg("json")
        .output()
        .expect("command should run");

    let stdout_json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let metadata_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            dir.path()
                .join("target")
                .join("zhao")
                .join("run-metadata.json"),
        )
        .expect("should read run-metadata.json"),
    )
    .expect("run-metadata.json should be valid JSON");

    for field in [
        "changes",
        "findings",
        "staleness_warning",
        "recommended_command",
    ] {
        assert_eq!(
            stdout_json.get(field),
            metadata_json.get(field),
            "field {field:?} should match exactly between stdout JSON and run-metadata.json"
        );
    }
}

/// Recursively collects every object key appearing anywhere in `value`,
/// at any nesting depth -- not just the top level. A new field added
/// inside e.g. one `findings[]` element's variant, or inside
/// `lineage_edges[]`'s `upstream`/`column` objects, shows up here exactly
/// the same as a new top-level field would.
fn collect_all_keys(value: &serde_json::Value, keys: &mut std::collections::BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                keys.insert(key.clone());
                collect_all_keys(nested, keys);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_all_keys(item, keys);
            }
        }
        _ => {}
    }
}

/// Acceptance criterion 3: no raw row data, connection strings, or
/// credentials appear anywhere in the file, verified by pinning the
/// exact field set -- at every nesting depth, not just the top level, so
/// a new field added inside a `findings[]`/`lineage_edges[]` element is
/// caught exactly the same as a new top-level field. Any field added to
/// `RunMetadata`/`Report`/`LineageEdgeJson` without updating this test is
/// a signal to stop and check it's not accidentally introducing something
/// sensitive.
#[test]
fn run_metadata_json_field_set_is_exactly_this_and_nothing_else() {
    let dir = copy_fixture_to_temp_dir("breaking_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .assert()
        .code(1);

    let metadata_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            dir.path()
                .join("target")
                .join("zhao")
                .join("run-metadata.json"),
        )
        .expect("should read run-metadata.json"),
    )
    .expect("run-metadata.json should be valid JSON");

    let mut all_fields = std::collections::BTreeSet::new();
    collect_all_keys(&metadata_json, &mut all_fields);

    let expected: std::collections::BTreeSet<String> = [
        // Top level.
        "changes",
        "findings",
        "staleness_warning",
        "recommended_command",
        "lineage_edges",
        // ChangeJson / FindingJson variants' fields.
        "type",
        "rule",
        "severity",
        "node",
        "column",
        "from_type",
        "to_type",
        "position",
        "from_kind",
        "to_kind",
        "reached",
        "reached_column",
        // LineageEdgeJson / UpstreamJson / ColumnLineageJson fields.
        "upstream",
        "downstream",
        "kind",
        "id",
        "upstream_column",
        "downstream_column",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // A subset, not an exact match: some fields (`staleness_warning`,
    // `recommended_command`, `column` on a `LineageEdgeJson`, ...) are
    // conditionally present and this one fixture/scenario won't
    // necessarily surface all of them. What matters is that nothing
    // *outside* the allowlist ever appears.
    let unexpected: std::collections::BTreeSet<&String> =
        all_fields.difference(&expected).collect();
    assert!(
        unexpected.is_empty(),
        "run-metadata.json contains field(s) not on the allowlist: {unexpected:?} -- if \
         this is a deliberate addition, confirm it can't ever carry raw row data, \
         connection strings, or credentials before adding it to this test's allowlist"
    );
}

/// Acceptance criterion 3, continued: no raw row data, connection
/// strings, or credentials anywhere in the file -- and criterion 4: no
/// reference to any cloud service, API, or external endpoint. Scans the
/// full serialized text for characteristic substrings rather than only
/// checking field names, since a field could theoretically carry a
/// sensitive value even under an innocuous-sounding key.
#[test]
fn run_metadata_contains_no_credentials_row_data_or_cloud_references() {
    let dir = copy_fixture_to_temp_dir("breaking_project");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .assert()
        .code(1);

    let contents = std::fs::read_to_string(
        dir.path()
            .join("target")
            .join("zhao")
            .join("run-metadata.json"),
    )
    .expect("should read run-metadata.json");
    let lowercase = contents.to_lowercase();

    for forbidden in [
        "http://",
        "https://",
        "password",
        "token",
        "secret",
        "credential",
        "api_key",
        "apikey",
        "connection_string",
        ".zhao.dev",
        ".zhao.io",
        "zhaocloud",
    ] {
        assert!(
            !lowercase.contains(forbidden),
            "run-metadata.json must not contain {forbidden:?}, but it did: {contents}"
        );
    }
}

/// A failure to write run-metadata.json (here: a read-only `target/`)
/// must not change the process exit code -- by the time it's attempted,
/// the report has already been printed and the real gate result already
/// computed, so a sidecar file failing to write shouldn't turn an
/// otherwise-correct "breaking change found" (exit 1) into "zhao itself
/// failed" (exit 2). A warning should still reach stderr, though, so the
/// failure isn't silently swallowed either.
#[cfg(unix)]
#[test]
fn a_run_metadata_write_failure_does_not_change_the_exit_code() {
    use std::os::unix::fs::PermissionsExt;

    let dir = copy_fixture_to_temp_dir("breaking_project");
    let target_dir = dir.path().join("target");
    let mut permissions = std::fs::metadata(&target_dir)
        .expect("should stat target dir")
        .permissions();
    permissions.set_mode(0o500); // read + execute, no write
    std::fs::set_permissions(&target_dir, permissions.clone())
        .expect("should chmod target dir read-only");

    let result = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--state")
        .arg(fixture("diff_baseline_manifest.json"))
        .arg("--project-dir")
        .arg(dir.path())
        .output();

    // Restore write permission so the temp dir can be cleaned up
    // regardless of what the assertions below find.
    permissions.set_mode(0o700);
    std::fs::set_permissions(&target_dir, permissions).expect("should restore permissions");

    let output = result.expect("command should run");
    assert_eq!(
        output.status.code(),
        Some(1),
        "a real breaking change should still exit 1 even though run-metadata.json \
         couldn't be written; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not write run metadata"),
        "the failure should still be reported on stderr, not silently swallowed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
