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
}

/// Arguments to `zhao check`.
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
}

/// `zhao check`'s output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// A brief human-readable summary.
    Text,
    /// Machine-readable JSON.
    Json,
}
