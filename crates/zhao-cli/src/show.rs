//! The `zhao show` command: a thin, wrapper-aware wrapper around dbt's
//! own `dbt show`, previewing a model's/seed's/source's query results
//! capped by a configurable row limit. Works against both dbt-core and
//! dbt Fusion projects, reusing the exact same `--dbt-command`/
//! `zhao.yml` wrapper-resolution machinery `zhao lineage`/`zhao check`
//! already use (see [`crate::dbt_target::resolve_dbt_invocation`]).
//!
//! `--output json` doesn't relay dbt's own JSON verbatim: dbt-core and
//! dbt Fusion emit genuinely different shapes on stdout (confirmed
//! against real installs of both) -- dbt-core wraps the row array in a
//! `{"node": ..., "show": [...]}` object and writes it pretty-printed
//! alongside its own INFO-level log lines on the *same* stdout stream;
//! Fusion emits a bare `[...]` array, compact, with its own banner/
//! progress lines both before and after it on stdout too. Neither
//! engine's raw stdout is valid JSON on its own. [`extract_show_result`]
//! is the pure function that makes sense of either shape and normalizes
//! both into one stable `{"columns": [...], "rows": [...]}` result --
//! the actual contract a consumer like `zhao-vscode-ext` can rely on.

use std::process::ExitCode;

use serde::Serialize;
use zhao_core::config::Config;

use crate::adapter::ResolvedAdapter;
use crate::cli::{ShowArgs, ShowOutputFormat};

/// Exit code for "ran successfully" -- `zhao show` is a query tool, not a
/// gate, matching `zhao lineage`'s own `EXIT_OK`.
const EXIT_OK: u8 = 0;

/// Exit code for "couldn't even run" -- shared convention with `zhao
/// lineage`/`zhao check`/`zhao diff`.
const EXIT_ERROR: u8 = 2;

/// `zhao show`'s hardcoded row-limit fallback, used only when neither
/// `--limit` nor `zhao.yml`'s `show.default_limit` is set.
pub const DEFAULT_LIMIT: u32 = 50;

/// Runs `zhao show` and returns the process exit code.
pub fn run(args: &ShowArgs) -> ExitCode {
    let config = match Config::load_for_project(&args.project_dir) {
        Ok(config) => config,
        Err(err) => return fail(&err.to_string()),
    };
    let adapter = match ResolvedAdapter::resolve(&args.project_dir, config.tool()) {
        Ok(adapter) => adapter,
        Err(err) => return fail(&err.to_string()),
    };

    // Same `--dbt-command`/`--dbt-arg`/`--dbt-args` precedence as `zhao
    // lineage`/`zhao check` -- a project already using its own wrapper
    // shouldn't need `zhao show` to be the one place that still
    // hardcodes `"dbt"`.
    let dbt_passthrough_args = match args.dbt_passthrough_args() {
        Ok(args) => args,
        Err(err) => return fail(&err),
    };
    let (dbt_command, dbt_passthrough_args) = match crate::dbt_target::resolve_dbt_invocation(
        args.dbt_command.as_deref(),
        dbt_passthrough_args,
        &config,
    ) {
        Ok(resolved) => resolved,
        Err(err) => return fail(&err),
    };

    let limit = resolve_limit(args.limit, config.show_default_limit());
    let output_json = matches!(args.output, ShowOutputFormat::Json);

    let output = match adapter.show(
        &args.project_dir,
        &dbt_command,
        &args.target,
        limit,
        output_json,
        &dbt_passthrough_args,
    ) {
        Ok(output) => output,
        Err(err) => return fail(&err.to_string()),
    };

    if !output_json {
        // Default: relay dbt's own human-readable table verbatim --
        // running `zhao show` in a terminal should feel like running
        // `dbt show` directly.
        print!("{}", output.stdout);
        return ExitCode::from(EXIT_OK);
    }

    match extract_show_result(&output.stdout) {
        Ok(result) => {
            let json = serde_json::to_string_pretty(&ShowJsonOutput {
                columns: result.columns,
                rows: result.rows,
            })
            .expect("a normalized show result should always serialize");
            println!("{json}");
            ExitCode::from(EXIT_OK)
        }
        Err(err) => fail(&format!(
            "dbt show ran successfully but its output couldn't be parsed as the expected JSON \
             shape: {err}\n\nraw dbt output:\n{}",
            output.stdout
        )),
    }
}

/// Resolves `zhao show`'s effective row limit: `--limit`, if given; else
/// `zhao.yml`'s `show.default_limit`; else [`DEFAULT_LIMIT`]. A pure
/// function -- no I/O, easy to test every combination of directly.
fn resolve_limit(cli_limit: Option<u32>, config_limit: Option<u32>) -> u32 {
    cli_limit.or(config_limit).unwrap_or(DEFAULT_LIMIT)
}

/// `zhao show --output json`'s normalized, stable result shape -- see
/// the module doc comment for why this isn't just dbt's own JSON relayed
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShowResult {
    /// Column names, in the same order dbt itself returned them (not
    /// resorted) -- derived from the first row's keys; empty if there
    /// are no rows at all.
    columns: Vec<String>,
    /// Each row, as a JSON object -- preserved key order, same reasoning
    /// as `columns`.
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// The JSON shape actually printed to stdout for `--output json` --
/// [`ShowResult`] is `zhao-cli`-internal; this is what a consumer like
/// `zhao-vscode-ext` actually parses.
#[derive(Debug, Serialize)]
struct ShowJsonOutput {
    columns: Vec<String>,
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Extracts and normalizes `dbt show --output json`'s result from
/// `raw_stdout`, tolerating either engine's log-line noise around the
/// actual payload (see the module doc comment) and either engine's own
/// JSON shape (dbt-core's `{"node", "show"}` wrapper vs. Fusion's bare
/// array).
fn extract_show_result(raw_stdout: &str) -> Result<ShowResult, String> {
    let value = extract_json_value(raw_stdout)?;
    normalize_show_value(value)
}

/// Scans `raw` for the first well-formed JSON value (a `{...}` or
/// `[...]`, brace/bracket-matched with string-quoting awareness so a
/// literal `{`/`[`/`}`/`]` inside a string value doesn't throw off the
/// scan), ignoring any non-JSON text before or after it. Both dbt-core
/// and dbt Fusion interleave their own log/progress lines with
/// `--output json`'s actual payload on the same stdout stream, so this
/// can't simply parse `raw` as JSON outright -- confirmed against real
/// installs of both engines.
fn extract_json_value(raw: &str) -> Result<serde_json::Value, String> {
    let bytes = raw.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start] == b'{' || bytes[start] == b'[' {
            if let Some(end) = matching_close(raw, start) {
                let candidate = &raw[start..=end];
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
                    return Ok(value);
                }
            }
        }
        start += 1;
    }
    Err("no JSON value found in dbt's output".to_string())
}

/// Finds the byte index of the bracket/brace that closes the one opened
/// at `open_idx`, respecting JSON string quoting (so a `{`/`}`/`[`/`]`
/// inside a string literal is never mistaken for real nesting).
fn matching_close(s: &str, open_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate().skip(open_idx) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Normalizes an already-extracted JSON [`serde_json::Value`] (dbt-
/// core's `{"node", "show"}` wrapper, or dbt Fusion's bare array) into
/// [`ShowResult`].
fn normalize_show_value(value: serde_json::Value) -> Result<ShowResult, String> {
    let rows_value = match value {
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(mut obj) => match obj.remove("show") {
            Some(serde_json::Value::Array(rows)) => rows,
            Some(other) => {
                return Err(format!(
                    "expected \"show\" to be a JSON array, got: {other}"
                ));
            }
            None => return Err("expected a \"show\" key in dbt's JSON output".to_string()),
        },
        other => {
            return Err(format!(
                "unexpected top-level JSON shape from dbt show: {other}"
            ));
        }
    };

    let mut rows = Vec::with_capacity(rows_value.len());
    for row in rows_value {
        match row {
            serde_json::Value::Object(map) => rows.push(map),
            other => {
                return Err(format!(
                    "expected each row to be a JSON object, got: {other}"
                ));
            }
        }
    }

    let columns = rows
        .first()
        .map(|row| row.keys().cloned().collect())
        .unwrap_or_default();

    Ok(ShowResult { columns, rows })
}

/// Prints `message` to stderr as `error: {message}` and returns
/// [`EXIT_ERROR`] -- mirrors `zhao lineage`'s own `fail`.
fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // `resolve_limit` -- the three-way precedence.
    // -----------------------------------------------------------------

    #[test]
    fn cli_limit_wins_over_everything() {
        assert_eq!(resolve_limit(Some(10), Some(200)), 10);
    }

    #[test]
    fn config_limit_wins_when_no_cli_limit_is_given() {
        assert_eq!(resolve_limit(None, Some(200)), 200);
    }

    #[test]
    fn hardcoded_default_wins_when_neither_is_set() {
        assert_eq!(resolve_limit(None, None), DEFAULT_LIMIT);
        assert_eq!(DEFAULT_LIMIT, 50);
    }

    // -----------------------------------------------------------------
    // `extract_show_result` -- real shapes captured from a real dbt-core
    // v1 install and a real dbt Fusion (v2) install, not hypothetical
    // ones (see ticket #88's cross-engine acceptance testing).
    // -----------------------------------------------------------------

    #[test]
    fn extracts_dbt_cores_object_wrapped_shape_around_log_noise() {
        let raw = "\u{1b}[0m02:11:29  Running with dbt=1.10.23\n\u{1b}[0m02:11:29  Found 12 models\n{\n  \"node\": \"customers\",\n  \"show\": [\n    {\"customer_id\": \"abc\", \"customer_name\": \"Joy Lam\"},\n    {\"customer_id\": \"def\", \"customer_name\": \"Tyler Henderson\"}\n  ]\n}\n";

        let result = extract_show_result(raw).expect("should extract successfully");
        assert_eq!(result.columns, vec!["customer_id", "customer_name"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["customer_name"], "Joy Lam");
    }

    #[test]
    fn extracts_fusions_bare_array_shape_around_banner_and_trailing_summary_noise() {
        let raw = "dbt-fusion 2.0.0-preview.218\n   Loading profiles.yml\n[{\"customer_id\":\"abc\",\"customer_name\":\"Todd Burton\"}]\n Succeeded [  0.05s] model main.customers (table)\n\n==================== Execution Summary =====================\nFinished 'show' successfully for target 'dev' [1.0s]\n";

        let result = extract_show_result(raw).expect("should extract successfully");
        assert_eq!(result.columns, vec!["customer_id", "customer_name"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["customer_name"], "Todd Burton");
    }

    #[test]
    fn an_empty_result_set_yields_empty_columns_not_an_error() {
        let raw = "[]";
        let result = extract_show_result(raw).expect("an empty array is a valid result");
        assert_eq!(result.columns, Vec::<String>::new());
        assert!(result.rows.is_empty());
    }

    #[test]
    fn column_order_is_preserved_not_resorted() {
        let raw = r#"[{"zeta": 1, "alpha": 2, "middle": 3}]"#;
        let result = extract_show_result(raw).expect("should extract successfully");
        assert_eq!(result.columns, vec!["zeta", "alpha", "middle"]);
    }

    #[test]
    fn no_json_in_the_output_at_all_is_a_clear_error_not_a_panic() {
        let raw = "some dbt log line with no JSON anywhere in it\n";
        let err = extract_show_result(raw).expect_err("there is no JSON to extract");
        assert!(err.contains("no JSON value found"), "{err}");
    }

    #[test]
    fn a_brace_inside_a_string_value_does_not_confuse_the_bracket_matcher() {
        let raw = r#"[{"description": "a value with a { brace and a [ bracket inside it"}]"#;
        let result = extract_show_result(raw)
            .expect("should extract successfully despite the embedded brace/bracket");
        assert_eq!(result.rows.len(), 1);
    }
}
