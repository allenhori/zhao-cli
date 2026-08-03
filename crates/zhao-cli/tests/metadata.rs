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

    for field in ["changes", "findings", "recommended_command"] {
        assert_eq!(
            stdout_json.get(field),
            metadata_json.get(field),
            "field {field:?} should match exactly between stdout JSON and run-metadata.json"
        );
    }
}

/// Acceptance criterion 3: no raw row data, connection strings, or
/// credentials appear anywhere in the file, verified by pinning the
/// exact top-level field set -- any future field added to
/// `RunMetadata`/`Report` without updating this test is a signal to stop
/// and check it's not accidentally introducing something sensitive.
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

    let top_level_fields: std::collections::BTreeSet<&str> = metadata_json
        .as_object()
        .expect("run-metadata.json should be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();

    let expected: std::collections::BTreeSet<&str> = [
        "changes",
        "findings",
        "recommended_command",
        "lineage_edges",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        top_level_fields, expected,
        "run-metadata.json's top-level field set changed -- if this is a deliberate \
         addition, confirm it can't ever carry raw row data, connection strings, or \
         credentials before updating this test"
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
