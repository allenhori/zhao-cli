//! Integration tests for Addon dispatch (`addon.rs`): `zhao <name>`
//! falls through to a `zhao-<name>` binary on `PATH` when `<name>` isn't
//! one of `zhao`'s own built-in subcommands.
//!
//! Stub "addon" binaries are symlinks to already-existing, already-
//! installed system binaries (`/usr/bin/echo`, `/usr/bin/false`), never
//! a freshly-written script this test process creates and immediately
//! execs -- see zhao-dbt-plan's `select.rs` test module (a sibling
//! project's own test suite) for why: writing a script, chmod'ing it,
//! then exec'ing it flaked with `ETXTBSY` ("Text file busy") on Linux
//! CI even with an explicit flush-and-close before returning. A symlink
//! to a binary that was never freshly written by this process at all
//! sidesteps that whole class of race, and real system binaries are a
//! perfectly adequate stand-in for verifying dispatch itself (argument
//! forwarding, stdout passthrough, exit code passthrough) -- the actual
//! Addon *logic* is `zhao-dbt-plan`'s own concern, not this test's.

use assert_cmd::Command;
use std::path::PathBuf;

/// The first of these that exists is used as the stub addon target.
/// Covers both common locations across Linux distributions and macOS.
fn find_real_binary(names: &[&str]) -> PathBuf {
    for name in names {
        let path = PathBuf::from(name);
        if path.is_file() {
            return path;
        }
    }
    panic!("none of {names:?} exist on this system -- test environment assumption violated");
}

/// A tempdir on `PATH` (for the child `zhao` process only, via
/// `Command::env` -- never mutates this test process's own real `PATH`)
/// containing a symlink named `zhao-<addon_name>` pointing at
/// `real_binary`.
fn path_dir_with_addon(addon_name: &str, real_binary: &PathBuf) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create tempdir");
    std::os::unix::fs::symlink(real_binary, dir.path().join(format!("zhao-{addon_name}")))
        .expect("should create symlink");
    dir
}

#[test]
fn an_addon_on_path_receives_forwarded_arguments_and_stdout_passes_through() {
    let echo = find_real_binary(&["/usr/bin/echo", "/bin/echo"]);
    let path_dir = path_dir_with_addon("echo-stub", &echo);

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("echo-stub")
        .arg("hello")
        .arg("world")
        .env("PATH", path_dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("hello world"));
}

#[test]
fn an_addons_non_zero_exit_code_passes_through_unmodified() {
    let false_bin = find_real_binary(&["/usr/bin/false", "/bin/false"]);
    let path_dir = path_dir_with_addon("false-stub", &false_bin);

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("false-stub")
        .env("PATH", path_dir.path())
        .assert()
        .code(1);
}

#[test]
fn an_unknown_subcommand_with_no_matching_addon_still_produces_clap_own_error() {
    let empty_dir = tempfile::tempdir().expect("should create tempdir");

    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("totally-not-a-real-subcommand-or-addon")
        .env("PATH", empty_dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("unrecognized"));
}

#[test]
fn a_builtin_subcommand_name_is_never_treated_as_an_addon_even_if_one_exists_on_path() {
    // A malicious or confused PATH entry named "zhao-check" must never
    // shadow the real `zhao check` -- built-ins always win.
    let echo = find_real_binary(&["/usr/bin/echo", "/bin/echo"]);
    let path_dir = path_dir_with_addon("check", &echo);

    let output = Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("check")
        .arg("--project-dir")
        .arg("/nonexistent-path-for-this-test")
        .env("PATH", path_dir.path())
        .output()
        .expect("command should run");

    // The real `zhao check` fails fast on a bad --project-dir with its
    // own EXIT_ERROR (2); the echo stub would have exited 0 and printed
    // its arguments instead -- confirms the built-in ran, not the stub.
    assert_eq!(output.status.code(), Some(2));
}
