//! Entry point for the `zhao` command-line tool.
//!
//! This crate is intentionally a thin shell: argument parsing, output
//! formatting, and process exit codes live here, while all actual analysis
//! is delegated to `zhao-core`. See `ARCHITECTURE.md` at the repository
//! root for the intended command surface as it's implemented.

mod addon;
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
mod update;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    // Addon dispatch (see addon.rs) happens before clap ever parses
    // argv: clap only knows zhao's own built-in subcommands, so a
    // subcommand name it doesn't recognize would otherwise become a
    // hard parse error before there's any chance to check whether it's
    // actually an installed Addon (e.g. `zhao dbt-plan` ->
    // `zhao-dbt-plan` on PATH). argv[1] is the subcommand position;
    // anything starting with `-` (a flag, e.g. bare `zhao --help`) is
    // left to clap as normal.
    let args: Vec<String> = std::env::args().collect();
    if let Some(subcommand) = args.get(1).filter(|arg| !arg.starts_with('-')) {
        if !addon::is_builtin_subcommand(subcommand) {
            if let Some(addon_path) = addon::find_on_path(subcommand) {
                return addon::dispatch(&addon_path, &args[2..]);
            }
        }
    }

    let cli = Cli::parse();

    match &cli.command {
        Command::Check(args) => check::run(args),
        Command::Diff(args) => diff::run(args),
        Command::Lineage(args) => lineage::run(args),
        Command::Update(args) => update::run(args),
    }
}
