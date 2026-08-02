//! The `zhao check` command: diffs the current project against a
//! Baseline, evaluates zhao's Rules, and reports the result.

use std::path::Path;
use std::process::ExitCode;

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
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

    let baseline = match load_manifest(&args.state) {
        Ok(project) => project,
        Err(message) => return fail(&message),
    };
    let current = match load_manifest(&current_manifest) {
        Ok(project) => project,
        Err(message) => return fail(&message),
    };

    let changes = diff(&baseline, &current);
    let findings = evaluate(&baseline, &changes);
    let report = Report::new(&changes, &findings);

    match args.format {
        OutputFormat::Json => match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(err) => return fail(&format!("could not serialize report as JSON: {err}")),
        },
        OutputFormat::Text => print!("{}", render_text(&report)),
    }

    ExitCode::from(if report.is_breaking() {
        EXIT_BREAKING
    } else {
        EXIT_PASS
    })
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
