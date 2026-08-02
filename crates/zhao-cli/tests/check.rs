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
            .arg(&repo.path);
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
    /// exit code, even under a `strict` Preset (which raises every Rule's
    /// Severity, including ones that would otherwise be non-breaking).
    #[test]
    fn the_warning_never_changes_the_exit_code_even_under_a_strict_preset() {
        let repo = stale_repo();
        std::fs::write(repo.path.join("zhao.yml"), "preset: strict\n")
            .expect("should write zhao.yml");

        check_command(&repo)
            .arg("--format")
            .arg("json")
            .assert()
            .code(0)
            .stdout(predicate::str::contains("\"staleness_warning\":"));
    }
}
