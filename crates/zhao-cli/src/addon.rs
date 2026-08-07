//! Generic Addon dispatch (see ADR 0010 in the `zhao` planning repo):
//! when `zhao` is invoked with a subcommand that isn't one of its own
//! built-ins, look for a `zhao-<subcommand>` binary on `PATH` before
//! falling through to clap's own "unknown subcommand" error -- the same
//! pattern `git` itself uses for `git <custom-command>` ->
//! `git-<custom-command>`.
//!
//! `zhao-cli` has zero compiled-in knowledge of any specific Addon (no
//! registry of known names, no validation of what the binary actually
//! is) -- it only knows the naming/discovery convention. See
//! `zhao-dbt-plan` (github.com/allenhori/zhao-dbt-plan) for the first
//! real Addon, and `examples/hello-zhao-addon/` in this repo for a
//! minimal reference implementation of the same contract.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// `zhao`'s own built-in subcommand names -- checked before searching
/// `PATH` for an Addon, so a genuine typo of a built-in still gets
/// clap's normal "unknown subcommand" error (with its own
/// did-you-mean suggestion) instead of a confusing "no such Addon"
/// message. `"help"` is included since clap auto-generates that
/// subcommand too.
const BUILTIN_SUBCOMMANDS: &[&str] = &["check", "diff", "lineage", "update", "help"];

/// Whether `name` is one of `zhao`'s own built-in subcommands (not an
/// Addon candidate at all).
pub(crate) fn is_builtin_subcommand(name: &str) -> bool {
    BUILTIN_SUBCOMMANDS.contains(&name)
}

/// Searches `PATH` for an executable named `zhao-<name>` (`zhao-<name>.exe`
/// on Windows), returning its path if found. Doesn't validate that the
/// file is actually executable beyond it existing as a regular file --
/// a non-executable match still gets tried and fails with the OS's own
/// clear "permission denied"/"not executable" error from [`dispatch`],
/// rather than silently skipped.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_in_path_var(name, &path_var)
}

/// The testable core of [`find_on_path`], taking the `PATH`-shaped value
/// to search as a parameter -- kept separate so tests can pass a
/// controlled directory list instead of mutating the real process-global
/// `PATH`, which other tests in this same test binary (git-native
/// Baseline resolution, `dbt` subprocess calls, ...) genuinely rely on
/// to find real executables and would break if PATH were rewritten out
/// from under them by a concurrently-running test.
fn find_in_path_var(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    let binary_name = addon_binary_name(name);
    std::env::split_paths(path_var)
        .map(|dir| dir.join(&binary_name))
        .find(|candidate| candidate.is_file())
}

#[cfg(not(target_os = "windows"))]
fn addon_binary_name(name: &str) -> String {
    format!("zhao-{name}")
}

#[cfg(target_os = "windows")]
fn addon_binary_name(name: &str) -> String {
    format!("zhao-{name}.exe")
}

/// Runs the Addon binary at `path`, forwarding `args` verbatim and
/// passing through its exit code, stdout, and stderr unmodified (this
/// process's own stdout/stderr are inherited by the child, not
/// captured/re-printed, so there's no buffering delay or reformatting).
///
/// If the Addon can't even be spawned (permissions, not actually
/// executable despite matching the naming convention, ...), reports a
/// clear error to stderr and returns exit code `2` -- the same "zhao
/// itself couldn't run this" code `zhao check`/`zhao diff` use (see
/// `engine::EXIT_ERROR`) for their own unrelated failure modes.
pub(crate) fn dispatch(path: &Path, args: &[String]) -> ExitCode {
    match Command::new(path).args(args).status() {
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(code as u8),
            // No exit code at all means the child was killed by a
            // signal (Unix-only) -- there's no faithful single-byte
            // exit code to forward, so this maps to the same "zhao
            // itself couldn't run this" code other unrecoverable
            // failures use.
            None => ExitCode::from(2),
        },
        Err(err) => {
            eprintln!("error: could not run addon {}: {err}", path.display());
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_real_subcommand_is_recognized_as_builtin() {
        for name in ["check", "diff", "lineage", "update", "help"] {
            assert!(is_builtin_subcommand(name), "{name} should be builtin");
        }
    }

    #[test]
    fn an_addon_style_name_is_not_builtin() {
        assert!(!is_builtin_subcommand("dbt-plan"));
    }

    #[test]
    fn find_in_path_var_locates_a_matching_executable() {
        let dir = tempfile::tempdir().expect("should create tempdir");
        let binary_path = dir.path().join(addon_binary_name("dbt-plan"));
        std::fs::write(&binary_path, "").expect("should write stub");

        let path_var = std::env::join_paths([dir.path()]).expect("should join paths");
        let found = find_in_path_var("dbt-plan", &path_var);
        assert_eq!(found.as_deref(), Some(binary_path.as_path()));
    }

    #[test]
    fn find_in_path_var_returns_none_when_nothing_matches() {
        let dir = tempfile::tempdir().expect("should create tempdir");
        let path_var = std::env::join_paths([dir.path()]).expect("should join paths");
        assert_eq!(find_in_path_var("nonexistent-addon-xyz", &path_var), None);
    }

    #[test]
    fn find_in_path_var_checks_every_directory_in_order() {
        let dir_without = tempfile::tempdir().expect("should create tempdir");
        let dir_with = tempfile::tempdir().expect("should create tempdir");
        let binary_path = dir_with.path().join(addon_binary_name("dbt-plan"));
        std::fs::write(&binary_path, "").expect("should write stub");

        let path_var =
            std::env::join_paths([dir_without.path(), dir_with.path()]).expect("should join paths");
        let found = find_in_path_var("dbt-plan", &path_var);
        assert_eq!(found.as_deref(), Some(binary_path.as_path()));
    }
}
