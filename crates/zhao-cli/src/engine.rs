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
    /// The resolved run-log retention window (`--purge-logs` if given,
    /// else `zhao.yml`'s `log.retention_days`, else `None`) -- see
    /// [`purge_run_logs`] and issue #37.
    pub log_retention_days: Option<u32>,
}

/// Runs the full engine pipeline -- Baseline resolution, diff, Rule
/// evaluation -- and builds the resulting [`Report`], including its
/// staleness warning, recommended command, and `--defer` plan.
pub(crate) fn build_report(args: &CheckArgs) -> Result<EngineOutput, String> {
    let current_manifest = args.project_dir.join("target").join("manifest.json");

    // Checked before Baseline resolution (which compiles a whole
    // temporary git worktree -- not cheap) so a stale current manifest
    // fails fast rather than after that work is already done.
    if !args.allow_stale_manifest {
        check_current_manifest_freshness(&args.project_dir, &current_manifest)?;
    }

    let config = Config::load_for_project(&args.project_dir).map_err(|err| err.to_string())?;
    // CLI `--dbt-arg`/`--dbt-args` wins outright when either is given
    // (clap's `conflicts_with` already guarantees at most one CLI form);
    // otherwise falls back to `zhao.yml`'s `dbt-args`, shell-word-split
    // the same way the CLI's own `--dbt-args` string form is.
    let dbt_passthrough_args = args.dbt_passthrough_args()?;
    let dbt_passthrough_args = if dbt_passthrough_args.is_empty() {
        match config.dbt_args() {
            Some(raw) => shell_words::split(raw)
                .map_err(|err| format!("zhao.yml dbt-args {raw:?}: {err}"))?,
            None => Vec::new(),
        }
    } else {
        dbt_passthrough_args
    };
    // `--against` wins when explicitly passed; otherwise `zhao.yml`'s
    // `against`; otherwise zhao's own default. Resolved once, up front,
    // so both Baseline resolution and the staleness check (which must
    // agree on what "the target branch" is) use the exact same value.
    let against = args
        .against
        .clone()
        .or_else(|| config.against().map(str::to_string))
        .unwrap_or_else(|| "master".to_string());
    // `--dbt-command` wins when explicitly passed; otherwise `zhao.yml`'s
    // `dbt-command`; otherwise `"dbt"`, resolved via `PATH` -- the same
    // precedence `against` already uses just above.
    let dbt_command = args
        .dbt_command
        .clone()
        .or_else(|| config.dbt_command().map(str::to_string))
        .unwrap_or_else(|| "dbt".to_string());
    let baseline = crate::baseline::resolve(
        args.state.as_deref(),
        &args.project_dir,
        &against,
        &dbt_command,
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
        report = apply_check_relations(
            report,
            args,
            &current_manifest,
            &dbt_command,
            &dbt_passthrough_args,
        );
    }

    // `--purge-logs` wins when explicitly passed; otherwise `zhao.yml`'s
    // `log.retention_days`; otherwise `None` (no purging). See issue #37.
    let log_retention_days = args.purge_logs.or_else(|| config.log_retention_days());

    Ok(EngineOutput {
        report,
        current,
        log_retention_days,
    })
}

/// Purges old `target/zhao/logs/` entries per `log_retention_days` (see
/// [`EngineOutput::log_retention_days`]) -- a thin wrapper so `zhao
/// check`/`zhao diff` don't need to reach into `crate::log` directly.
/// A no-op when `log_retention_days` is `None`. See issue #37.
pub(crate) fn purge_run_logs(args: &CheckArgs, log_retention_days: Option<u32>) {
    crate::log::purge(&args.project_dir, log_retention_days);
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
    dbt_command: &str,
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
        dbt_command,
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
    // Built up first, then both printed and mirrored to the daily run
    // log (issue #35) -- a literal mirror of what actually went to
    // stdout, so it matches `println!`'s trailing newline in the JSON
    // case too, not just the text case's own already-newline-terminated
    // render.
    let printed = match args.format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(report)
                .map_err(|err| format!("could not serialize report as JSON: {err}"))?;
            println!("{json}");
            format!("{json}\n")
        }
        OutputFormat::Text => {
            let text = render_text(report, DbtAdapter.vocabulary(), use_color(args.no_color));
            print!("{text}");
            text
        }
    };
    crate::log::mirror(&args.project_dir, &printed);
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

/// Root-level files that feed `dbt compile`/`dbt parse` directly, checked
/// for staleness alongside [`DBT_SOURCE_DIRS`].
const DBT_SOURCE_ROOT_FILES: &[&str] = &["dbt_project.yml", "packages.yml", "dependencies.yml"];

/// Conventional dbt source directories -- everything under these feeds
/// compilation (model/seed/snapshot/macro/test SQL, schema YAML, seed
/// CSVs). Deliberately not "every file under `project_dir`": that would
/// also flag unrelated edits (README, `zhao.yml`, CI config) as requiring
/// a recompile, which they don't.
const DBT_SOURCE_DIRS: &[&str] = &[
    "models",
    "macros",
    "seeds",
    "snapshots",
    "analyses",
    "tests",
];

/// Refuses to proceed if `manifest_path` predates any of `project_dir`'s
/// own dbt source files -- the exact bug pattern of checking out a
/// different branch (or pulling new commits) without rerunning `dbt
/// compile`, which otherwise leaves `zhao check`/`zhao diff` silently
/// diffing against a manifest compiled from a different project state
/// entirely. See issue tracker for the report this was written from.
///
/// A no-op (never flags staleness) when `project_dir` has no dbt source
/// files to compare against at all -- e.g. a manifest-only test fixture
/// with no real dbt project checked out alongside it -- since there's
/// nothing to compare. Also a no-op if either mtime can't be read (best
/// effort, same precedent as [`is_stale`]'s git-based staleness check);
/// a genuinely missing/unreadable manifest still surfaces its own clear
/// error from [`load_manifest`] right after this check runs.
fn check_current_manifest_freshness(
    project_dir: &Path,
    manifest_path: &Path,
) -> Result<(), String> {
    let Some(newest_source) = newest_dbt_source_mtime(project_dir) else {
        return Ok(());
    };
    let Ok(manifest_mtime) = std::fs::metadata(manifest_path).and_then(|m| m.modified()) else {
        return Ok(());
    };
    if newest_source > manifest_mtime {
        return Err(format!(
            "{manifest} looks stale: a dbt source file under {project_dir} was modified more \
             recently than the compiled manifest. This usually means the manifest was compiled \
             from a different branch or an older commit -- run `dbt compile` in {project_dir} \
             and try again. Pass --allow-stale-manifest to skip this check (not recommended).",
            manifest = manifest_path.display(),
            project_dir = project_dir.display(),
        ));
    }
    Ok(())
}

/// The newest modification time among `project_dir`'s dbt source-of-truth
/// input files ([`DBT_SOURCE_ROOT_FILES`] plus everything under
/// [`DBT_SOURCE_DIRS`]). `None` if none of those exist at all.
fn newest_dbt_source_mtime(project_dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut consider = |path: &Path| {
        if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
            if newest.is_none_or(|current| modified > current) {
                newest = Some(modified);
            }
        }
    };

    for file_name in DBT_SOURCE_ROOT_FILES {
        consider(&project_dir.join(file_name));
    }
    for dir_name in DBT_SOURCE_DIRS {
        walk_mtimes(&project_dir.join(dir_name), &mut consider);
    }

    newest
}

/// Recursively visits every regular file under `dir`, calling `consider`
/// on each. Silently does nothing if `dir` doesn't exist or can't be
/// read (a project simply not using that conventional dbt subdirectory
/// is the common case, not an error).
fn walk_mtimes(dir: &Path, consider: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_mtimes(&path, consider);
        } else {
            consider(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_mtime(path: &Path, seconds_since_epoch: u64) {
        let time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds_since_epoch);
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("file should be openable for writing")
            .set_modified(time)
            .expect("mtime should be settable");
    }

    #[test]
    fn no_dbt_project_present_is_never_flagged_stale() {
        // Matches the shape of existing test fixtures: a manifest-only
        // directory with no real dbt project (models/, dbt_project.yml,
        // ...) checked out alongside it -- there's nothing to compare
        // staleness against, so this must never fail.
        let dir = tempfile::tempdir().expect("tempdir should be creatable");
        std::fs::create_dir_all(dir.path().join("target")).expect("target dir should be creatable");
        let manifest = dir.path().join("target").join("manifest.json");
        std::fs::write(&manifest, "{}").expect("manifest should be writable");

        assert!(check_current_manifest_freshness(dir.path(), &manifest).is_ok());
    }

    #[test]
    fn a_model_file_newer_than_the_manifest_is_flagged_stale() {
        let dir = tempfile::tempdir().expect("tempdir should be creatable");
        std::fs::create_dir_all(dir.path().join("target")).expect("target dir should be creatable");
        let manifest = dir.path().join("target").join("manifest.json");
        std::fs::write(&manifest, "{}").expect("manifest should be writable");
        set_mtime(&manifest, 1_000);

        std::fs::create_dir_all(dir.path().join("models")).expect("models dir should be creatable");
        let model = dir.path().join("models").join("foo.sql");
        std::fs::write(&model, "select 1").expect("model should be writable");
        set_mtime(&model, 2_000);

        let err = check_current_manifest_freshness(dir.path(), &manifest)
            .expect_err("a newer model file should be flagged stale");
        assert!(err.contains("looks stale"), "{err}");
        assert!(err.contains("--allow-stale-manifest"), "{err}");
    }

    #[test]
    fn a_changed_dbt_project_yml_alone_is_flagged_stale() {
        let dir = tempfile::tempdir().expect("tempdir should be creatable");
        std::fs::create_dir_all(dir.path().join("target")).expect("target dir should be creatable");
        let manifest = dir.path().join("target").join("manifest.json");
        std::fs::write(&manifest, "{}").expect("manifest should be writable");
        set_mtime(&manifest, 1_000);

        let dbt_project = dir.path().join("dbt_project.yml");
        std::fs::write(&dbt_project, "name: fixture").expect("dbt_project.yml should be writable");
        set_mtime(&dbt_project, 2_000);

        assert!(check_current_manifest_freshness(dir.path(), &manifest).is_err());
    }

    #[test]
    fn a_manifest_newer_than_every_source_file_is_not_flagged_stale() {
        let dir = tempfile::tempdir().expect("tempdir should be creatable");
        std::fs::create_dir_all(dir.path().join("models")).expect("models dir should be creatable");
        let model = dir.path().join("models").join("foo.sql");
        std::fs::write(&model, "select 1").expect("model should be writable");
        set_mtime(&model, 1_000);

        std::fs::create_dir_all(dir.path().join("target")).expect("target dir should be creatable");
        let manifest = dir.path().join("target").join("manifest.json");
        std::fs::write(&manifest, "{}").expect("manifest should be writable");
        set_mtime(&manifest, 2_000);

        assert!(check_current_manifest_freshness(dir.path(), &manifest).is_ok());
    }

    #[test]
    fn an_unrelated_file_outside_dbt_source_conventions_is_ignored() {
        // Editing zhao.yml, README, or CI config shouldn't demand a
        // recompile -- only actual dbt compile inputs should. A real dbt
        // source tree is present here too (older than the manifest), so
        // this specifically proves zhao.yml is excluded, not just that
        // there was nothing to compare against.
        let dir = tempfile::tempdir().expect("tempdir should be creatable");
        std::fs::create_dir_all(dir.path().join("models")).expect("models dir should be creatable");
        let model = dir.path().join("models").join("foo.sql");
        std::fs::write(&model, "select 1").expect("model should be writable");
        set_mtime(&model, 500);

        std::fs::create_dir_all(dir.path().join("target")).expect("target dir should be creatable");
        let manifest = dir.path().join("target").join("manifest.json");
        std::fs::write(&manifest, "{}").expect("manifest should be writable");
        set_mtime(&manifest, 1_000);

        let unrelated = dir.path().join("zhao.yml");
        std::fs::write(&unrelated, "preset: strict").expect("zhao.yml should be writable");
        set_mtime(&unrelated, 2_000);

        assert!(check_current_manifest_freshness(dir.path(), &manifest).is_ok());
    }

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
