//! The `zhao check` command: diffs the current project against a
//! Baseline, evaluates zhao's Rules, and reports the result.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
use zhao_core::config::Config;
use zhao_core::diff::diff;
use zhao_core::rules::evaluate;

use crate::cli::{CheckArgs, OutputFormat};
use crate::report::{Report, render_text};

/// Exit code for "no breaking Change found."
const EXIT_PASS: u8 = 0;
/// Exit code for "at least one Rule fired at `error` Severity."
const EXIT_BREAKING: u8 = 1;
/// Exit code for "couldn't even run the check" (bad paths, unparsable
/// manifests, ...) -- distinct from a breaking-change result so a caller
/// can tell "your change broke something" apart from "zhao itself failed."
const EXIT_ERROR: u8 = 2;

/// Runs `zhao check` and returns the process exit code.
pub fn run(args: &CheckArgs) -> ExitCode {
    let current_manifest = args.project_dir.join("target").join("manifest.json");

    let baseline =
        match crate::baseline::resolve(args.state.as_deref(), &args.project_dir, &args.against) {
            Ok(project) => project,
            Err(err) => return fail(&err.to_string()),
        };
    let current = match load_manifest(&current_manifest) {
        Ok(project) => project,
        Err(message) => return fail(&message),
    };
    let config = match Config::load_for_project(&args.project_dir) {
        Ok(config) => config,
        Err(err) => return fail(&err.to_string()),
    };

    let changes = diff(&baseline, &current);
    let findings = evaluate(&baseline, &changes, &config);
    let report = Report::new(&changes, &findings)
        .with_staleness_warning(is_stale(&args.project_dir, &args.against))
        .with_recommended_command(&current, DbtAdapter.vocabulary());

    match args.format {
        OutputFormat::Json => match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(err) => return fail(&format!("could not serialize report as JSON: {err}")),
        },
        OutputFormat::Text => print!(
            "{}",
            render_text(&report, DbtAdapter.vocabulary(), use_color(args.no_color))
        ),
    }

    ExitCode::from(if report.is_breaking() {
        EXIT_BREAKING
    } else {
        EXIT_PASS
    })
}

/// Whether the Baseline's merge-base against `against` has fallen behind
/// `against`'s current tip. Best-effort and purely informational: any
/// failure to determine this (e.g. `project_dir` isn't inside a git
/// repository at all) is treated as "not stale" rather than failing the
/// whole command -- staleness is a courtesy warning, not a requirement.
fn is_stale(project_dir: &Path, against: &str) -> bool {
    zhao_core::git::repo_root(project_dir)
        .and_then(|repo_root| zhao_core::git::merge_base_is_stale(&repo_root, against))
        .unwrap_or(false)
}

/// Whether text output should include ANSI color codes.
///
/// `no_color_flag` (`--no-color`) always wins outright. Otherwise this
/// defers to [`use_color_decision`], the pure/testable core, fed with the
/// real environment's actual state.
fn use_color(no_color_flag: bool) -> bool {
    use_color_decision(
        no_color_flag,
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("GITHUB_ACTIONS").is_some(),
        std::io::stdout().is_terminal(),
    )
}

/// The actual color decision, as a pure function of already-read inputs --
/// kept separate from [`use_color`] so it's testable without needing to
/// mutate real process env vars (racy under parallel tests) or fake a real
/// TTY.
///
/// `--no-color` and `NO_COLOR` (<https://no-color.org>) both unconditionally
/// disable color. Otherwise, color is enabled either when stdout is a real
/// terminal, or when running inside GitHub Actions specifically: its log
/// renderer supports ANSI color even though the runner's stdout isn't a
/// real TTY, so a plain `is_terminal()` check alone would wrongly suppress
/// color in exactly the CI environment this report is most useful in.
fn use_color_decision(
    no_color_flag: bool,
    no_color_env_set: bool,
    github_actions_env_set: bool,
    stdout_is_tty: bool,
) -> bool {
    if no_color_flag || no_color_env_set {
        return false;
    }
    github_actions_env_set || stdout_is_tty
}

fn load_manifest(path: &Path) -> Result<zhao_core::model::ParsedProject, String> {
    DbtAdapter
        .parse(path)
        .map_err(|err| format!("{path}: {err}", path = path.display()))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_flag_wins_over_everything_else() {
        assert!(!use_color_decision(true, false, true, true));
    }

    #[test]
    fn no_color_env_var_wins_over_a_real_tty_and_github_actions() {
        assert!(!use_color_decision(false, true, true, true));
    }

    #[test]
    fn a_real_tty_enables_color_by_default() {
        assert!(use_color_decision(false, false, false, true));
    }

    #[test]
    fn github_actions_enables_color_even_without_a_real_tty() {
        assert!(use_color_decision(false, false, true, false));
    }

    #[test]
    fn a_plain_non_tty_non_ci_environment_suppresses_color() {
        assert!(!use_color_decision(false, false, false, false));
    }
}
