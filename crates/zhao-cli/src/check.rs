//! The `zhao check` command: the breaking-change gate. Runs the shared
//! engine (see [`crate::engine`]) and maps the result to an exit code a CI
//! job can act on.

use std::process::ExitCode;

use crate::cli::CheckArgs;
use crate::engine::{build_report, fail, print_report, write_run_metadata};

/// Exit code for "no breaking Change found."
const EXIT_PASS: u8 = 0;
/// Exit code for "at least one Rule fired at `error` Severity."
const EXIT_BREAKING: u8 = 1;

/// Runs `zhao check` and returns the process exit code.
pub fn run(args: &CheckArgs) -> ExitCode {
    let output = match build_report(args) {
        Ok(output) => output,
        Err(message) => return fail(&message),
    };
    if let Err(message) = print_report(&output.report, args) {
        return fail(&message);
    }
    if let Err(message) = write_run_metadata(&output, args) {
        return fail(&message);
    }

    ExitCode::from(if output.report.is_breaking() {
        EXIT_BREAKING
    } else {
        EXIT_PASS
    })
}
