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
    /// Path to the Baseline's compiled dbt manifest (`manifest.json`).
    #[arg(long)]
    pub state: PathBuf,

    /// The dbt project directory to check. Its current compiled manifest
    /// is read from `<project-dir>/target/manifest.json` -- run `dbt
    /// compile` in the project before invoking `zhao check`.
    #[arg(long, default_value = ".")]
    pub project_dir: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// `zhao check`'s output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// A brief human-readable summary.
    Text,
    /// Machine-readable JSON.
    Json,
}
