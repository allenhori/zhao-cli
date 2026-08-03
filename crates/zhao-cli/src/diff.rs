//! The `zhao diff` command: runs the identical engine as `zhao check` (see
//! [`crate::engine`]) -- Baseline resolution, diff, Rule evaluation, report
//! rendering -- but without any gate semantics. Always exits zero
//! regardless of what Severity outcomes are present, for local inspection
//! during development rather than CI gating.

use std::process::ExitCode;

use crate::cli::CheckArgs;
use crate::engine::{build_report, fail, print_report, write_run_metadata};

/// Exit code for "ran successfully" -- used unconditionally, regardless of
/// Severity outcomes present. `zhao diff` is an inspection tool, not a
/// gate; use `zhao check` for CI.
const EXIT_OK: u8 = 0;

/// Runs `zhao diff` and returns the process exit code.
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

    ExitCode::from(EXIT_OK)
}
