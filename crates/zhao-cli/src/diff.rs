//! The `zhao diff` command: runs the identical engine as `zhao check` (see
//! [`crate::engine`]) -- Baseline resolution, diff, Rule evaluation, report
//! rendering -- but without any gate semantics. Always exits zero
//! regardless of what Severity outcomes are present, for local inspection
//! during development rather than CI gating.

use std::process::ExitCode;

use crate::cli::CheckArgs;
use crate::engine::{build_report, fail, print_report};

/// Exit code for "ran successfully" -- used unconditionally, regardless of
/// Severity outcomes present. `zhao diff` is an inspection tool, not a
/// gate; use `zhao check` for CI.
const EXIT_OK: u8 = 0;
/// Exit code for "couldn't even run" (bad paths, unparsable manifests,
/// ...), matching `zhao check`'s own error exit code.
const EXIT_ERROR: u8 = 2;

/// Runs `zhao diff` and returns the process exit code.
pub fn run(args: &CheckArgs) -> ExitCode {
    let report = match build_report(args) {
        Ok(report) => report,
        Err(message) => return fail(&message, EXIT_ERROR),
    };
    if let Err(message) = print_report(&report, args) {
        return fail(&message, EXIT_ERROR);
    }

    ExitCode::from(EXIT_OK)
}
