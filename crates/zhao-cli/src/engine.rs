//! Shared engine logic behind both `zhao check` (a gate, exit-code aware)
//! and `zhao diff` (always exits zero, for local inspection): both run the
//! identical Baseline resolution -> diff -> Rule evaluation -> report
//! pipeline. Only what each command does with the resulting [`Report`] --
//! and which exit code it maps outcomes to -- differs.

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

/// The full result of running the engine pipeline: the [`Report`] itself,
/// plus the current project state it was built from -- needed separately
/// since [`crate::metadata::RunMetadata`] includes the current state's
/// full Lineage Edge breakdown, which the `Report` alone doesn't carry.
pub(crate) struct EngineOutput {
    /// The completed report -- what a `--format json`/text run prints.
    pub report: Report,
    /// The current project state the report was built from.
    pub current: zhao_core::model::ParsedProject,
}

/// Runs the full engine pipeline -- Baseline resolution, diff, Rule
/// evaluation -- and builds the resulting [`Report`], including its
/// staleness warning and recommended command.
pub(crate) fn build_report(args: &CheckArgs) -> Result<EngineOutput, String> {
    let current_manifest = args.project_dir.join("target").join("manifest.json");

    let baseline =
        crate::baseline::resolve(args.state.as_deref(), &args.project_dir, &args.against)
            .map_err(|err| err.to_string())?;
    let current = load_manifest(&current_manifest)?;
    let config = Config::load_for_project(&args.project_dir).map_err(|err| err.to_string())?;

    let changes = diff(&baseline, &current);
    let findings = evaluate(&baseline, &changes, &config);
    let report = Report::new(&changes, &findings)
        .with_staleness_warning(is_stale(&args.project_dir, &args.against))
        .with_recommended_command(DbtAdapter.vocabulary());

    Ok(EngineOutput { report, current })
}

/// Writes `target/zhao/run-metadata.json` for this run -- see
/// [`crate::metadata`]. Called by both `zhao check` and `zhao diff`, since
/// every run writes it, not just gated ones.
pub(crate) fn write_run_metadata(output: &EngineOutput, args: &CheckArgs) -> Result<(), String> {
    let metadata = crate::metadata::RunMetadata::new(&output.report, &output.current);
    crate::metadata::write(&metadata, &args.project_dir)
}

/// Prints `report` in `args.format` -- JSON or the color-aware text
/// report, per `args.no_color` and environment auto-detection.
pub(crate) fn print_report(report: &Report, args: &CheckArgs) -> Result<(), String> {
    match args.format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(report)
                .map_err(|err| format!("could not serialize report as JSON: {err}"))?;
            println!("{json}");
        }
        OutputFormat::Text => {
            print!(
                "{}",
                render_text(report, DbtAdapter.vocabulary(), use_color(args.no_color))
            );
        }
    }
    Ok(())
}

/// Exit code for "couldn't even run" (bad paths, unparsable manifests,
/// invalid `zhao.yml`, ...) -- shared by `zhao check` and `zhao diff`,
/// distinct from either command's own "ran successfully" exit codes so a
/// caller can always tell "zhao itself failed" apart from any outcome the
/// engine actually produced.
const EXIT_ERROR: u8 = 2;

/// Prints `message` to stderr as `error: {message}` and returns
/// [`EXIT_ERROR`] -- the shared shape both `zhao check` and `zhao diff`
/// use for "couldn't even run" failures.
pub(crate) fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_ERROR)
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
