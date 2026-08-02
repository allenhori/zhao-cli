//! Computing the set of [`Change`]s between two [`ParsedProject`] states
//! (a Baseline and a current state, both produced by the same
//! [`crate::adapters::TransformationToolAdapter`]).

use crate::model::{Column, ColumnName, JoinKind, Node, NodeId, ParsedProject};

/// A single detected difference between a Baseline and current version of
/// a [`Node`]'s schema or definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A column present in the current state that wasn't in the Baseline.
    ColumnAdded {
        /// The Node the column was added to.
        node: NodeId,
        /// The added column's name.
        column: ColumnName,
    },
    /// A column present in the Baseline that's no longer in the current
    /// state.
    ColumnRemoved {
        /// The Node the column was removed from.
        node: NodeId,
        /// The removed column's name.
        column: ColumnName,
    },
    /// A column present in both states, but whose documented type
    /// differs. Only produced when *both* states happen to document a
    /// type for that column -- see [`Column::data_type`]; zhao never
    /// infers a type itself.
    ColumnTypeChanged {
        /// The Node the column belongs to.
        node: NodeId,
        /// The column whose documented type changed.
        column: ColumnName,
        /// The type documented in the Baseline.
        from_type: String,
        /// The type documented in the current state.
        to_type: String,
    },
    /// The join at a given position in the Node's final `SELECT` changed
    /// kind, or a join was added/removed at that position (represented as
    /// `from_kind`/`to_kind` being `None`).
    JoinChanged {
        /// The Node whose definition's joins changed.
        node: NodeId,
        /// The join's position (0-indexed, in the order joins appear).
        position: usize,
        /// The join's kind in the Baseline, or `None` if this position
        /// didn't exist there (the join was added).
        from_kind: Option<JoinKind>,
        /// The join's kind in the current state, or `None` if this
        /// position no longer exists (the join was removed).
        to_kind: Option<JoinKind>,
    },
}

/// Computes the [`Change`]s between a Baseline and current [`ParsedProject`].
///
/// Only Nodes present in both states are compared -- a Node that's
/// entirely new, or entirely removed, doesn't produce a `Change` here.
/// "How did this Node change" presupposes the Node already existed;
/// whole-Node addition/removal is a coarser, separate question this
/// function doesn't answer.
pub fn diff(baseline: &ParsedProject, current: &ParsedProject) -> Vec<Change> {
    let mut changes = Vec::new();

    for current_node in &current.nodes {
        let Some(baseline_node) = baseline.node(&current_node.id) else {
            continue;
        };

        changes.extend(diff_columns(baseline_node, current_node));
        changes.extend(diff_joins(baseline_node, current_node));
    }

    changes
}

fn diff_columns(baseline: &Node, current: &Node) -> Vec<Change> {
    let mut changes = Vec::new();

    for current_col in &current.columns {
        match baseline.columns.iter().find(|c| c.name == current_col.name) {
            None => changes.push(Change::ColumnAdded {
                node: current.id.clone(),
                column: current_col.name.clone(),
            }),
            Some(baseline_col) => {
                changes.extend(column_type_change(&current.id, baseline_col, current_col))
            }
        }
    }

    for baseline_col in &baseline.columns {
        if !current.columns.iter().any(|c| c.name == baseline_col.name) {
            changes.push(Change::ColumnRemoved {
                node: current.id.clone(),
                column: baseline_col.name.clone(),
            });
        }
    }

    changes
}

fn column_type_change(node: &NodeId, baseline: &Column, current: &Column) -> Option<Change> {
    let from_type = baseline.data_type.as_ref()?;
    let to_type = current.data_type.as_ref()?;
    if from_type == to_type {
        return None;
    }
    Some(Change::ColumnTypeChanged {
        node: node.clone(),
        column: current.name.clone(),
        from_type: from_type.clone(),
        to_type: to_type.clone(),
    })
}

fn diff_joins(baseline: &Node, current: &Node) -> Vec<Change> {
    let longest = baseline.joins.len().max(current.joins.len());
    (0..longest)
        .filter_map(|position| {
            let from_kind = baseline.joins.get(position).copied();
            let to_kind = current.joins.get(position).copied();
            (from_kind != to_kind).then_some(Change::JoinChanged {
                node: current.id.clone(),
                position,
                from_kind,
                to_kind,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, columns: Vec<Column>, joins: Vec<JoinKind>) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.to_string(),
            columns,
            joins,
        }
    }

    fn column(name: &str, data_type: Option<&str>) -> Column {
        Column {
            name: ColumnName::new(name),
            data_type: data_type.map(str::to_string),
        }
    }

    fn project(nodes: Vec<Node>) -> ParsedProject {
        ParsedProject {
            nodes,
            origins: Vec::new(),
            edges: Vec::new(),
        }
    }

    #[test]
    fn identical_nodes_produce_no_changes() {
        let n = node(
            "model.a",
            vec![column("x", Some("int"))],
            vec![JoinKind::Inner],
        );
        let baseline = project(vec![n.clone()]);
        let current = project(vec![n]);

        assert_eq!(diff(&baseline, &current), Vec::new());
    }

    #[test]
    fn detects_column_added_and_removed() {
        let baseline = project(vec![node("model.a", vec![column("old", None)], vec![])]);
        let current = project(vec![node("model.a", vec![column("new", None)], vec![])]);

        let changes = diff(&baseline, &current);
        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&Change::ColumnAdded {
            node: NodeId::new("model.a"),
            column: ColumnName::new("new")
        }));
        assert!(changes.contains(&Change::ColumnRemoved {
            node: NodeId::new("model.a"),
            column: ColumnName::new("old")
        }));
    }

    #[test]
    fn detects_a_documented_type_change_but_not_when_only_one_side_documents_it() {
        let baseline = project(vec![node(
            "model.a",
            vec![column("x", Some("bigint"))],
            vec![],
        )]);
        let current = project(vec![node(
            "model.a",
            vec![column("x", Some("int"))],
            vec![],
        )]);
        assert_eq!(
            diff(&baseline, &current),
            vec![Change::ColumnTypeChanged {
                node: NodeId::new("model.a"),
                column: ColumnName::new("x"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            }]
        );

        // Only the current side documents a type -- nothing to compare
        // against, so no Change, not a guessed one.
        let baseline_undocumented = project(vec![node("model.a", vec![column("x", None)], vec![])]);
        assert_eq!(diff(&baseline_undocumented, &current), Vec::new());
    }

    #[test]
    fn detects_a_join_kind_change_at_the_same_position() {
        let baseline = project(vec![node("model.a", vec![], vec![JoinKind::Left])]);
        let current = project(vec![node("model.a", vec![], vec![JoinKind::Inner])]);

        assert_eq!(
            diff(&baseline, &current),
            vec![Change::JoinChanged {
                node: NodeId::new("model.a"),
                position: 0,
                from_kind: Some(JoinKind::Left),
                to_kind: Some(JoinKind::Inner),
            }]
        );
    }

    #[test]
    fn detects_a_join_added() {
        let baseline = project(vec![node("model.a", vec![], vec![])]);
        let current = project(vec![node("model.a", vec![], vec![JoinKind::Inner])]);

        assert_eq!(
            diff(&baseline, &current),
            vec![Change::JoinChanged {
                node: NodeId::new("model.a"),
                position: 0,
                from_kind: None,
                to_kind: Some(JoinKind::Inner)
            }]
        );
    }

    #[test]
    fn a_node_present_only_in_current_produces_no_change() {
        let baseline = project(vec![]);
        let current = project(vec![node("model.new", vec![column("x", None)], vec![])]);

        assert_eq!(diff(&baseline, &current), Vec::new());
    }

    #[test]
    fn origins_and_edges_are_irrelevant_to_the_diff() {
        // Sanity check that diff() only looks at nodes -- constructing a
        // ParsedProject with populated origins/edges shouldn't panic or
        // affect the result for otherwise-identical nodes.
        use crate::model::{Origin, OriginId};
        let n = node("model.a", vec![], vec![]);
        let mut baseline = project(vec![n.clone()]);
        baseline.origins.push(Origin {
            id: OriginId::new("source.x"),
            name: "x".to_string(),
        });
        let current = project(vec![n]);

        assert_eq!(diff(&baseline, &current), Vec::new());
    }
}
