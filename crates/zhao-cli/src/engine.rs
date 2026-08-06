//! Shared engine logic behind both `zhao check` (a gate, exit-code aware)
//! and `zhao diff` (always exits zero, for local inspection): both run the
//! identical Baseline resolution -> diff -> Rule evaluation -> report
//! pipeline. Only what each command does with the resulting [`Report`] --
//! and which exit code it maps outcomes to -- differs.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::{DbtAdapter, DbtQueryExecutor};
use zhao_core::adapters::warehouse;
use zhao_core::config::Config;
use zhao_core::diff::diff;
use zhao_core::rules::evaluate;

use crate::cli::{CheckArgs, OutputFormat};
use crate::report::{DeferSettings, Report, render_text};

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
/// staleness warning, recommended command, and `--defer` plan.
pub(crate) fn build_report(args: &CheckArgs) -> Result<EngineOutput, String> {
    let current_manifest = args.project_dir.join("target").join("manifest.json");

    let dbt_passthrough_args = args.dbt_passthrough_args()?;
    let config = Config::load_for_project(&args.project_dir).map_err(|err| err.to_string())?;
    // `--against` wins when explicitly passed; otherwise `zhao.yml`'s
    // `against`; otherwise zhao's own default. Resolved once, up front,
    // so both Baseline resolution and the staleness check (which must
    // agree on what "the target branch" is) use the exact same value.
    let against = args
        .against
        .clone()
        .or_else(|| config.against().map(str::to_string))
        .unwrap_or_else(|| "master".to_string());
    let baseline = crate::baseline::resolve(
        args.state.as_deref(),
        &args.project_dir,
        &against,
        &dbt_passthrough_args,
    )
    .map_err(|err| err.to_string())?;
    let current = load_manifest(&current_manifest)?;

    let changes = diff(&baseline, &current);
    let findings = evaluate(&baseline, &changes, &config);
    let defer_settings = DeferSettings {
        target: args
            .defer_target
            .clone()
            .or_else(|| config.defer_target().map(str::to_string)),
        state: args
            .defer_state
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| config.defer_state().map(str::to_string)),
    };
    let mut report = Report::new(&changes, &findings)
        .with_staleness_warning(is_stale(&args.project_dir, &against))
        .with_recommended_command(DbtAdapter.vocabulary())
        .with_defer_plan(&current, DbtAdapter.vocabulary(), &defer_settings)
        .with_schema_evolution_warnings(&current);

    if args.check_relations {
        report = apply_check_relations(report, args, &current_manifest, &dbt_passthrough_args);
    }

    Ok(EngineOutput { report, current })
}

/// `--check-relations`: upgrades or drops each conditional schema-
/// evolution warning by actually checking whether its Node exists in
/// the configured target. Best-effort: any warehouse zhao doesn't
/// support, or any failure of the check itself, is reported to stderr
/// as a warning and leaves that warning's conditional wording
/// untouched -- never turns an otherwise-successful run into "zhao
/// itself failed" over a live-check problem.
fn apply_check_relations(
    report: Report,
    args: &CheckArgs,
    current_manifest: &Path,
    dbt_passthrough_args: &[String],
) -> Report {
    let adapter_type = match DbtAdapter.adapter_type(current_manifest) {
        Ok(Some(adapter_type)) => adapter_type,
        Ok(None) => {
            eprintln!(
                "warning: --check-relations: the compiled manifest doesn't record which \
                 warehouse it targets, so live existence checks aren't available"
            );
            return report;
        }
        Err(err) => {
            eprintln!("warning: --check-relations: could not read the compiled manifest: {err}");
            return report;
        }
    };
    let Some(warehouse_adapter) = warehouse::resolve(&adapter_type) else {
        eprintln!(
            "warning: --check-relations: {adapter_type:?} isn't a supported warehouse yet, \
             so live existence checks aren't available"
        );
        return report;
    };
    let relation_identities = match DbtAdapter.relation_identities(current_manifest) {
        Ok(identities) => identities,
        Err(err) => {
            eprintln!(
                "warning: --check-relations: could not read relation identities from the \
                 compiled manifest: {err}"
            );
            return report;
        }
    };

    let executor = DbtQueryExecutor {
        project_dir: &args.project_dir,
        dbt_command: "dbt",
        extra_args: dbt_passthrough_args,
    };

    report.with_live_relation_checks(|node_id| {
        let relation = relation_identities.get(node_id)?;
        match warehouse_adapter.relation_exists(relation, &executor) {
            Ok(exists) => Some(exists),
            Err(err) => {
                eprintln!("warning: --check-relations: could not check {node_id}: {err}");
                None
            }
        }
    })
}

/// Writes `target/zhao/run-metadata.json` for this run -- see
/// [`crate::metadata`]. Called by both `zhao check` and `zhao diff`, since
/// every run writes it, not just gated ones.
///
/// A failure here (permission denied, disk full, a read-only `target/` in
/// some sandboxed runner, ...) is reported to stderr as a warning, but
/// deliberately doesn't change the process exit code: by the time this
/// runs, the report has already been printed and the real gate result
/// already computed, so a sidecar file failing to write shouldn't turn an
/// otherwise-successful (or otherwise-correctly-failing) run into "zhao
/// itself failed" -- that would let something orthogonal to the actual
/// Change/Finding analysis flip a CI job's outcome.
pub(crate) fn write_run_metadata(output: &EngineOutput, args: &CheckArgs) {
    let metadata = crate::metadata::RunMetadata::new(&output.report, &output.current);
    if let Err(message) = crate::metadata::write(&metadata, &args.project_dir) {
        eprintln!("warning: could not write run metadata: {message}");
    }
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
