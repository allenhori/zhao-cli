//! Classifying [`Change`]s into [`Finding`]s: which of zhao's Rules fired,
//! at what Severity, and which downstream Node it actually reaches.

use crate::diff::Change;
use crate::model::{ColumnName, NodeId, ParsedProject, Upstream};

/// A Rule's configured response when it fires on a Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fails the CI gate.
    Error,
    /// Annotates the output but doesn't fail the gate.
    Warn,
    /// Recorded internally but not surfaced as an issue.
    Pass,
}

/// One of zhao's built-in Rules: a specific, named kind of semantic Change
/// the engine can detect and classify. `v1` ships with a single Rule;
/// more are added to this enum as the catalog grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleId {
    /// A column was removed from a Node while a downstream Node still
    /// held an active (column-level-resolved) reference to it in the
    /// Baseline.
    ColumnRemovedWithActiveReferences,
}

impl RuleId {
    /// This Rule's Severity by default, before any `zhao.yml` override.
    pub fn default_severity(self) -> Severity {
        match self {
            RuleId::ColumnRemovedWithActiveReferences => Severity::Error,
        }
    }
}

/// A single Rule's classification of a Change's impact on a specific
/// downstream Node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Which Rule fired.
    pub rule: RuleId,
    /// The Rule's Severity for this Finding.
    pub severity: Severity,
    /// The Node the Change happened on.
    pub node: NodeId,
    /// The column the Change happened to.
    pub column: ColumnName,
    /// The downstream Node this Change actually reaches.
    pub reached: NodeId,
    /// The column on `reached` through which it depends on `column`.
    pub reached_column: ColumnName,
}

/// Evaluates every Rule against a Change list, using `baseline`'s Lineage
/// Edges to determine which downstream Nodes actually held an active
/// reference before the Change happened.
///
/// `baseline`, not the current state, is deliberate: a removed column's
/// Lineage Edge no longer exists once it's gone -- the only place a
/// "was this actively referenced" question can be answered from is the
/// state where the reference still existed.
pub fn evaluate(baseline: &ParsedProject, changes: &[Change]) -> Vec<Finding> {
    changes
        .iter()
        .flat_map(|change| column_removed_with_active_references(baseline, change))
        .collect()
}

fn column_removed_with_active_references(
    baseline: &ParsedProject,
    change: &Change,
) -> Vec<Finding> {
    let Change::ColumnRemoved { node, column } = change else {
        return Vec::new();
    };

    baseline
        .edges
        .iter()
        .filter(|edge| edge.upstream == Upstream::Node(node.clone()))
        .filter_map(|edge| {
            let lineage = edge.column.as_ref()?;
            (&lineage.upstream_column == column).then(|| Finding {
                rule: RuleId::ColumnRemovedWithActiveReferences,
                severity: RuleId::ColumnRemovedWithActiveReferences.default_severity(),
                node: node.clone(),
                column: column.clone(),
                reached: edge.downstream.clone(),
                reached_column: lineage.downstream_column.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnLineage, LineageEdge, Node};

    fn node_id(s: &str) -> NodeId {
        NodeId::new(s)
    }

    fn column(s: &str) -> ColumnName {
        ColumnName::new(s)
    }

    #[test]
    fn fires_when_baseline_shows_an_active_reference_to_the_removed_column() {
        let baseline = ParsedProject {
            nodes: vec![
                Node {
                    id: node_id("model.a"),
                    name: "a".to_string(),
                    columns: vec![],
                    joins: vec![],
                },
                Node {
                    id: node_id("model.b"),
                    name: "b".to_string(),
                    columns: vec![],
                    joins: vec![],
                },
            ],
            origins: vec![],
            edges: vec![LineageEdge {
                upstream: Upstream::Node(node_id("model.a")),
                downstream: node_id("model.b"),
                column: Some(ColumnLineage {
                    upstream_column: column("x"),
                    downstream_column: column("x"),
                }),
            }],
        };
        let changes = vec![Change::ColumnRemoved {
            node: node_id("model.a"),
            column: column("x"),
        }];

        let findings = evaluate(&baseline, &changes);

        assert_eq!(
            findings,
            vec![Finding {
                rule: RuleId::ColumnRemovedWithActiveReferences,
                severity: Severity::Error,
                node: node_id("model.a"),
                column: column("x"),
                reached: node_id("model.b"),
                reached_column: column("x"),
            }]
        );
    }

    #[test]
    fn does_not_fire_when_no_downstream_node_referenced_the_removed_column() {
        let baseline = ParsedProject {
            nodes: vec![Node {
                id: node_id("model.a"),
                name: "a".to_string(),
                columns: vec![],
                joins: vec![],
            }],
            origins: vec![],
            // A node-level-only edge (no column resolved) shouldn't count
            // as an "active reference" to this specific column.
            edges: vec![LineageEdge {
                upstream: Upstream::Node(node_id("model.a")),
                downstream: node_id("model.b"),
                column: None,
            }],
        };
        let changes = vec![Change::ColumnRemoved {
            node: node_id("model.a"),
            column: column("x"),
        }];

        assert_eq!(evaluate(&baseline, &changes), Vec::new());
    }

    #[test]
    fn does_not_fire_for_a_column_removal_with_no_downstream_edges_at_all() {
        let baseline = ParsedProject {
            nodes: vec![Node {
                id: node_id("model.a"),
                name: "a".to_string(),
                columns: vec![],
                joins: vec![],
            }],
            origins: vec![],
            edges: vec![],
        };
        let changes = vec![Change::ColumnRemoved {
            node: node_id("model.a"),
            column: column("x"),
        }];

        assert_eq!(evaluate(&baseline, &changes), Vec::new());
    }

    #[test]
    fn ignores_changes_other_than_column_removed() {
        let baseline = ParsedProject {
            nodes: vec![],
            origins: vec![],
            edges: vec![],
        };
        let changes = vec![Change::ColumnAdded {
            node: node_id("model.a"),
            column: column("x"),
        }];

        assert_eq!(evaluate(&baseline, &changes), Vec::new());
    }
}
