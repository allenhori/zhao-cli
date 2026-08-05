//! The `zhao lineage` command: a structural query over the current
//! project's compiled state -- what's upstream/downstream of a target
//! model, using dbt's own `+`-prefix/suffix selector syntax. Unlike
//! `zhao check`/`zhao diff`, this reads no Baseline, resolves no `--state`,
//! and never invokes `dbt compile` -- it operates purely on
//! `<project-dir>/target/manifest.json` as it already is.

use std::process::ExitCode;

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
use zhao_core::lineage::{Direction, LineageError, LineageResult, trace};

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

    let (target_name, direction) = args.parse_target();
    let result = match trace(&project, target_name, direction) {
        Ok(result) => result,
        Err(LineageError::UnknownTarget { name }) => {
            return fail(&format!("no model named {name:?} was found in the project"));
        }
    };

    print!(
        "{}",
        render_text(&result, target_name, direction, DbtAdapter.vocabulary())
    );
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
}
