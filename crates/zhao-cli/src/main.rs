//! Entry point for the `zhao` command-line tool.
//!
//! This crate is intentionally a thin shell: argument parsing, output
//! formatting, and process exit codes live here, while all actual analysis
//! is delegated to `zhao-core`. See `ARCHITECTURE.md` at the repository
//! root for the intended command surface as it's implemented.

mod baseline;
mod check;
mod cli;
mod diff;
mod engine;
mod lineage;
mod lineage_html;
mod log;
mod metadata;
mod report;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();

    match &cli.command {
        Command::Check(args) => check::run(args),
        Command::Diff(args) => diff::run(args),
        Command::Lineage(args) => lineage::run(args),
    }
}
