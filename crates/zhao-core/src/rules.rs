//! Classifying [`Change`]s into [`Finding`]s: which of zhao's Rules fired,
//! at what Severity, and the specifics of what it found.
//!
//! ## Known limitations
//!
//! - **Type narrowing** is only detected for a small, explicit set of
//!   recognized integer type names (`tinyint`/`smallint`/`int`/`integer`/
//!   `bigint`). An unrecognized type name -- including non-integer types
//!   like `varchar` -- never fires this Rule. Guessing "this is narrower"
//!   for a comparison we can't actually make sense of would be worse than
//!   not firing at all.
//! - **Join cardinality** is ranked `Inner` (tightest) < `Left`/`Right`
//!   (tied) < `Full` (loosest). `Cross` is deliberately excluded from the
//!   ranking -- a cartesian product isn't a "loosening" of row-matching
//!   semantics in the same sense the others are, and comparing it against
//!   them would overstate what this Rule actually knows.

use crate::config::Config;
use crate::diff::Change;
use crate::model::{ColumnName, JoinKind, NodeId, ParsedProject, Upstream};

/// A Rule's configured response when it fires on a Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fails the CI gate.
    Error,
    /// Annotates the output but doesn't fail the gate.
    Warn,
    /// Recorded but not treated as an issue -- informational.
    Pass,
}

impl Severity {
    /// Parses a Severity's `zhao.yml` configuration name.
    pub fn from_config_name(name: &str) -> Option<Severity> {
        match name {
            "error" => Some(Severity::Error),
            "warn" => Some(Severity::Warn),
            "pass" => Some(Severity::Pass),
            _ => None,
        }
    }
}

/// One of zhao's built-in Rules: a specific, named kind of semantic Change
/// the engine can detect and classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleId {
    /// A column was removed from a Node while a downstream Node still
    /// held an active (column-level-resolved) reference to it in the
    /// Baseline.
    ColumnRemovedWithActiveReferences,
    /// A column's documented type narrowed (e.g. `bigint` to `int`).
    ColumnTypeNarrowed,
    /// A join's cardinality loosened (e.g. `INNER` to `LEFT`/`FULL`).
    JoinCardinalityLoosened,
    /// A column was added. Informational by default.
    ColumnAdded,
}

impl RuleId {
    /// This Rule's Severity by default, before any `zhao.yml` override.
    pub fn default_severity(self) -> Severity {
        match self {
            RuleId::ColumnRemovedWithActiveReferences => Severity::Error,
            RuleId::ColumnTypeNarrowed => Severity::Warn,
            RuleId::JoinCardinalityLoosened => Severity::Warn,
            RuleId::ColumnAdded => Severity::Pass,
        }
    }

    /// This Rule's canonical name in `zhao.yml` configuration.
    pub fn config_name(self) -> &'static str {
        match self {
            RuleId::ColumnRemovedWithActiveReferences => "column-removed-with-active-references",
            RuleId::ColumnTypeNarrowed => "column-type-narrowed",
            RuleId::JoinCardinalityLoosened => "join-cardinality-loosened",
            RuleId::ColumnAdded => "column-added",
        }
    }

    /// Parses a Rule's `zhao.yml` configuration name back into a `RuleId`.
    pub fn from_config_name(name: &str) -> Option<RuleId> {
        match name {
            "column-removed-with-active-references" => {
                Some(RuleId::ColumnRemovedWithActiveReferences)
            }
            "column-type-narrowed" => Some(RuleId::ColumnTypeNarrowed),
            "join-cardinality-loosened" => Some(RuleId::JoinCardinalityLoosened),
            "column-added" => Some(RuleId::ColumnAdded),
            _ => None,
        }
    }

    /// Every Rule in the v1 catalog, in declaration order. Exists so
    /// callers (e.g. an "unknown rule name" error message) can list valid
    /// names without duplicating the catalog themselves.
    pub fn all() -> [RuleId; 4] {
        [
            RuleId::ColumnRemovedWithActiveReferences,
            RuleId::ColumnTypeNarrowed,
            RuleId::JoinCardinalityLoosened,
            RuleId::ColumnAdded,
        ]
    }
}

/// The specifics of a single Rule's match -- exactly the fields relevant
/// to that Rule, not a one-size-fits-all shape padded with fields that
/// are meaningless for some Rules (a "column added" Finding, for
/// instance, has no downstream Node it "reaches").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingDetail {
    /// See [`RuleId::ColumnRemovedWithActiveReferences`].
    ColumnRemovedWithActiveReferences {
        /// The Node the column was removed from.
        node: NodeId,
        /// The removed column.
        column: ColumnName,
        /// The downstream Node that actively referenced it.
        reached: NodeId,
        /// The column on `reached` through which it depended on `column`.
        reached_column: ColumnName,
    },
    /// See [`RuleId::ColumnTypeNarrowed`].
    ColumnTypeNarrowed {
        /// The Node the column belongs to.
        node: NodeId,
        /// The column whose type narrowed.
        column: ColumnName,
        /// The documented type in the Baseline.
        from_type: String,
        /// The documented type in the current state.
        to_type: String,
    },
    /// See [`RuleId::JoinCardinalityLoosened`].
    JoinCardinalityLoosened {
        /// The Node whose join loosened.
        node: NodeId,
        /// The join's position (0-indexed).
        position: usize,
        /// The join's kind in the Baseline.
        from_kind: JoinKind,
        /// The join's kind in the current state.
        to_kind: JoinKind,
    },
    /// See [`RuleId::ColumnAdded`].
    ColumnAdded {
        /// The Node the column was added to.
        node: NodeId,
        /// The added column.
        column: ColumnName,
    },
}

impl FindingDetail {
    /// The Rule this detail belongs to.
    pub fn rule(&self) -> RuleId {
        match self {
            FindingDetail::ColumnRemovedWithActiveReferences { .. } => {
                RuleId::ColumnRemovedWithActiveReferences
            }
            FindingDetail::ColumnTypeNarrowed { .. } => RuleId::ColumnTypeNarrowed,
            FindingDetail::JoinCardinalityLoosened { .. } => RuleId::JoinCardinalityLoosened,
            FindingDetail::ColumnAdded { .. } => RuleId::ColumnAdded,
        }
    }
}

/// A single Rule's classification of a Change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// This Finding's Severity.
    pub severity: Severity,
    /// The specifics of what matched.
    pub detail: FindingDetail,
}

/// Evaluates every Rule against a Change list, using `baseline`'s Lineage
/// Edges where a Rule needs to know what was actively referenced before
/// the Change happened (a removed column's own Lineage Edge no longer
/// exists in the current state, so only the Baseline can answer that),
/// and `config` to resolve each Rule's configured Severity.
pub fn evaluate(baseline: &ParsedProject, changes: &[Change], config: &Config) -> Vec<Finding> {
    changes
        .iter()
        .flat_map(|change| evaluate_change(baseline, change, config))
        .collect()
}

fn evaluate_change(baseline: &ParsedProject, change: &Change, config: &Config) -> Vec<Finding> {
    match change {
        Change::ColumnRemoved { .. } => {
            column_removed_with_active_references(baseline, change, config)
        }
        Change::ColumnTypeChanged {
            node,
            column,
            from_type,
            to_type,
        } => column_type_narrowed(node, column, from_type, to_type, config)
            .into_iter()
            .collect(),
        Change::JoinChanged {
            node,
            position,
            from_kind,
            to_kind,
        } => join_cardinality_loosened(node, *position, *from_kind, *to_kind, config)
            .into_iter()
            .collect(),
        Change::ColumnAdded { node, column } => {
            vec![finding(
                config,
                FindingDetail::ColumnAdded {
                    node: node.clone(),
                    column: column.clone(),
                },
            )]
        }
    }
}

fn finding(config: &Config, detail: FindingDetail) -> Finding {
    Finding {
        severity: config.severity_for(detail.rule()),
        detail,
    }
}

fn column_removed_with_active_references(
    baseline: &ParsedProject,
    change: &Change,
    config: &Config,
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
            (&lineage.upstream_column == column).then(|| {
                finding(
                    config,
                    FindingDetail::ColumnRemovedWithActiveReferences {
                        node: node.clone(),
                        column: column.clone(),
                        reached: edge.downstream.clone(),
                        reached_column: lineage.downstream_column.clone(),
                    },
                )
            })
        })
        .collect()
}

/// The recognized integer types, from narrowest to widest. `int` and
/// `integer` are the same type under different names and share a rank.
const INTEGER_WIDTH_ORDER: &[&str] = &["tinyint", "smallint", "int", "bigint"];

fn normalize_integer_type_name(name: &str) -> Option<&'static str> {
    match name.trim().to_lowercase().as_str() {
        "tinyint" => Some("tinyint"),
        "smallint" => Some("smallint"),
        "int" | "integer" => Some("int"),
        "bigint" => Some("bigint"),
        _ => None,
    }
}

fn integer_width_rank(type_name: &str) -> Option<usize> {
    let normalized = normalize_integer_type_name(type_name)?;
    INTEGER_WIDTH_ORDER.iter().position(|t| *t == normalized)
}

fn column_type_narrowed(
    node: &NodeId,
    column: &ColumnName,
    from_type: &str,
    to_type: &str,
    config: &Config,
) -> Option<Finding> {
    let from_rank = integer_width_rank(from_type)?;
    let to_rank = integer_width_rank(to_type)?;
    (to_rank < from_rank).then(|| {
        finding(
            config,
            FindingDetail::ColumnTypeNarrowed {
                node: node.clone(),
                column: column.clone(),
                from_type: from_type.to_string(),
                to_type: to_type.to_string(),
            },
        )
    })
}

fn looseness_rank(kind: JoinKind) -> Option<u8> {
    match kind {
        JoinKind::Inner => Some(0),
        JoinKind::Left | JoinKind::Right => Some(1),
        JoinKind::Full => Some(2),
        JoinKind::Cross => None,
    }
}

fn join_cardinality_loosened(
    node: &NodeId,
    position: usize,
    from_kind: Option<JoinKind>,
    to_kind: Option<JoinKind>,
    config: &Config,
) -> Option<Finding> {
    let from_kind = from_kind?;
    let to_kind = to_kind?;
    let from_rank = looseness_rank(from_kind)?;
    let to_rank = looseness_rank(to_kind)?;
    (to_rank > from_rank).then(|| {
        finding(
            config,
            FindingDetail::JoinCardinalityLoosened {
                node: node.clone(),
                position,
                from_kind,
                to_kind,
            },
        )
    })
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

    fn empty_project() -> ParsedProject {
        ParsedProject {
            nodes: vec![],
            origins: vec![],
            edges: vec![],
        }
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

        assert_eq!(
            evaluate(&baseline, &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Error,
                detail: FindingDetail::ColumnRemovedWithActiveReferences {
                    node: node_id("model.a"),
                    column: column("x"),
                    reached: node_id("model.b"),
                    reached_column: column("x"),
                },
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

        assert_eq!(
            evaluate(&baseline, &changes, &Config::default()),
            Vec::new()
        );
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

        assert_eq!(
            evaluate(&baseline, &changes, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn column_type_narrowed_fires_on_a_recognized_narrowing() {
        let changes = vec![Change::ColumnTypeChanged {
            node: node_id("model.a"),
            column: column("x"),
            from_type: "bigint".to_string(),
            to_type: "int".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node: node_id("model.a"),
                    column: column("x"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            }]
        );
    }

    #[test]
    fn column_type_narrowed_does_not_fire_on_a_widening() {
        let changes = vec![Change::ColumnTypeChanged {
            node: node_id("model.a"),
            column: column("x"),
            from_type: "int".to_string(),
            to_type: "bigint".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn column_type_narrowed_does_not_fire_on_an_unrecognized_type_pair() {
        // Neither side is a recognized integer type -- never guess.
        let changes = vec![Change::ColumnTypeChanged {
            node: node_id("model.a"),
            column: column("x"),
            from_type: "varchar(255)".to_string(),
            to_type: "varchar(50)".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn join_cardinality_loosened_fires_on_inner_to_left() {
        let changes = vec![Change::JoinChanged {
            node: node_id("model.a"),
            position: 0,
            from_kind: Some(JoinKind::Inner),
            to_kind: Some(JoinKind::Left),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Warn,
                detail: FindingDetail::JoinCardinalityLoosened {
                    node: node_id("model.a"),
                    position: 0,
                    from_kind: JoinKind::Inner,
                    to_kind: JoinKind::Left,
                },
            }]
        );
    }

    #[test]
    fn join_cardinality_loosened_fires_on_inner_to_full() {
        let changes = vec![Change::JoinChanged {
            node: node_id("model.a"),
            position: 0,
            from_kind: Some(JoinKind::Inner),
            to_kind: Some(JoinKind::Full),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()).len(),
            1
        );
    }

    #[test]
    fn join_cardinality_loosened_does_not_fire_on_left_to_inner() {
        // Tightening, not loosening.
        let changes = vec![Change::JoinChanged {
            node: node_id("model.a"),
            position: 0,
            from_kind: Some(JoinKind::Left),
            to_kind: Some(JoinKind::Inner),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn join_cardinality_loosened_does_not_fire_when_a_join_was_added_or_removed() {
        let added = vec![Change::JoinChanged {
            node: node_id("model.a"),
            position: 0,
            from_kind: None,
            to_kind: Some(JoinKind::Inner),
        }];
        let removed = vec![Change::JoinChanged {
            node: node_id("model.a"),
            position: 0,
            from_kind: Some(JoinKind::Inner),
            to_kind: None,
        }];

        assert_eq!(
            evaluate(&empty_project(), &added, &Config::default()),
            Vec::new()
        );
        assert_eq!(
            evaluate(&empty_project(), &removed, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn column_added_fires_as_pass_severity() {
        let changes = vec![Change::ColumnAdded {
            node: node_id("model.a"),
            column: column("x"),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Pass,
                detail: FindingDetail::ColumnAdded {
                    node: node_id("model.a"),
                    column: column("x")
                },
            }]
        );
    }

    #[test]
    fn all_four_rules_fire_together_on_a_fixture_with_simultaneous_changes() {
        let baseline = ParsedProject {
            nodes: vec![Node {
                id: node_id("model.a"),
                name: "a".to_string(),
                columns: vec![],
                joins: vec![],
            }],
            origins: vec![],
            edges: vec![LineageEdge {
                upstream: Upstream::Node(node_id("model.a")),
                downstream: node_id("model.b"),
                column: Some(ColumnLineage {
                    upstream_column: column("removed_col"),
                    downstream_column: column("removed_col"),
                }),
            }],
        };
        let changes = vec![
            Change::ColumnRemoved {
                node: node_id("model.a"),
                column: column("removed_col"),
            },
            Change::ColumnTypeChanged {
                node: node_id("model.a"),
                column: column("id"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            },
            Change::JoinChanged {
                node: node_id("model.a"),
                position: 0,
                from_kind: Some(JoinKind::Inner),
                to_kind: Some(JoinKind::Left),
            },
            Change::ColumnAdded {
                node: node_id("model.a"),
                column: column("new_col"),
            },
        ];

        let findings = evaluate(&baseline, &changes, &Config::default());
        assert_eq!(findings.len(), 4);

        let rules: Vec<RuleId> = findings.iter().map(|f| f.detail.rule()).collect();
        assert!(rules.contains(&RuleId::ColumnRemovedWithActiveReferences));
        assert!(rules.contains(&RuleId::ColumnTypeNarrowed));
        assert!(rules.contains(&RuleId::JoinCardinalityLoosened));
        assert!(rules.contains(&RuleId::ColumnAdded));

        let severities: Vec<Severity> = findings.iter().map(|f| f.severity).collect();
        assert_eq!(
            severities.iter().filter(|s| **s == Severity::Error).count(),
            1
        );
        assert_eq!(
            severities.iter().filter(|s| **s == Severity::Warn).count(),
            2
        );
        assert_eq!(
            severities.iter().filter(|s| **s == Severity::Pass).count(),
            1
        );
    }
}
