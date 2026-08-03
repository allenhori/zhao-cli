//! Writes `target/zhao/run-metadata.json`: the full Change/Rule/Lineage
//! Edge breakdown for a `zhao check`/`zhao diff` run, as a standalone
//! artifact any consumer (a future zhao-cloud, a hand-rolled dashboard, a
//! curious engineer with `jq`) can read without re-running zhao itself.
//!
//! Deliberately minimal and self-contained: no raw row data, no
//! connection strings or credentials, no cloud service/API/endpoint
//! reference of any kind. This is a standard, standalone zhao-cli
//! feature -- not a hook, not a client, not something that phones home.
//! `tests/metadata.rs`'s `run_metadata_json_field_set_is_exactly_this_and_nothing_else`
//! pins the exact field set for this reason: any future field added here
//! should be a deliberate decision, not an accident.

use std::path::Path;

use serde::Serialize;
use zhao_core::model::{LineageEdge, ParsedProject, Upstream};

use crate::report::Report;

/// `target/zhao/run-metadata.json`'s full contents: the same Change/
/// Finding/staleness/recommended-command data a `--format json` run
/// prints to stdout (flattened in directly, so there's exactly one
/// source of truth for those fields -- see [`Report`]), plus the current
/// state's full Lineage Edge breakdown, which stdout's report never
/// includes.
#[derive(Debug, Serialize)]
pub struct RunMetadata<'a> {
    /// Every field a `--format json` run of `zhao check`/`zhao diff`
    /// would print: `changes`, `findings`, `staleness_warning` (if any),
    /// `recommended_command` (if any). Borrowed, not owned, so building
    /// this doesn't require giving up the caller's own copy of `report`
    /// (e.g. still needed afterward to compute an exit code).
    #[serde(flatten)]
    pub report: &'a Report,
    /// Every Lineage Edge in the current project's state -- the full
    /// dependency graph a Change/Finding was evaluated against, not just
    /// the edges a Change happened to touch.
    pub lineage_edges: Vec<LineageEdgeJson>,
}

impl<'a> RunMetadata<'a> {
    /// Builds a [`RunMetadata`] from a completed [`Report`] and the
    /// current project state's [`ParsedProject`].
    pub fn new(report: &'a Report, current: &ParsedProject) -> Self {
        Self {
            report,
            lineage_edges: current.edges.iter().map(LineageEdgeJson::from).collect(),
        }
    }
}

/// A [`LineageEdge`], reshaped for JSON output -- same convention as
/// [`crate::report::ChangeJson`]/[`crate::report::FindingJson`]:
/// zhao-core's own types carry no serialization derives, so this module
/// owns the JSON shape as its own concern.
#[derive(Debug, Serialize)]
pub struct LineageEdgeJson {
    /// The upstream dependency this edge points from.
    pub upstream: UpstreamJson,
    /// The downstream Node this edge points to.
    pub downstream: String,
    /// Column-level detail, when the specific column mapping was
    /// resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<ColumnLineageJson>,
}

impl From<&LineageEdge> for LineageEdgeJson {
    fn from(edge: &LineageEdge) -> Self {
        Self {
            upstream: UpstreamJson::from(&edge.upstream),
            downstream: edge.downstream.to_string(),
            column: edge.column.as_ref().map(|column| ColumnLineageJson {
                upstream_column: column.upstream_column.to_string(),
                downstream_column: column.downstream_column.to_string(),
            }),
        }
    }
}

/// An [`Upstream`], reshaped for JSON output.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpstreamJson {
    /// See [`Upstream::Node`].
    Node {
        /// The upstream Node's ID.
        id: String,
    },
    /// See [`Upstream::Origin`].
    Origin {
        /// The upstream Origin's ID.
        id: String,
    },
}

impl From<&Upstream> for UpstreamJson {
    fn from(upstream: &Upstream) -> Self {
        match upstream {
            Upstream::Node(id) => UpstreamJson::Node { id: id.to_string() },
            Upstream::Origin(id) => UpstreamJson::Origin { id: id.to_string() },
        }
    }
}

/// A [`zhao_core::model::ColumnLineage`], reshaped for JSON output.
#[derive(Debug, Serialize)]
pub struct ColumnLineageJson {
    /// The column on the upstream Node or Origin the data comes from.
    pub upstream_column: String,
    /// The column on the downstream Node that receives it.
    pub downstream_column: String,
}

/// Writes `<project_dir>/target/zhao/run-metadata.json`, creating the
/// `target/zhao/` directory if it doesn't already exist.
///
/// Written atomically: the JSON is written to a temp file in the same
/// directory first, then renamed into place, so a failure partway through
/// (disk full, process killed, ...) can never leave a truncated or
/// otherwise corrupt `run-metadata.json` where a valid one used to be --
/// the old file (if any) stays exactly as it was until the new one is
/// fully written and ready to swap in.
pub fn write(metadata: &RunMetadata, project_dir: &Path) -> Result<(), String> {
    let dir = project_dir.join("target").join("zhao");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;

    let json = serde_json::to_string_pretty(metadata)
        .map_err(|err| format!("could not serialize run metadata as JSON: {err}"))?;

    let mut temp_file = tempfile::NamedTempFile::new_in(&dir)
        .map_err(|err| format!("could not create a temp file in {}: {err}", dir.display()))?;
    std::io::Write::write_all(&mut temp_file, json.as_bytes())
        .map_err(|err| format!("could not write run metadata: {err}"))?;

    let path = dir.join("run-metadata.json");
    temp_file
        .persist(&path)
        .map_err(|err| format!("could not finalize {}: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::model::{ColumnLineage, ColumnName, NodeId, OriginId};

    #[test]
    fn lineage_edge_json_carries_node_upstream_and_column_detail() {
        let edge = LineageEdge {
            upstream: Upstream::Node(NodeId::new("model.a")),
            downstream: NodeId::new("model.b"),
            column: Some(ColumnLineage {
                upstream_column: ColumnName::new("id"),
                downstream_column: ColumnName::new("a_id"),
            }),
        };

        let json = LineageEdgeJson::from(&edge);

        assert!(matches!(json.upstream, UpstreamJson::Node { id } if id == "model.a"));
        assert_eq!(json.downstream, "model.b");
        let column = json.column.expect("column detail should be present");
        assert_eq!(column.upstream_column, "id");
        assert_eq!(column.downstream_column, "a_id");
    }

    #[test]
    fn lineage_edge_json_carries_origin_upstream_and_no_column_detail() {
        let edge = LineageEdge {
            upstream: Upstream::Origin(OriginId::new("source.raw.customers")),
            downstream: NodeId::new("model.b"),
            column: None,
        };

        let json = LineageEdgeJson::from(&edge);

        assert!(
            matches!(json.upstream, UpstreamJson::Origin { id } if id == "source.raw.customers")
        );
        assert!(json.column.is_none());
    }

    #[test]
    fn run_metadata_flattens_report_fields_alongside_lineage_edges() {
        let report = Report::new(&[], &[]);
        let current = ParsedProject {
            nodes: Vec::new(),
            origins: Vec::new(),
            edges: vec![LineageEdge {
                upstream: Upstream::Node(NodeId::new("model.a")),
                downstream: NodeId::new("model.b"),
                column: None,
            }],
        };

        let metadata = RunMetadata::new(&report, &current);
        let json = serde_json::to_value(&metadata).expect("should serialize");

        assert!(json.get("changes").is_some(), "{json}");
        assert!(json.get("findings").is_some(), "{json}");
        assert_eq!(
            json.get("lineage_edges")
                .and_then(|edges| edges.as_array())
                .map(Vec::len),
            Some(1)
        );
    }
}
