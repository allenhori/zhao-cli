//! Integration tests for `zhao update` (issue #28): invokes the actual
//! compiled binary end to end against the real, public GitHub Releases
//! API -- there's no local fixture to point this at, since the whole
//! point of the command is fetching a real release. Requires network
//! access, same as `cargo build`'s own crates.io downloads already do.

use assert_cmd::Command;
use predicates::prelude::*;

/// Copies the just-built `zhao` binary to a fresh executable temp file
/// and returns its path -- a `TempPath` (not a `NamedTempFile`), since
/// a `NamedTempFile` keeps its own file descriptor open for the guard's
/// entire lifetime, and executing a file that's still open for writing
/// fails with `ETXTBSY` ("Text file busy") on Linux (silently fine on
/// macOS, which doesn't enforce this -- the exact reason this needs
/// its own helper rather than the more obvious `NamedTempFile::new()`
/// left open). `into_temp_path()` closes that descriptor while still
/// keeping the file alive (and cleaned up on drop) via its path alone.
fn copy_zhao_binary_to_a_fresh_temp_file() -> tempfile::TempPath {
    let original = Command::cargo_bin("zhao")
        .expect("binary should build")
        .get_program()
        .to_os_string();
    let temp_file = tempfile::NamedTempFile::new().expect("should create temp file");
    std::fs::copy(&original, temp_file.path()).expect("should copy the built binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(temp_file.path())
            .expect("should stat copy")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(temp_file.path(), perms).expect("should chmod copy");
    }
    temp_file.into_temp_path()
}

/// Acceptance criterion: `zhao update <version>` pins to that exact
/// release, actually replacing the binary on disk. Pinned to a real,
/// permanent tag (`v0.1.0`) rather than "latest," so this test's
/// expectations don't drift as new stable releases are cut.
#[test]
fn update_to_a_pinned_version_replaces_the_binary() {
    let temp_copy = copy_zhao_binary_to_a_fresh_temp_file();
    let before = std::fs::read(&temp_copy).expect("should read the pre-update binary");

    let status = std::process::Command::new(&temp_copy)
        .arg("update")
        .arg("v0.1.0")
        .status()
        .expect("command should run");
    assert!(status.success(), "zhao update v0.1.0 should succeed");

    let after = std::fs::read(&temp_copy).expect("should read the post-update binary");
    assert_ne!(
        before, after,
        "the binary's contents should have actually changed"
    );
}

/// Acceptance criterion: a clear, actionable error when the requested
/// tag doesn't exist -- and the existing binary is left untouched.
#[test]
fn update_to_a_nonexistent_tag_produces_a_clear_error_and_leaves_the_binary_untouched() {
    let temp_copy = copy_zhao_binary_to_a_fresh_temp_file();
    let before = std::fs::read(&temp_copy).expect("should read the pre-update binary");

    Command::from_std(std::process::Command::new(&temp_copy))
        .arg("update")
        .arg("v99.99.99-does-not-exist")
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("v99.99.99-does-not-exist")),
        );

    let after = std::fs::read(&temp_copy).expect("should read the binary after the failed update");
    assert_eq!(
        before, after,
        "a failed update should never leave a partial/broken binary in place"
    );
}

/// `--nightly` and a pinned version are mutually exclusive.
#[test]
fn nightly_and_a_version_argument_are_mutually_exclusive() {
    Command::cargo_bin("zhao")
        .expect("binary should build")
        .arg("update")
        .arg("v0.1.0")
        .arg("--nightly")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}
