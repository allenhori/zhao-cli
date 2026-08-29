//! Resolves the directory dbt writes its compiled artifacts
//! (`manifest.json`, `catalog.json`) into, honoring a `--target-path`
//! override present in a `--dbt-arg`/`--dbt-args` passthrough list --
//! the same list zhao forwards verbatim to the `dbt compile` subprocess
//! it runs internally (see `LineageArgs::dbt_passthrough_args` /
//! `CheckArgs::dbt_passthrough_args`).
//!
//! Without this, a compile isolated via `--dbt-args "--target-path
//! <dir>"` would still have its resulting `manifest.json` read back
//! from the project's real `target/` -- the literal, hardcoded path
//! every other part of zhao assumed until now -- silently ignoring the
//! isolation the caller asked dbt for. `resolve_target_dir` is the one
//! place that hardcoded assumption gets replaced, so `zhao lineage`
//! (and, via `zhao check`/`zhao diff`'s own current-manifest read, a
//! subsequent `zhao diff` pointed at the same `--target-path`) can
//! agree on where the compile actually landed.

use std::path::{Path, PathBuf};

/// Returns `<project_dir>/<target-path>` when `dbt_passthrough_args`
/// contains a `--target-path <dir>` or `--target-path=<dir>` override,
/// else `<project_dir>/target`, matching dbt's own default location.
///
/// Only the *last* occurrence wins if `--target-path` appears more than
/// once, matching how dbt itself treats a repeated CLI flag on one
/// invocation. A trailing `--target-path` with no following value is
/// ignored (falls back to the default) rather than treated as a real
/// override, since dbt itself would reject that invocation before ever
/// running -- there's no override to honor.
pub(crate) fn resolve_target_dir(project_dir: &Path, dbt_passthrough_args: &[String]) -> PathBuf {
    let mut target_path: Option<&str> = None;
    let mut iter = dbt_passthrough_args.iter().peekable();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--target-path=") {
            target_path = Some(value);
        } else if arg == "--target-path" {
            if let Some(next) = iter.peek() {
                target_path = Some(next.as_str());
                iter.next();
            }
        }
    }

    match target_path {
        Some(path) => project_dir.join(path),
        None => project_dir.join("target"),
    }
}

/// Resolves the final `(dbt_command, dbt_passthrough_args)` pair to use
/// for a `dbt` subprocess invocation, from a CLI-level `--dbt-command`/
/// already-split passthrough-args pair plus `zhao.yml`'s own
/// `dbt-command`/`dbt-args` -- shared by `zhao check`/`zhao diff` (via
/// `crate::engine::build_report`) and `zhao lineage --compile`, which
/// both apply the identical precedence: the CLI form wins outright when
/// given; otherwise `zhao.yml`'s value; otherwise `"dbt"`/no extra args.
pub(crate) fn resolve_dbt_invocation(
    cli_dbt_command: Option<&str>,
    cli_dbt_passthrough_args: Vec<String>,
    config: &zhao_core::config::Config,
) -> Result<(String, Vec<String>), String> {
    let dbt_command = cli_dbt_command
        .map(str::to_string)
        .or_else(|| config.dbt_command().map(str::to_string))
        .unwrap_or_else(|| "dbt".to_string());

    let dbt_passthrough_args = if cli_dbt_passthrough_args.is_empty() {
        match config.dbt_args() {
            Some(raw) => shell_words::split(raw)
                .map_err(|err| format!("zhao.yml dbt-args {raw:?}: {err}"))?,
            None => Vec::new(),
        }
    } else {
        cli_dbt_passthrough_args
    };

    Ok((dbt_command, dbt_passthrough_args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::config::Config;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// An empty config -- `Config::load` on a nonexistent path is the
    /// same pattern `zhao-core`'s own config tests use for "no
    /// `zhao.yml` at all".
    fn empty_config() -> Config {
        Config::load(Path::new("/nonexistent/zhao.yml")).expect("should be ok")
    }

    #[test]
    fn cli_dbt_command_and_args_win_over_everything() {
        let (command, passthrough) = resolve_dbt_invocation(
            Some("uv run dbt"),
            args(&["--target-path", "custom"]),
            &empty_config(),
        )
        .expect("should resolve");
        assert_eq!(command, "uv run dbt");
        assert_eq!(passthrough, args(&["--target-path", "custom"]));
    }

    #[test]
    fn with_nothing_set_anywhere_the_default_dbt_command_is_used_and_args_are_empty() {
        let (command, passthrough) =
            resolve_dbt_invocation(None, Vec::new(), &empty_config()).expect("should resolve");
        assert_eq!(command, "dbt");
        assert!(passthrough.is_empty());
    }

    #[test]
    fn zhao_yml_is_used_when_no_cli_form_is_given() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        std::fs::write(
            dir.path().join("zhao.yml"),
            "dbt-command: \"uv run dbt\"\ndbt-args: \"--target ci\"\n",
        )
        .expect("should write zhao.yml");
        let config = Config::load(&dir.path().join("zhao.yml")).expect("should parse");

        let (command, passthrough) =
            resolve_dbt_invocation(None, Vec::new(), &config).expect("should resolve");
        assert_eq!(command, "uv run dbt");
        assert_eq!(passthrough, args(&["--target", "ci"]));
    }

    #[test]
    fn no_passthrough_args_defaults_to_target() {
        let dir = resolve_target_dir(Path::new("/proj"), &[]);
        assert_eq!(dir, PathBuf::from("/proj/target"));
    }

    #[test]
    fn unrelated_passthrough_args_still_default_to_target() {
        let dir = resolve_target_dir(
            Path::new("/proj"),
            &args(&["--target", "ci", "--full-refresh"]),
        );
        assert_eq!(dir, PathBuf::from("/proj/target"));
    }

    /// An absolute `--target-path` value joins onto `project_dir` via
    /// `Path::join`, whose own documented semantics (an absolute `path`
    /// replaces `project_dir` entirely) are exactly what's wanted here.
    #[test]
    fn space_separated_absolute_target_path_is_honored() {
        let dir = resolve_target_dir(
            Path::new("/proj"),
            &args(&["--target-path", "/tmp/zhao-abc"]),
        );
        assert_eq!(dir, PathBuf::from("/tmp/zhao-abc"));
    }

    #[test]
    fn relative_target_path_joins_onto_project_dir() {
        let dir = resolve_target_dir(
            Path::new("/proj"),
            &args(&["--target-path", "custom_target"]),
        );
        assert_eq!(dir, PathBuf::from("/proj/custom_target"));
    }

    #[test]
    fn equals_form_target_path_is_honored() {
        let dir = resolve_target_dir(Path::new("/proj"), &args(&["--target-path=custom_target"]));
        assert_eq!(dir, PathBuf::from("/proj/custom_target"));
    }

    #[test]
    fn last_of_multiple_target_path_overrides_wins() {
        let dir = resolve_target_dir(
            Path::new("/proj"),
            &args(&["--target-path", "first", "--target-path", "second"]),
        );
        assert_eq!(dir, PathBuf::from("/proj/second"));
    }

    #[test]
    fn target_path_mixed_with_other_flags_is_still_found() {
        let dir = resolve_target_dir(
            Path::new("/proj"),
            &args(&[
                "--target",
                "ci",
                "--target-path",
                "custom_target",
                "--full-refresh",
            ]),
        );
        assert_eq!(dir, PathBuf::from("/proj/custom_target"));
    }

    #[test]
    fn a_trailing_target_path_with_no_value_is_ignored() {
        let dir = resolve_target_dir(Path::new("/proj"), &args(&["--target-path"]));
        assert_eq!(dir, PathBuf::from("/proj/target"));
    }
}
