//! Integration tests for relative `dbt-command` paths in `zhao.yml`: a
//! real stub executable referenced by a relative path must actually run,
//! from any working directory, at whichever `zhao.yml` level set it.

#![cfg(unix)]

use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Writes `dbt_project.yml` with its mtime pinned to the Unix epoch so it
/// can never trip the current-manifest-freshness check (see `log.rs`).
fn write_dbt_project_marker(dir: &Path) {
    let path = dir.join("dbt_project.yml");
    std::fs::write(&path, "name: fixture\nversion: '1.0.0'\n").expect("should write marker");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("should reopen marker")
        .set_modified(std::time::SystemTime::UNIX_EPOCH)
        .expect("should pin mtime");
}

/// Writes an executable stub at `path` that writes a manifest and leaves
/// a `stub-ran` marker file in the directory it ran in.
fn write_stub(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("should create stub dir");
    std::fs::write(
        path,
        "#!/bin/sh\nmkdir -p target && echo '{}' > target/manifest.json\ntouch stub-ran\n",
    )
    .expect("should write stub");
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
    // See the ETXTBSY note on `stub_dbt_command` in `log.rs`.
    std::thread::sleep(std::time::Duration::from_millis(50));
}

fn run_lineage_compile(project_dir: &Path, cwd: &Path) -> std::process::Output {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .current_dir(cwd)
        .arg("lineage")
        .arg("--compile")
        .arg("--project-dir")
        .arg(project_dir)
        .output()
        .expect("command should run")
}

#[test]
fn root_level_relative_dbt_command_is_inherited_by_a_nested_project() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    write_stub(&repo.path().join(".venv/bin/dbt"));
    std::fs::write(repo.path().join("zhao.yml"), "dbt-command: .venv/bin/dbt\n").unwrap();
    let project = repo.path().join("analytics/project-a");
    std::fs::create_dir_all(&project).unwrap();
    write_dbt_project_marker(&project);
    // A project-local file that leaves dbt-command unset still inherits
    // the root's, anchored at the repo root.
    std::fs::write(project.join("zhao.yml"), "preset: strict\n").unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = run_lineage_compile(&project, elsewhere.path());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(project.join("stub-ran").exists());
}

#[test]
fn project_local_dbt_command_overrides_the_root_and_anchors_at_its_own_directory() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    // The root points at a path that does not exist; only the override works.
    std::fs::write(repo.path().join("zhao.yml"), "dbt-command: .venv/bin/dbt\n").unwrap();
    let project = repo.path().join("project-b");
    std::fs::create_dir_all(&project).unwrap();
    write_dbt_project_marker(&project);
    write_stub(&project.join("tools/dbt"));
    std::fs::write(project.join("zhao.yml"), "dbt-command: tools/dbt\n").unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = run_lineage_compile(&project, elsewhere.path());

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(project.join("stub-ran").exists());
}

#[test]
fn a_relative_dbt_command_pointing_nowhere_fails_naming_the_resolved_path() {
    let project = tempfile::tempdir().unwrap();
    write_dbt_project_marker(project.path());
    std::fs::write(
        project.path().join("zhao.yml"),
        "dbt-command: bin/missing\n",
    )
    .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = run_lineage_compile(project.path(), elsewhere.path());

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&project.path().join("bin/missing").display().to_string()),
        "error should name the resolved path, got: {stderr}"
    );
}
