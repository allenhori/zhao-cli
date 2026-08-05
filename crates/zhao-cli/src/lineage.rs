//! The `zhao lineage` command: a structural query over the current
//! project's compiled state -- what's upstream/downstream of a target
//! model, using dbt's own `+`-prefix/suffix selector syntax. Unlike
//! `zhao check`/`zhao diff`, this reads no Baseline, resolves no `--state`,
//! and never invokes `dbt compile` -- it operates purely on
//! `<project-dir>/target/manifest.json` as it already is.

use std::process::ExitCode;

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
use zhao_core::lineage::{ColumnLineageResult, Direction, LineageResult, trace, trace_column};

use crate::cli::LineageArgs;

/// Exit code for "ran successfully" -- `zhao lineage` is a query tool,
/// not a gate, so a successful (even empty) result always exits zero;
/// only a genuine failure to run at all (bad path, unparsable manifest,
/// unknown target) exits non-zero.
const EXIT_OK: u8 = 0;

/// Exit code shared with `zhao check`/`zhao diff` for "couldn't even
/// run" -- see [`crate::engine::fail`].
const EXIT_ERROR: u8 = 2;

/// Runs `zhao lineage` and returns the process exit code.
pub fn run(args: &LineageArgs) -> ExitCode {
    let manifest_path = args.project_dir.join("target").join("manifest.json");
    let project = match DbtAdapter.parse(&manifest_path) {
        Ok(project) => project,
        Err(err) => return fail(&format!("{}: {err}", manifest_path.display())),
    };

    let (target_name, target_column, direction) = args.parse_target();

    // `LineageError`'s own `Display` (via thiserror) already gives a
    // clear, actionable message for every variant -- `UnknownTarget`,
    // `AmbiguousTarget`, `UnknownColumn` alike -- so there's no need to
    // re-derive it per variant here.
    let text = match target_column {
        Some(column) => match trace_column(&project, target_name, column, direction) {
            Ok(result) => render_column_text(&result, direction, DbtAdapter.vocabulary()),
            Err(err) => return fail(&err.to_string()),
        },
        None => match trace(&project, target_name, direction) {
            Ok(result) => render_text(&result, target_name, direction, DbtAdapter.vocabulary()),
            Err(err) => return fail(&err.to_string()),
        },
    };

    print!("{text}");
    ExitCode::from(EXIT_OK)
}

/// Prints `message` to stderr as `error: {message}` and returns
/// [`EXIT_ERROR`] -- mirrors [`crate::engine::fail`], duplicated rather
/// than shared since `zhao lineage` doesn't otherwise depend on
/// `crate::engine`'s Baseline-diff-specific pipeline.
fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// Renders a [`LineageResult`] as human-readable text: an "Upstream:"
/// section (Nodes and Origins, via `vocabulary`'s own terms), a
/// "Downstream:" section, whichever `direction` didn't exclude -- or a
/// plain "nothing found" line if the included side(s) are genuinely
/// empty, never a bare blank output that could be mistaken for a
/// failure.
fn render_text(
    result: &LineageResult,
    target_name: &str,
    direction: Direction,
    vocabulary: &dyn zhao_core::adapters::AdapterVocabulary,
) -> String {
    let node_term = vocabulary.node_term();
    let origin_term = vocabulary.origin_term();
    let mut out = String::new();

    if matches!(direction, Direction::Upstream | Direction::Both) {
        out.push_str("Upstream:\n");
        if result.upstream_nodes.is_empty() && result.upstream_origins.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for id in &result.upstream_origins {
                out.push_str(&format!("  {origin_term} {id}\n"));
            }
            for id in &result.upstream_nodes {
                out.push_str(&format!("  {node_term} {id}\n"));
            }
        }
    }

    if matches!(direction, Direction::Downstream | Direction::Both) {
        out.push_str("Downstream:\n");
        if result.downstream_nodes.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for id in &result.downstream_nodes {
                out.push_str(&format!("  {node_term} {id}\n"));
            }
        }
    }

    if out.is_empty() {
        // Only reachable if `direction` somehow excluded both sides,
        // which `LineageArgs::parse_target` never produces -- kept as a
        // defensive fallback so this function can never silently print
        // nothing for a real target.
        out.push_str(&format!("{node_term} {target_name}: nothing found\n"));
    }

    out
}

/// The column-level mirror of [`render_text`]: an "Upstream:"/
/// "Downstream:" section per included side, each listing resolved
/// columns (`<term> <node-id>.<column>`, and Origins via
/// `origin_term()`) plus, separately, any Node reached whose specific
/// column mapping couldn't be resolved -- rendered as `<term> <node-id>
/// (unresolved)` so it's visibly present, never silently dropped or
/// indistinguishable from a fully-traced entry.
fn render_column_text(
    result: &ColumnLineageResult,
    direction: Direction,
    vocabulary: &dyn zhao_core::adapters::AdapterVocabulary,
) -> String {
    let node_term = vocabulary.node_term();
    let origin_term = vocabulary.origin_term();
    let mut out = String::new();

    if matches!(direction, Direction::Upstream | Direction::Both) {
        out.push_str("Upstream:\n");
        if result.upstream_columns.is_empty()
            && result.upstream_origins.is_empty()
            && result.unresolved_upstream_at.is_empty()
        {
            out.push_str("  (none)\n");
        } else {
            for origin_ref in &result.upstream_origins {
                out.push_str(&format!(
                    "  {origin_term} {}.{}\n",
                    origin_ref.origin, origin_ref.column
                ));
            }
            for column_ref in &result.upstream_columns {
                out.push_str(&format!(
                    "  {node_term} {}.{}\n",
                    column_ref.node, column_ref.column
                ));
            }
            for id in &result.unresolved_upstream_at {
                out.push_str(&format!("  {node_term} {id} (unresolved)\n"));
            }
        }
    }

    if matches!(direction, Direction::Downstream | Direction::Both) {
        out.push_str("Downstream:\n");
        if result.downstream_columns.is_empty() && result.unresolved_downstream_at.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for column_ref in &result.downstream_columns {
                out.push_str(&format!(
                    "  {node_term} {}.{}\n",
                    column_ref.node, column_ref.column
                ));
            }
            for id in &result.unresolved_downstream_at {
                out.push_str(&format!("  {node_term} {id} (unresolved)\n"));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::adapters::dbt::DbtVocabulary;
    use zhao_core::model::{NodeId, OriginId};

    #[test]
    fn render_text_lists_both_sections_for_both_directions() {
        let result = LineageResult {
            upstream_nodes: vec![NodeId::new("model.p.a")],
            upstream_origins: vec![OriginId::new("source.p.raw")],
            downstream_nodes: vec![NodeId::new("model.p.c")],
        };
        let text = render_text(&result, "b", Direction::Both, &DbtVocabulary);

        assert!(text.contains("Upstream:\n"), "{text}");
        assert!(text.contains("  source source.p.raw\n"), "{text}");
        assert!(text.contains("  model model.p.a\n"), "{text}");
        assert!(text.contains("Downstream:\n"), "{text}");
        assert!(text.contains("  model model.p.c\n"), "{text}");
    }

    #[test]
    fn render_text_omits_the_downstream_section_for_upstream_only() {
        let result = LineageResult {
            upstream_nodes: vec![NodeId::new("model.p.a")],
            upstream_origins: Vec::new(),
            downstream_nodes: Vec::new(),
        };
        let text = render_text(&result, "b", Direction::Upstream, &DbtVocabulary);

        assert!(text.contains("Upstream:\n"), "{text}");
        assert!(!text.contains("Downstream:\n"), "{text}");
    }

    #[test]
    fn render_text_omits_the_upstream_section_for_downstream_only() {
        let result = LineageResult {
            upstream_nodes: Vec::new(),
            upstream_origins: Vec::new(),
            downstream_nodes: vec![NodeId::new("model.p.c")],
        };
        let text = render_text(&result, "b", Direction::Downstream, &DbtVocabulary);

        assert!(!text.contains("Upstream:\n"), "{text}");
        assert!(text.contains("Downstream:\n"), "{text}");
    }

    #[test]
    fn render_text_reports_none_for_an_empty_included_side_not_a_blank_output() {
        let result = LineageResult::default();
        let text = render_text(&result, "isolated", Direction::Both, &DbtVocabulary);

        assert!(text.contains("Upstream:\n  (none)\n"), "{text}");
        assert!(text.contains("Downstream:\n  (none)\n"), "{text}");
    }

    #[test]
    fn render_column_text_lists_resolved_columns_and_origins() {
        let result = ColumnLineageResult {
            upstream_columns: vec![zhao_core::lineage::ColumnRef {
                node: NodeId::new("model.p.a"),
                column: zhao_core::model::ColumnName::new("x"),
            }],
            upstream_origins: vec![zhao_core::lineage::OriginColumnRef {
                origin: OriginId::new("source.p.raw"),
                column: zhao_core::model::ColumnName::new("x"),
            }],
            unresolved_upstream_at: Vec::new(),
            downstream_columns: vec![zhao_core::lineage::ColumnRef {
                node: NodeId::new("model.p.c"),
                column: zhao_core::model::ColumnName::new("x"),
            }],
            unresolved_downstream_at: Vec::new(),
        };
        let text = render_column_text(&result, Direction::Both, &DbtVocabulary);

        assert!(text.contains("  source source.p.raw.x\n"), "{text}");
        assert!(text.contains("  model model.p.a.x\n"), "{text}");
        assert!(text.contains("  model model.p.c.x\n"), "{text}");
    }

    /// Acceptance criterion: an unresolved column is visibly reported,
    /// distinguishable from a fully-resolved entry -- never silently
    /// dropped or indistinguishable from "nothing here."
    #[test]
    fn render_column_text_reports_unresolved_nodes_distinctly() {
        let result = ColumnLineageResult {
            upstream_columns: Vec::new(),
            upstream_origins: Vec::new(),
            unresolved_upstream_at: vec![NodeId::new("model.p.b")],
            downstream_columns: Vec::new(),
            unresolved_downstream_at: Vec::new(),
        };
        let text = render_column_text(&result, Direction::Upstream, &DbtVocabulary);

        assert!(text.contains("  model model.p.b (unresolved)\n"), "{text}");
        assert!(
            !text.contains("(none)"),
            "an unresolved entry means this side isn't genuinely empty: {text}"
        );
    }

    #[test]
    fn render_column_text_reports_none_when_genuinely_empty() {
        let result = ColumnLineageResult::default();
        let text = render_column_text(&result, Direction::Both, &DbtVocabulary);

        assert!(text.contains("Upstream:\n  (none)\n"), "{text}");
        assert!(text.contains("Downstream:\n  (none)\n"), "{text}");
    }
}
