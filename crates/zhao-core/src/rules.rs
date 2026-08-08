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
//!   not firing at all. The same narrow integer-width comparison, and the
//!   same "never guess" refusal on anything else, is reused verbatim for
//!   [`RuleId::StructFieldTypeNarrowed`] one level deeper.
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
    /// A field was removed from a `STRUCT`-typed column's internal shape,
    /// where that shape was statically knowable in both the Baseline and
    /// current state (see [`crate::model::Column::struct_fields`]). The
    /// same category of breaking change as
    /// [`RuleId::ColumnRemovedWithActiveReferences`] one level deeper --
    /// a `STRUCT`'s field list is as real a contract as a Node's own
    /// column list -- but without that Rule's own column-lineage-based
    /// "was it actively referenced" check: nested-field lineage isn't
    /// tracked at that granularity (a nested field access always
    /// collapses to its base column, see the dbt adapter's module-level
    /// "Known limitations" doc comment), so this fires unconditionally on
    /// every detected removal rather than only a referenced one. `error`
    /// by default.
    StructFieldRemoved,
    /// A field was added to a `STRUCT`-typed column's internal shape.
    /// `error` by default -- deliberately *not* the same tier as
    /// [`RuleId::ColumnAdded`] (`pass`) one level up, even though neither
    /// can break a *reference* to something that didn't exist before:
    /// unlike a new top-level column (which most warehouses accept via
    /// plain schema evolution), adding a field to an existing `STRUCT`
    /// column breaks an incremental `MERGE`/`INSERT` on an
    /// already-materialized table without manual intervention on at
    /// least Databricks/Delta Lake -- confirmed against a real Databricks
    /// workspace, not just documentation: `[DELTA_UPDATE_SCHEMA_MISMATCH_EXPRESSION]
    /// Cannot cast struct<a:string,b:string> to struct<a:string>. All
    /// nested columns must match.` A team that's confirmed their own
    /// warehouse/materialization combination tolerates this can still
    /// relax it to `warn`/`pass` via `zhao.yml`.
    StructFieldAdded,
    /// A field within a `STRUCT`-typed column's internal shape narrowed
    /// its documented type (the same narrow, integer-width-only
    /// comparison [`RuleId::ColumnTypeNarrowed`] uses -- see the
    /// module-level "Known limitations" doc comment). `error` by
    /// default -- deliberately stricter than [`RuleId::ColumnTypeNarrowed`]'s
    /// `warn`: struct-internal changes are easy to miss in a PR diff
    /// (nested inside a column, not a new/removed top-level column), and
    /// the same schema-mismatch risk [`RuleId::StructFieldAdded`]'s doc
    /// comment describes applies here too. Relax to `warn`/`pass` via
    /// `zhao.yml` if a team has confirmed it's safe for their own setup.
    StructFieldTypeNarrowed,
}

impl RuleId {
    /// This Rule's Severity by default, before any `zhao.yml` override.
    pub fn default_severity(self) -> Severity {
        match self {
            RuleId::ColumnRemovedWithActiveReferences => Severity::Error,
            RuleId::ColumnTypeNarrowed => Severity::Warn,
            RuleId::JoinCardinalityLoosened => Severity::Warn,
            RuleId::ColumnAdded => Severity::Pass,
            RuleId::StructFieldRemoved => Severity::Error,
            RuleId::StructFieldAdded => Severity::Error,
            RuleId::StructFieldTypeNarrowed => Severity::Error,
        }
    }

    /// This Rule's canonical name in `zhao.yml` configuration.
    pub fn config_name(self) -> &'static str {
        match self {
            RuleId::ColumnRemovedWithActiveReferences => "column-removed-with-active-references",
            RuleId::ColumnTypeNarrowed => "column-type-narrowed",
            RuleId::JoinCardinalityLoosened => "join-cardinality-loosened",
            RuleId::ColumnAdded => "column-added",
            RuleId::StructFieldRemoved => "struct-field-removed",
            RuleId::StructFieldAdded => "struct-field-added",
            RuleId::StructFieldTypeNarrowed => "struct-field-type-narrowed",
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
            "struct-field-removed" => Some(RuleId::StructFieldRemoved),
            "struct-field-added" => Some(RuleId::StructFieldAdded),
            "struct-field-type-narrowed" => Some(RuleId::StructFieldTypeNarrowed),
            _ => None,
        }
    }

    /// Every Rule in the v1 catalog, in declaration order. Exists so
    /// callers (e.g. an "unknown rule name" error message) can list valid
    /// names without duplicating the catalog themselves.
    pub fn all() -> [RuleId; 7] {
        [
            RuleId::ColumnRemovedWithActiveReferences,
            RuleId::ColumnTypeNarrowed,
            RuleId::JoinCardinalityLoosened,
            RuleId::ColumnAdded,
            RuleId::StructFieldRemoved,
            RuleId::StructFieldAdded,
            RuleId::StructFieldTypeNarrowed,
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
    /// See [`RuleId::StructFieldRemoved`].
    StructFieldRemoved {
        /// The Node the column belongs to.
        node: NodeId,
        /// The struct-typed column the field was removed from.
        column: ColumnName,
        /// The removed field.
        field: ColumnName,
    },
    /// See [`RuleId::StructFieldAdded`].
    StructFieldAdded {
        /// The Node the column belongs to.
        node: NodeId,
        /// The struct-typed column the field was added to.
        column: ColumnName,
        /// The added field.
        field: ColumnName,
    },
    /// See [`RuleId::StructFieldTypeNarrowed`].
    StructFieldTypeNarrowed {
        /// The Node the column belongs to.
        node: NodeId,
        /// The struct-typed column the field belongs to.
        column: ColumnName,
        /// The field whose documented type narrowed.
        field: ColumnName,
        /// The field's documented type in the Baseline.
        from_type: String,
        /// The field's documented type in the current state.
        to_type: String,
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
            FindingDetail::StructFieldRemoved { .. } => RuleId::StructFieldRemoved,
            FindingDetail::StructFieldAdded { .. } => RuleId::StructFieldAdded,
            FindingDetail::StructFieldTypeNarrowed { .. } => RuleId::StructFieldTypeNarrowed,
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
        Change::StructFieldRemoved {
            node,
            column,
            field,
        } => vec![finding(
            config,
            FindingDetail::StructFieldRemoved {
                node: node.clone(),
                column: column.clone(),
                field: field.clone(),
            },
        )],
        Change::StructFieldAdded {
            node,
            column,
            field,
        } => vec![finding(
            config,
            FindingDetail::StructFieldAdded {
                node: node.clone(),
                column: column.clone(),
                field: field.clone(),
            },
        )],
        Change::StructFieldTypeChanged {
            node,
            column,
            field,
            from_type,
            to_type,
        } => struct_field_type_narrowed(node, column, field, from_type, to_type, config)
            .into_iter()
            .collect(),
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

/// The struct-field counterpart to [`column_type_narrowed`], one level
/// deeper: fires under exactly the same "recognized integer type,
/// genuinely narrower" condition, reusing [`integer_width_rank`] verbatim
/// rather than a parallel comparison that could drift out of sync with
/// it. See [`RuleId::StructFieldTypeNarrowed`] for why this doesn't (and
/// can't) additionally check for active downstream references the way
/// [`column_removed_with_active_references`] does.
fn struct_field_type_narrowed(
    node: &NodeId,
    column: &ColumnName,
    field: &ColumnName,
    from_type: &str,
    to_type: &str,
    config: &Config,
) -> Option<Finding> {
    let from_rank = integer_width_rank(from_type)?;
    let to_rank = integer_width_rank(to_type)?;
    (to_rank < from_rank).then(|| {
        finding(
            config,
            FindingDetail::StructFieldTypeNarrowed {
                node: node.clone(),
                column: column.clone(),
                field: field.clone(),
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
    use crate::model::{ColumnLineage, LineageEdge, Materialization, Node};

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
                    materialization: Materialization::Table,
                },
                Node {
                    id: node_id("model.b"),
                    name: "b".to_string(),
                    columns: vec![],
                    joins: vec![],
                    materialization: Materialization::Table,
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
                materialization: Materialization::Table,
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
                materialization: Materialization::Table,
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
                materialization: Materialization::Table,
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

    // -----------------------------------------------------------------
    // Struct-internal field evolution (issue #53).
    // -----------------------------------------------------------------

    /// Acceptance criterion (a): a struct field being removed is detected
    /// as breaking (`error` severity), unconditionally -- unlike
    /// `column-removed-with-active-references`, this doesn't (and can't)
    /// gate on an active downstream reference, since nested-field lineage
    /// isn't tracked at that granularity. See
    /// [`RuleId::StructFieldRemoved`]'s doc comment.
    #[test]
    fn struct_field_removed_fires_as_error_severity() {
        let changes = vec![Change::StructFieldRemoved {
            node: node_id("model.a"),
            column: column("payload"),
            field: column("legacy_flag"),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Error,
                detail: FindingDetail::StructFieldRemoved {
                    node: node_id("model.a"),
                    column: column("payload"),
                    field: column("legacy_flag"),
                },
            }]
        );
    }

    /// Acceptance criterion (b): a struct field being added fires at
    /// `error`, deliberately stricter than `column-added`'s `pass` --
    /// confirmed against a real Databricks workspace that this genuinely
    /// breaks an incremental `MERGE` without manual intervention, unlike
    /// a new top-level column (see [`RuleId::StructFieldAdded`]'s doc
    /// comment).
    #[test]
    fn struct_field_added_fires_as_error_severity() {
        let changes = vec![Change::StructFieldAdded {
            node: node_id("model.a"),
            column: column("payload"),
            field: column("email"),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Error,
                detail: FindingDetail::StructFieldAdded {
                    node: node_id("model.a"),
                    column: column("payload"),
                    field: column("email"),
                },
            }]
        );
    }

    /// `error` by default -- deliberately stricter than
    /// `column-type-narrowed`'s `warn`, since struct-internal changes are
    /// easy to miss in a PR diff (see [`RuleId::StructFieldTypeNarrowed`]'s
    /// doc comment).
    #[test]
    fn struct_field_type_narrowed_fires_on_a_recognized_narrowing() {
        let changes = vec![Change::StructFieldTypeChanged {
            node: node_id("model.a"),
            column: column("payload"),
            field: column("amount"),
            from_type: "bigint".to_string(),
            to_type: "int".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            vec![Finding {
                severity: Severity::Error,
                detail: FindingDetail::StructFieldTypeNarrowed {
                    node: node_id("model.a"),
                    column: column("payload"),
                    field: column("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            }]
        );
    }

    #[test]
    fn struct_field_type_narrowed_does_not_fire_on_a_widening() {
        let changes = vec![Change::StructFieldTypeChanged {
            node: node_id("model.a"),
            column: column("payload"),
            field: column("amount"),
            from_type: "int".to_string(),
            to_type: "bigint".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            Vec::new()
        );
    }

    #[test]
    fn struct_field_type_narrowed_does_not_fire_on_an_unrecognized_type_pair() {
        // Neither side is a recognized integer type -- never guess, same
        // as `column_type_narrowed_does_not_fire_on_an_unrecognized_type_pair`.
        let changes = vec![Change::StructFieldTypeChanged {
            node: node_id("model.a"),
            column: column("payload"),
            field: column("note"),
            from_type: "varchar(255)".to_string(),
            to_type: "varchar(50)".to_string(),
        }];

        assert_eq!(
            evaluate(&empty_project(), &changes, &Config::default()),
            Vec::new()
        );
    }

    /// Acceptance criterion (c): when a column's struct shape isn't
    /// statically knowable on both sides, `diff()` (see `diff.rs`'s own
    /// tests) never produces a `StructField*` `Change` for it in the
    /// first place -- so there is, by construction, nothing for this
    /// module to evaluate and no Rule can fire. This test pins that
    /// contract from the Rule-evaluation side: an empty Change list
    /// (exactly what an "unknown shape" comparison produces) yields zero
    /// struct-evolution Findings, not a guessed one.
    #[test]
    fn no_struct_evolution_finding_fires_when_there_is_no_struct_field_change_to_evaluate() {
        let findings = evaluate(&empty_project(), &[], &Config::default());
        assert!(findings.is_empty());
    }

    /// `zhao.yml` can name every new Rule by its documented config name,
    /// round-tripping through `RuleId::all()` the same way the original
    /// four Rules already do -- guards against a typo in `config_name`/
    /// `from_config_name` silently drifting apart.
    #[test]
    fn every_struct_evolution_rule_config_name_round_trips() {
        for rule in [
            RuleId::StructFieldRemoved,
            RuleId::StructFieldAdded,
            RuleId::StructFieldTypeNarrowed,
        ] {
            assert_eq!(RuleId::from_config_name(rule.config_name()), Some(rule));
            assert!(RuleId::all().contains(&rule));
        }
    }

    /// All three struct-evolution Rules can fire together on a fixture
    /// with simultaneous changes, exactly mirroring
    /// `all_four_rules_fire_together_on_a_fixture_with_simultaneous_changes`
    /// above for the original top-level-column Rules.
    #[test]
    fn all_three_struct_evolution_rules_fire_together_on_a_fixture_with_simultaneous_changes() {
        let changes = vec![
            Change::StructFieldRemoved {
                node: node_id("model.a"),
                column: column("payload"),
                field: column("legacy_flag"),
            },
            Change::StructFieldAdded {
                node: node_id("model.a"),
                column: column("payload"),
                field: column("email"),
            },
            Change::StructFieldTypeChanged {
                node: node_id("model.a"),
                column: column("payload"),
                field: column("amount"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            },
        ];

        let findings = evaluate(&empty_project(), &changes, &Config::default());
        assert_eq!(findings.len(), 3);

        let rules: Vec<RuleId> = findings.iter().map(|f| f.detail.rule()).collect();
        assert!(rules.contains(&RuleId::StructFieldRemoved));
        assert!(rules.contains(&RuleId::StructFieldAdded));
        assert!(rules.contains(&RuleId::StructFieldTypeNarrowed));

        // All three fire at `error` by default -- struct-internal changes
        // are stricter across the board than their top-level counterparts
        // (see each `RuleId` variant's own doc comment for why).
        let severities: Vec<Severity> = findings.iter().map(|f| f.severity).collect();
        assert!(severities.iter().all(|s| *s == Severity::Error));
    }
}
