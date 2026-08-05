//! Command-line argument parsing for `zhao`.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// zhao: a change-review and CI engine for data transformation projects.
#[derive(Debug, Parser)]
#[command(name = "zhao", version)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// A `zhao` subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Runs the breaking-change gate: diffs the current project against a
    /// Baseline and exits non-zero if any Rule fires at `error` Severity.
    Check(CheckArgs),
    /// Runs the identical engine as `check` -- Baseline resolution, diff,
    /// Rule evaluation, report rendering -- but always exits zero,
    /// regardless of what Severity outcomes are present. For local
    /// inspection during development; use `check` for CI gating.
    Diff(CheckArgs),
    /// Answers "what's upstream/downstream of this model?" -- a
    /// structural query over the current project's compiled state, not a
    /// Baseline-vs-current diff. No `--state`, no git, no `dbt compile`.
    Lineage(LineageArgs),
}

/// Arguments for `zhao lineage`.
#[derive(Debug, clap::Args)]
pub struct LineageArgs {
    /// The lineage target, in dbt's own selector syntax: a bare model
    /// name (or `model.column` for column-level lineage) shows both
    /// upstream and downstream; a `+` prefix shows only upstream
    /// (ancestors); a `+` suffix shows only downstream (descendants).
    /// Required for text output; optional for `--html`, where omitting
    /// it embeds the whole project's lineage graph instead of scoping to
    /// one target.
    pub target: Option<String>,

    /// The dbt project directory to query. Its current compiled manifest
    /// is read from `<project-dir>/target/manifest.json`, as-is -- run
    /// `dbt compile` in the project before invoking `zhao lineage` (or
    /// pass `--compile`).
    #[arg(long, default_value = ".")]
    pub project_dir: PathBuf,

    /// Generates a self-contained, interactive HTML lineage graph at
    /// this path instead of printing text. A local development
    /// convenience -- not intended to run in CI.
    #[arg(long)]
    pub html: Option<PathBuf>,

    /// Runs `dbt compile` in `--project-dir` before generating the
    /// export, for a guaranteed-fresh view. Without it, the existing
    /// `target/manifest.json` is read as-is, same as the default for
    /// text output.
    #[arg(long)]
    pub compile: bool,
}

impl LineageArgs {
    /// Splits `target` into the bare model name, an optional column name
    /// (present for a `model.column` target), and the requested
    /// [`zhao_core::lineage::Direction`], per dbt's own `+`-prefix/suffix
    /// selector convention. `None` when no target was given at all
    /// (only valid alongside `--html`, for the whole-project graph).
    pub fn parse_target(&self) -> Option<(&str, Option<&str>, zhao_core::lineage::Direction)> {
        let target = self.target.as_deref()?;
        let has_prefix = target.starts_with('+');
        let after_prefix = target.strip_prefix('+').unwrap_or(target);
        let has_suffix = after_prefix.ends_with('+');
        let name = after_prefix.strip_suffix('+').unwrap_or(after_prefix);

        // `+target+` (both sides) means both directions in dbt's own
        // selector syntax too, same as a bare target -- handled
        // explicitly here rather than falling out of checking the prefix
        // alone and leaving a trailing `+` stuck to the name.
        let direction = match (has_prefix, has_suffix) {
            (true, true) | (false, false) => zhao_core::lineage::Direction::Both,
            (true, false) => zhao_core::lineage::Direction::Upstream,
            (false, true) => zhao_core::lineage::Direction::Downstream,
        };

        // dbt model names never contain `.`, so the first `.` (if any)
        // unambiguously separates the model from a column-level target.
        let (model, column) = match name.split_once('.') {
            Some((model, column)) => (model, Some(column)),
            None => (name, None),
        };
        Some((model, column, direction))
    }
}

/// Arguments shared by `zhao check` and `zhao diff` -- both run the
/// identical engine and accept identical inputs; they differ only in
/// what they do with the result (a gate's exit code vs. always zero).
#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Path to the Baseline's compiled dbt manifest (`manifest.json`). If
    /// omitted, zhao resolves its own Baseline: it finds the merge-base
    /// commit between `HEAD` and `--against`, checks it out into a
    /// temporary git worktree, compiles it with `dbt`, and uses that as
    /// the Baseline instead.
    #[arg(long)]
    pub state: Option<PathBuf>,

    /// The dbt project directory to check. Its current compiled manifest
    /// is read from `<project-dir>/target/manifest.json` -- run `dbt
    /// compile` in the project before invoking `zhao check`.
    #[arg(long, default_value = ".")]
    pub project_dir: PathBuf,

    /// The ref to resolve a git-native Baseline's merge-base against.
    /// Ignored when `--state` is given.
    #[arg(long, default_value = "master")]
    pub against: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Disables ANSI color codes in text output, regardless of what
    /// auto-detection would otherwise decide. Has no effect on `--format
    /// json`, which never contains color codes.
    #[arg(long)]
    pub no_color: bool,

    /// An extra argument to append to every `dbt deps`/`dbt compile`
    /// invocation zhao runs internally (git-native Baseline resolution
    /// only) -- repeat for multiple arguments, e.g. `--dbt-arg --target
    /// --dbt-arg ci`. Appended verbatim, in the order given; zhao never
    /// parses or validates these, dbt does. Mutually exclusive with
    /// `--dbt-args`.
    #[arg(
        long = "dbt-arg",
        conflicts_with = "dbt_args",
        allow_hyphen_values = true
    )]
    pub dbt_arg: Vec<String>,

    /// A single dbt-invocation-shaped string to split (shell-word-style)
    /// into individual arguments and append to every `dbt deps`/`dbt
    /// compile` invocation zhao runs internally -- a convenience
    /// alternative to repeating `--dbt-arg`, e.g. `--dbt-args "--target ci
    /// --vars '{\"key\": \"value\"}'"`. Mutually exclusive with
    /// `--dbt-arg`.
    #[arg(
        long = "dbt-args",
        conflicts_with = "dbt_arg",
        allow_hyphen_values = true
    )]
    pub dbt_args: Option<String>,

    /// Upgrades the conditional schema-evolution flag (see the "Schema
    /// evolution" report section) into a definitive one, or drops it
    /// entirely, by actually checking whether each flagged model exists
    /// in the configured target -- via the same connection `dbt run`
    /// already needs, never a connection zhao holds itself. Opt-in,
    /// since (unlike every other check zhao runs) this requires a real
    /// connection; without it, the flag stays conditionally worded.
    /// Silently unavailable (not an error) for any warehouse zhao
    /// doesn't yet support checking against.
    #[arg(long = "check-relations")]
    pub check_relations: bool,

    /// The name of the dbt target the `--defer` plan should defer to
    /// (e.g. `"prod"`) -- purely a human-readable label shown alongside
    /// the generated command, not passed to dbt as `--target`. Overrides
    /// `zhao.yml`'s `defer.target` when given. Has no effect unless a
    /// state path is also available (from `--defer-state` or
    /// `zhao.yml`'s `defer.state`).
    #[arg(long = "defer-target")]
    pub defer_target: Option<String>,

    /// The path to a compiled manifest to defer to -- when set (here or
    /// via `zhao.yml`'s `defer.state`), the report's `--defer` plan
    /// includes a ready-to-run `dbt ... --defer --state <path>` command
    /// alongside the plan's build/defer Node lists, not just the lists
    /// themselves. Overrides `zhao.yml`'s `defer.state` when given.
    #[arg(long = "defer-state")]
    pub defer_state: Option<PathBuf>,
}

impl CheckArgs {
    /// Resolves the final, ordered list of extra arguments to append to
    /// every `dbt deps`/`dbt compile` invocation, from whichever of
    /// `--dbt-arg`/`--dbt-args` was given (clap's `conflicts_with` on both
    /// fields already guarantees at most one was) -- empty if neither was.
    pub fn dbt_passthrough_args(&self) -> Result<Vec<String>, String> {
        if !self.dbt_arg.is_empty() {
            return Ok(self.dbt_arg.clone());
        }
        if let Some(raw) = &self.dbt_args {
            return shell_words::split(raw)
                .map_err(|err| format!("could not parse --dbt-args {raw:?}: {err}"));
        }
        Ok(Vec::new())
    }
}

/// `zhao check`'s output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// A brief human-readable summary.
    Text,
    /// Machine-readable JSON.
    Json,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::lineage::Direction;

    fn args(target: &str) -> LineageArgs {
        LineageArgs {
            target: Some(target.to_string()),
            project_dir: PathBuf::from("."),
            html: None,
            compile: false,
        }
    }

    #[test]
    fn bare_target_parses_as_both_directions() {
        let a = args("dim_customers");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, None);
        assert_eq!(direction, Direction::Both);
    }

    #[test]
    fn a_plus_prefix_parses_as_upstream_only() {
        let a = args("+dim_customers");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, None);
        assert_eq!(direction, Direction::Upstream);
    }

    #[test]
    fn a_plus_suffix_parses_as_downstream_only() {
        let a = args("dim_customers+");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, None);
        assert_eq!(direction, Direction::Downstream);
    }

    /// `+target+` (both sides), same as dbt's own selector syntax, means
    /// both directions -- same as a bare target, and critically not a
    /// garbled name with a stray trailing `+` still attached.
    #[test]
    fn a_plus_prefix_and_suffix_together_parses_as_both_directions() {
        let a = args("+dim_customers+");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, None);
        assert_eq!(direction, Direction::Both);
    }

    #[test]
    fn a_dotted_target_splits_into_model_and_column() {
        let a = args("dim_customers.customer_id");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, Some("customer_id"));
        assert_eq!(direction, Direction::Both);
    }

    #[test]
    fn a_dotted_target_still_honors_plus_prefix_and_suffix() {
        let a = args("+dim_customers.customer_id+");
        let (name, column, direction) = a.parse_target().expect("target should be present");
        assert_eq!(name, "dim_customers");
        assert_eq!(column, Some("customer_id"));
        assert_eq!(direction, Direction::Both);
    }
}
