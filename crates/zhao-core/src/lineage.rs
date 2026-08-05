//! Model-level lineage queries over a [`ParsedProject`]'s existing
//! `LineageEdge`s: what's upstream/downstream of a given Node, by full
//! transitive closure -- matching the semantics dbt's own `+`/`+` selector
//! syntax has, since that's the syntax [`Direction`] borrows.
//!
//! Column-level lineage (a later capability) is out of scope here; this
//! module only ever answers at the whole-Node level.

use std::collections::{HashSet, VecDeque};

use crate::model::{NodeId, OriginId, ParsedProject, Upstream};

/// Which direction(s) of a lineage traversal to include -- a bare target
/// is [`Direction::Both`]; `+target`/`target+` narrow to one side, same
/// as dbt's own selector syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Only ancestors (`+target`).
    Upstream,
    /// Only descendants (`target+`).
    Downstream,
    /// Both -- the default for a bare target.
    Both,
}

/// Everything that can go wrong resolving a lineage query.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LineageError {
    /// `target_name` doesn't match any Node's name in the project.
    #[error("no model named {name:?} was found in the project")]
    UnknownTarget {
        /// The name that couldn't be resolved.
        name: String,
    },
    /// `target_name` matches more than one Node's bare name -- e.g. two
    /// same-named models in different dbt packages. Rather than silently
    /// picking whichever one happened to appear first in the project's
    /// Node list (an arbitrary, undocumented tiebreak a user could never
    /// predict), this is treated the same as an unresolvable target:
    /// the caller must disambiguate, e.g. by package-qualifying the name.
    #[error(
        "{name:?} matches more than one model in the project ({ids:?}) -- disambiguate by \
         package"
    )]
    AmbiguousTarget {
        /// The name that matched more than one Node.
        name: String,
        /// Every Node's full ID the name matched, for the user to choose
        /// from.
        ids: Vec<String>,
    },
}

/// The full transitive closure of upstream/downstream Nodes (and
/// upstream Origins, since an ancestor chain can terminate at one) of a
/// query's target -- never includes the target itself. Whichever side
/// [`Direction`] excluded is simply empty, not absent -- callers don't
/// need to distinguish "excluded" from "genuinely has none" beyond that.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LineageResult {
    /// Every Node upstream of the target, in first-reached (breadth-first)
    /// order.
    pub upstream_nodes: Vec<NodeId>,
    /// Every Origin upstream of the target -- a Node's ancestor chain
    /// terminates at an Origin (something zhao doesn't build), so these
    /// are genuinely part of "what's upstream," not an afterthought.
    pub upstream_origins: Vec<OriginId>,
    /// Every Node downstream of the target, in first-reached
    /// (breadth-first) order.
    pub downstream_nodes: Vec<NodeId>,
}

/// Resolves a lineage query: finds the Node named `target_name` in
/// `project`, then walks `direction`'s side(s) of its Lineage Edges to
/// their full transitive closure.
pub fn trace(
    project: &ParsedProject,
    target_name: &str,
    direction: Direction,
) -> Result<LineageResult, LineageError> {
    let matches: Vec<&crate::model::Node> = project
        .nodes
        .iter()
        .filter(|node| node.name == target_name)
        .collect();
    let target = match matches.as_slice() {
        [] => {
            return Err(LineageError::UnknownTarget {
                name: target_name.to_string(),
            });
        }
        [only] => *only,
        multiple => {
            return Err(LineageError::AmbiguousTarget {
                name: target_name.to_string(),
                ids: multiple.iter().map(|node| node.id.to_string()).collect(),
            });
        }
    };

    let mut result = LineageResult::default();
    if matches!(direction, Direction::Upstream | Direction::Both) {
        let (nodes, origins) = walk_upstream(project, &target.id);
        result.upstream_nodes = nodes;
        result.upstream_origins = origins;
    }
    if matches!(direction, Direction::Downstream | Direction::Both) {
        result.downstream_nodes = walk_downstream(project, &target.id);
    }
    Ok(result)
}

/// Walks every edge transitively upstream of `start`, breadth-first,
/// stopping at each branch's Origins (which have no further upstream of
/// their own). A `visited` set (seeded with `start` itself, so it's
/// never mistakenly included in its own result) guards against
/// re-visiting a Node reached via more than one path -- necessary for
/// correctness on any DAG wider than a single chain, not just a defense
/// against a hypothetical cycle.
fn walk_upstream(project: &ParsedProject, start: &NodeId) -> (Vec<NodeId>, Vec<OriginId>) {
    let mut visited_nodes: HashSet<NodeId> = HashSet::from([start.clone()]);
    let mut visited_origins: HashSet<OriginId> = HashSet::new();
    let mut nodes = Vec::new();
    let mut origins = Vec::new();
    let mut frontier: VecDeque<NodeId> = VecDeque::from([start.clone()]);

    while let Some(current) = frontier.pop_front() {
        for edge in &project.edges {
            if edge.downstream != current {
                continue;
            }
            match &edge.upstream {
                Upstream::Node(id) => {
                    if visited_nodes.insert(id.clone()) {
                        nodes.push(id.clone());
                        frontier.push_back(id.clone());
                    }
                }
                Upstream::Origin(id) => {
                    if visited_origins.insert(id.clone()) {
                        origins.push(id.clone());
                    }
                }
            }
        }
    }

    (nodes, origins)
}

/// The downstream mirror of [`walk_upstream`] -- Origins never appear
/// here, since nothing is ever downstream of something zhao doesn't
/// build.
fn walk_downstream(project: &ParsedProject, start: &NodeId) -> Vec<NodeId> {
    let mut visited: HashSet<NodeId> = HashSet::from([start.clone()]);
    let mut nodes = Vec::new();
    let mut frontier: VecDeque<NodeId> = VecDeque::from([start.clone()]);

    while let Some(current) = frontier.pop_front() {
        for edge in &project.edges {
            let Upstream::Node(upstream_id) = &edge.upstream else {
                continue;
            };
            if upstream_id != &current {
                continue;
            }
            if visited.insert(edge.downstream.clone()) {
                nodes.push(edge.downstream.clone());
                frontier.push_back(edge.downstream.clone());
            }
        }
    }

    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LineageEdge, Materialization, Node, Origin};

    fn node(id: &str) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.rsplit('.').next().unwrap_or(id).to_string(),
            columns: Vec::new(),
            joins: Vec::new(),
            materialization: Materialization::Table,
        }
    }

    fn origin(id: &str) -> Origin {
        Origin {
            id: OriginId::new(id),
            name: id.rsplit('.').next().unwrap_or(id).to_string(),
        }
    }

    fn node_edge(upstream: &str, downstream: &str) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Node(NodeId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: None,
        }
    }

    fn origin_edge(upstream: &str, downstream: &str) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Origin(OriginId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: None,
        }
    }

    /// A diamond: origin.raw -> a -> {b, c} -> d, so both the multi-path
    /// (b and c both reach d) and the Origin-termination cases are
    /// exercised together.
    fn diamond_project() -> ParsedProject {
        ParsedProject {
            nodes: vec![
                node("model.p.a"),
                node("model.p.b"),
                node("model.p.c"),
                node("model.p.d"),
            ],
            origins: vec![origin("source.p.raw")],
            edges: vec![
                origin_edge("source.p.raw", "model.p.a"),
                node_edge("model.p.a", "model.p.b"),
                node_edge("model.p.a", "model.p.c"),
                node_edge("model.p.b", "model.p.d"),
                node_edge("model.p.c", "model.p.d"),
            ],
        }
    }

    #[test]
    fn bare_target_returns_both_directions() {
        let project = diamond_project();
        let result = trace(&project, "b", Direction::Both).expect("b should exist");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
        assert_eq!(result.upstream_origins, vec![OriginId::new("source.p.raw")]);
        assert_eq!(result.downstream_nodes, vec![NodeId::new("model.p.d")]);
    }

    #[test]
    fn upstream_direction_excludes_downstream() {
        let project = diamond_project();
        let result = trace(&project, "b", Direction::Upstream).expect("b should exist");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
        assert!(result.downstream_nodes.is_empty());
    }

    #[test]
    fn downstream_direction_excludes_upstream() {
        let project = diamond_project();
        let result = trace(&project, "b", Direction::Downstream).expect("b should exist");

        assert!(result.upstream_nodes.is_empty());
        assert!(result.upstream_origins.is_empty());
        assert_eq!(result.downstream_nodes, vec![NodeId::new("model.p.d")]);
    }

    /// `d` is reached from `a` via two separate paths (through `b` and
    /// through `c`) -- it must appear exactly once, not twice, and `a`
    /// itself (reached transitively through both `b` and `c`) must also
    /// be deduplicated to one entry.
    #[test]
    fn diamond_paths_are_deduplicated() {
        let project = diamond_project();
        let result = trace(&project, "a", Direction::Downstream).expect("a should exist");

        let mut downstream: Vec<String> = result
            .downstream_nodes
            .iter()
            .map(|id| id.to_string())
            .collect();
        downstream.sort();
        assert_eq!(
            downstream,
            vec![
                "model.p.b".to_string(),
                "model.p.c".to_string(),
                "model.p.d".to_string()
            ]
        );
    }

    #[test]
    fn unknown_target_produces_a_clear_error() {
        let project = diamond_project();
        let result = trace(&project, "does_not_exist", Direction::Both);

        assert_eq!(
            result,
            Err(LineageError::UnknownTarget {
                name: "does_not_exist".to_string()
            })
        );
    }

    #[test]
    fn a_node_with_no_connections_returns_an_empty_but_ok_result() {
        let mut project = diamond_project();
        project.nodes.push(node("model.p.isolated"));
        let result = trace(&project, "isolated", Direction::Both).expect("isolated should exist");

        assert_eq!(result, LineageResult::default());
    }

    /// Two Nodes in different packages sharing the same bare name must
    /// produce a clear, actionable error -- never a silent, arbitrary
    /// pick of whichever one happens to appear first in the project's
    /// Node list.
    #[test]
    fn a_name_matching_more_than_one_node_produces_a_clear_error() {
        let mut project = diamond_project();
        // A same-named model in a different package.
        project.nodes.push(node("model.other_package.a"));

        let result = trace(&project, "a", Direction::Both);

        assert_eq!(
            result,
            Err(LineageError::AmbiguousTarget {
                name: "a".to_string(),
                ids: vec!["model.p.a".to_string(), "model.other_package.a".to_string()],
            })
        );
    }

    /// Direct evidence of true breadth-first order (not just "some
    /// deterministic order that happens to work"): on an asymmetric-depth
    /// graph (`a -> b, a -> c, b -> d, c -> e, d -> f`), BFS visits
    /// `[b, c, d, e, f]` -- `e` (2 hops via the shorter `c` branch) before
    /// `f` (3 hops via the longer `b -> d` branch) is exactly the
    /// distinction a depth-first/stack-based walk would get wrong.
    #[test]
    fn downstream_order_is_genuinely_breadth_first() {
        let project = ParsedProject {
            nodes: vec![
                node("model.p.a"),
                node("model.p.b"),
                node("model.p.c"),
                node("model.p.d"),
                node("model.p.e"),
                node("model.p.f"),
            ],
            origins: Vec::new(),
            edges: vec![
                node_edge("model.p.a", "model.p.b"),
                node_edge("model.p.a", "model.p.c"),
                node_edge("model.p.b", "model.p.d"),
                node_edge("model.p.c", "model.p.e"),
                node_edge("model.p.d", "model.p.f"),
            ],
        };

        let result = trace(&project, "a", Direction::Downstream).expect("a should exist");

        assert_eq!(
            result.downstream_nodes,
            vec![
                NodeId::new("model.p.b"),
                NodeId::new("model.p.c"),
                NodeId::new("model.p.d"),
                NodeId::new("model.p.e"),
                NodeId::new("model.p.f"),
            ]
        );
    }
}
