//! Computing the set of [`Change`]s between two [`ParsedProject`] states
//! (a Baseline and a current state, both produced by the same
//! [`crate::adapters::TransformationToolAdapter`]).
//!
//! ## Known limitations
//!
//! Join comparison aligns the Baseline's and current state's join-kind
//! sequences via their longest common subsequence rather than comparing
//! index-by-index, so that a join inserted or removed in the middle of a
//! sequence is reported as
//! exactly that, not a cascade of unrelated-looking kind changes in every
//! join after it. The one honest caveat: when the same [`JoinKind`]
//! appears more than once in a Node's definition, this alignment can pair
//! a "kept" occurrence with a different occurrence than a human reading
//! the SQL would assume -- joins are only ever compared by kind here,
//! never by what they actually join on.

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

/// A single step in an edit script transforming one join-kind sequence
/// into another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinEditOp {
    /// This join is present, unchanged, in both sequences.
    Keep(JoinKind),
    /// This join is only in the Baseline.
    Remove(JoinKind),
    /// This join is only in the current state.
    Add(JoinKind),
}

/// Compares two join-kind sequences positionally is unsound: inserting or
/// removing a join anywhere but the end shifts every later join's index,
/// which a naive index-by-index comparison reports as that many
/// independent kind changes -- wrong, not just imprecise (see the
/// module-level doc comment's rationale for "no Change is better than a
/// wrong one"). This aligns the two sequences via their longest common
/// subsequence first, so an insertion/removal in the middle is reported
/// as exactly that, not a cascade of unrelated-looking changes.
///
/// One honest caveat: when the same [`JoinKind`] appears more than once
/// in a sequence, LCS alignment may pair a "kept" occurrence with a
/// different occurrence than a human skimming the SQL would assume --
/// the joins are structurally interchangeable from this function's point
/// of view, since it only ever sees their kind, never what they join on.
fn join_edit_script(baseline: &[JoinKind], current: &[JoinKind]) -> Vec<JoinEditOp> {
    let (m, n) = (baseline.len(), current.len());
    let mut lcs_length = vec![vec![0usize; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            lcs_length[i][j] = if baseline[i - 1] == current[j - 1] {
                lcs_length[i - 1][j - 1] + 1
            } else {
                lcs_length[i - 1][j].max(lcs_length[i][j - 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && baseline[i - 1] == current[j - 1] {
            ops.push(JoinEditOp::Keep(baseline[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || lcs_length[i][j - 1] >= lcs_length[i - 1][j]) {
            ops.push(JoinEditOp::Add(current[j - 1]));
            j -= 1;
        } else {
            ops.push(JoinEditOp::Remove(baseline[i - 1]));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

fn diff_joins(baseline: &Node, current: &Node) -> Vec<Change> {
    let ops = join_edit_script(&baseline.joins, &current.joins);

    let mut changes = Vec::new();
    let mut position = 0;
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            JoinEditOp::Keep(_) => {
                position += 1;
                i += 1;
            }
            JoinEditOp::Remove(from_kind) => {
                // A removal immediately followed by an addition is a kind
                // change at the same position, not two independent edits.
                if let Some(JoinEditOp::Add(to_kind)) = ops.get(i + 1) {
                    changes.push(Change::JoinChanged {
                        node: current.id.clone(),
                        position,
                        from_kind: Some(from_kind),
                        to_kind: Some(*to_kind),
                    });
                    position += 1;
                    i += 2;
                } else {
                    changes.push(Change::JoinChanged {
                        node: current.id.clone(),
                        position,
                        from_kind: Some(from_kind),
                        to_kind: None,
                    });
                    i += 1;
                }
            }
            JoinEditOp::Add(to_kind) => {
                changes.push(Change::JoinChanged {
                    node: current.id.clone(),
                    position,
                    from_kind: None,
                    to_kind: Some(to_kind),
                });
                position += 1;
                i += 1;
            }
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Materialization;

    fn node(id: &str, columns: Vec<Column>, joins: Vec<JoinKind>) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.to_string(),
            columns,
            joins,
            materialization: Materialization::Table,
        }
    }

    fn column(name: &str, data_type: Option<&str>) -> Column {
        Column {
            name: ColumnName::new(name),
            data_type: data_type.map(str::to_string),
            expression: None,
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

    /// Regression test: a join inserted in the *middle* of a sequence
    /// (not at the end) must be reported as one addition, not as a false
    /// kind-change cascading through every join that shifted position
    /// after it. Naive index-by-index comparison gets this wrong.
    #[test]
    fn a_join_inserted_in_the_middle_reports_one_addition_not_a_cascade() {
        let baseline = project(vec![node(
            "model.a",
            vec![],
            vec![JoinKind::Inner, JoinKind::Left],
        )]);
        let current = project(vec![node(
            "model.a",
            vec![],
            vec![JoinKind::Cross, JoinKind::Inner, JoinKind::Left],
        )]);

        assert_eq!(
            diff(&baseline, &current),
            vec![Change::JoinChanged {
                node: NodeId::new("model.a"),
                position: 0,
                from_kind: None,
                to_kind: Some(JoinKind::Cross),
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
