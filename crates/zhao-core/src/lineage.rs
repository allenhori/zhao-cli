//! Model-level and column-level lineage queries over a [`ParsedProject`]'s
//! existing `LineageEdge`s: what's upstream/downstream of a given Node (or
//! a specific column on it), by full transitive closure -- matching the
//! semantics dbt's own `+`/`+` selector syntax has, since that's the
//! syntax [`Direction`] borrows.

use std::collections::{HashSet, VecDeque};

use crate::model::{ColumnName, Node, NodeId, OriginId, ParsedProject, Upstream};

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
    /// the caller must disambiguate by package -- each candidate in
    /// `ids` is a full `<resource_type>.<package>.<name>` ID, so the
    /// package to pass is right there in the message.
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
    /// `model_name` resolved to a real Node, but `column_name` isn't one
    /// of its actual (resolved) output columns.
    #[error("model {model:?} has no column named {column:?}")]
    UnknownColumn {
        /// The model whose column list didn't contain `column`.
        model: String,
        /// The column name that couldn't be resolved.
        column: String,
    },
}

/// The package segment of a dbt-shaped Node ID
/// (`<resource_type>.<package>.<name>`), if it parses as that shape --
/// `None` for anything else (an adapter that doesn't use dot-separated,
/// package-qualified IDs at all).
fn node_package(id: &NodeId) -> Option<&str> {
    id.as_str().split('.').nth(1)
}

/// Resolves `target_name` to exactly one Node in `project`, or a
/// [`LineageError`] -- shared by [`trace`] and [`trace_column`], which
/// both start by resolving a bare model name the same way.
///
/// `package`, when given, narrows the match to Nodes whose ID's package
/// segment equals it (see [`node_package`]) *before* checking for
/// ambiguity -- the mechanism [`LineageError::AmbiguousTarget`]'s own
/// message points a caller at when a bare name matches more than one
/// Node (e.g. two same-named models in different dbt packages). A
/// `package` that matches nothing still narrows to zero candidates
/// (reported as [`LineageError::UnknownTarget`], same as a name that
/// never existed at all) rather than silently falling back to
/// unfiltered matching -- a package that doesn't apply is exactly as
/// wrong as a name that doesn't exist.
fn resolve_target_node<'a>(
    project: &'a ParsedProject,
    target_name: &str,
    package: Option<&str>,
) -> Result<&'a Node, LineageError> {
    let matches: Vec<&Node> = project
        .nodes
        .iter()
        .filter(|node| node.name == target_name)
        .filter(|node| match package {
            Some(pkg) => node_package(&node.id) == Some(pkg),
            None => true,
        })
        .collect();
    match matches.as_slice() {
        [] => Err(LineageError::UnknownTarget {
            name: target_name.to_string(),
        }),
        [only] => Ok(*only),
        multiple => Err(LineageError::AmbiguousTarget {
            name: target_name.to_string(),
            ids: multiple.iter().map(|node| node.id.to_string()).collect(),
        }),
    }
}

/// Public wrapper around `resolve_target_node` for callers that just
/// need the resolved `NodeId` itself (e.g. `zhao lineage --html`'s
/// initial-target validation), without needing their own copy of the
/// bare-name-to-Node resolution/ambiguity-checking logic. `package` is
/// the same optional disambiguator `trace`/`trace_column` accept.
pub fn resolve_target(
    project: &ParsedProject,
    target_name: &str,
    package: Option<&str>,
) -> Result<NodeId, LineageError> {
    resolve_target_node(project, target_name, package).map(|node| node.id.clone())
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
/// `project` (narrowed to `package` when given -- see
/// `resolve_target_node`), then walks `direction`'s side(s) of its
/// Lineage Edges to their full transitive closure.
pub fn trace(
    project: &ParsedProject,
    target_name: &str,
    package: Option<&str>,
    direction: Direction,
) -> Result<LineageResult, LineageError> {
    let target = resolve_target_node(project, target_name, package)?;

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

// ---------------------------------------------------------------------
// Column-level lineage.
// ---------------------------------------------------------------------

/// A single column reached during a column-level lineage query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    /// The Node this column belongs to.
    pub node: NodeId,
    /// The column's name on that Node.
    pub column: ColumnName,
}

/// A single Origin column reached during a column-level lineage query --
/// kept distinct from [`ColumnRef`] since an Origin isn't a Node (zhao
/// doesn't build it, and it carries no resolved output schema of its
/// own the way a Node's `columns` does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginColumnRef {
    /// The Origin this column belongs to.
    pub origin: OriginId,
    /// The column's name on that Origin.
    pub column: ColumnName,
}

/// The result of a column-level lineage query -- the column-grain mirror
/// of [`LineageResult`]. Every `*_columns`/`*_origins` entry represents a
/// *resolved* column-to-column edge (real `ColumnLineage` data, the same
/// kind `zhao check`'s Rule catalog already consumes) -- never a guess.
///
/// `unresolved_upstream_at`/`unresolved_downstream_at` name every Node
/// reached along the way that has a real node-level dependency in that
/// direction whose specific column mapping couldn't be resolved (e.g. a
/// computed expression, or one of the SQL shapes documented as
/// unsupported in the dbt adapter's "Known limitations") -- kept
/// separate from the resolved lists specifically so "genuinely nothing
/// here" and "something's here, we just don't know which column" are
/// never conflated into the same (mis)reading of an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnLineageResult {
    /// Every column upstream of the target, resolved via `ColumnLineage`,
    /// in first-reached (breadth-first) order.
    pub upstream_columns: Vec<ColumnRef>,
    /// Every Origin column upstream of the target, resolved the same way.
    pub upstream_origins: Vec<OriginColumnRef>,
    /// Every Node with an unresolved upstream dependency reached while
    /// tracing upstream -- the specific column that traces into it
    /// couldn't be determined, but the dependency itself is real.
    pub unresolved_upstream_at: Vec<NodeId>,
    /// Every column downstream of the target, resolved via
    /// `ColumnLineage`, in first-reached (breadth-first) order.
    pub downstream_columns: Vec<ColumnRef>,
    /// Every Node with an unresolved downstream dependency reached while
    /// tracing downstream.
    pub unresolved_downstream_at: Vec<NodeId>,
}

/// Resolves a column-level lineage query: finds the Node named
/// `target_node_name` (narrowed to `package` when given -- see
/// `resolve_target_node`), confirms `target_column_name` is really
/// one of its resolved output columns, then walks `direction`'s side(s)
/// of its column-level Lineage Edges to their full transitive closure.
pub fn trace_column(
    project: &ParsedProject,
    target_node_name: &str,
    package: Option<&str>,
    target_column_name: &str,
    direction: Direction,
) -> Result<ColumnLineageResult, LineageError> {
    let target = resolve_target_node(project, target_node_name, package)?;
    let column = ColumnName::new(target_column_name);
    if !target.columns.iter().any(|c| c.name == column) {
        return Err(LineageError::UnknownColumn {
            model: target_node_name.to_string(),
            column: target_column_name.to_string(),
        });
    }

    let mut result = ColumnLineageResult::default();
    if matches!(direction, Direction::Upstream | Direction::Both) {
        walk_upstream_column(project, &target.id, &column, &mut result);
    }
    if matches!(direction, Direction::Downstream | Direction::Both) {
        walk_downstream_column(project, &target.id, &column, &mut result);
    }
    Ok(result)
}

/// Walks every *resolved* column-level edge transitively upstream of
/// `(start_node, start_column)`, breadth-first.
///
/// The dbt adapter always adds a node-level `column: None` edge for
/// every real dependency *in addition to* any column-level edges it
/// managed to resolve for that same upstream/downstream pair (see that
/// adapter's own comment: "Column-level edges above are additive detail,
/// not a replacement for these") -- so a `None` edge existing at a Node
/// is not, on its own, evidence that *this* column's mapping is
/// unresolved; plenty of fully-resolved columns still have one
/// alongside their real `Some` edge. What actually means "this column's
/// own upstream is unresolved" is the combination of: this Node has
/// *some* real upstream connectivity at all (`Some` for another column,
/// or `None`), and *none* of it names this specific column as a match.
/// That combination is recorded once in `result.unresolved_upstream_at`,
/// and the walk doesn't continue past it on that path (there's nothing
/// concrete to continue into).
fn walk_upstream_column(
    project: &ParsedProject,
    start_node: &NodeId,
    start_column: &ColumnName,
    result: &mut ColumnLineageResult,
) {
    let mut visited_columns: HashSet<(NodeId, ColumnName)> =
        HashSet::from([(start_node.clone(), start_column.clone())]);
    let mut visited_unresolved: HashSet<NodeId> = HashSet::new();
    let mut frontier: VecDeque<(NodeId, ColumnName)> =
        VecDeque::from([(start_node.clone(), start_column.clone())]);

    while let Some((node, column)) = frontier.pop_front() {
        let mut found_resolved_match = false;
        let mut has_upstream_connectivity = false;
        for edge in &project.edges {
            if edge.downstream != node {
                continue;
            }
            has_upstream_connectivity = true;
            let Some(lineage) = &edge.column else {
                continue;
            };
            if lineage.downstream_column != column {
                continue;
            }
            found_resolved_match = true;
            match &edge.upstream {
                Upstream::Node(id) => {
                    let key = (id.clone(), lineage.upstream_column.clone());
                    if visited_columns.insert(key.clone()) {
                        result.upstream_columns.push(ColumnRef {
                            node: id.clone(),
                            column: lineage.upstream_column.clone(),
                        });
                        frontier.push_back(key);
                    }
                }
                Upstream::Origin(id) => {
                    let origin_ref = OriginColumnRef {
                        origin: id.clone(),
                        column: lineage.upstream_column.clone(),
                    };
                    if !result.upstream_origins.contains(&origin_ref) {
                        result.upstream_origins.push(origin_ref);
                    }
                }
            }
        }
        if !found_resolved_match
            && has_upstream_connectivity
            && visited_unresolved.insert(node.clone())
        {
            result.unresolved_upstream_at.push(node.clone());
        }
    }
}

/// The downstream mirror of [`walk_upstream_column`] -- Origins never
/// appear here, for the same reason [`walk_downstream`] never reaches
/// one: nothing is ever downstream of something zhao doesn't build. See
/// [`walk_upstream_column`]'s doc comment for why "unresolved" means
/// more than just "some edge here has `column: None`."
fn walk_downstream_column(
    project: &ParsedProject,
    start_node: &NodeId,
    start_column: &ColumnName,
    result: &mut ColumnLineageResult,
) {
    let mut visited_columns: HashSet<(NodeId, ColumnName)> =
        HashSet::from([(start_node.clone(), start_column.clone())]);
    let mut visited_unresolved: HashSet<NodeId> = HashSet::new();
    let mut frontier: VecDeque<(NodeId, ColumnName)> =
        VecDeque::from([(start_node.clone(), start_column.clone())]);

    while let Some((node, column)) = frontier.pop_front() {
        let mut found_resolved_match = false;
        let mut has_downstream_connectivity = false;
        for edge in &project.edges {
            let Upstream::Node(upstream_id) = &edge.upstream else {
                continue;
            };
            if upstream_id != &node {
                continue;
            }
            has_downstream_connectivity = true;
            let Some(lineage) = &edge.column else {
                continue;
            };
            if lineage.upstream_column != column {
                continue;
            }
            found_resolved_match = true;
            let key = (edge.downstream.clone(), lineage.downstream_column.clone());
            if visited_columns.insert(key.clone()) {
                result.downstream_columns.push(ColumnRef {
                    node: edge.downstream.clone(),
                    column: lineage.downstream_column.clone(),
                });
                frontier.push_back(key);
            }
        }
        if !found_resolved_match
            && has_downstream_connectivity
            && visited_unresolved.insert(node.clone())
        {
            result.unresolved_downstream_at.push(node.clone());
        }
    }
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
        let result = trace(&project, "b", None, Direction::Both).expect("b should exist");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
        assert_eq!(result.upstream_origins, vec![OriginId::new("source.p.raw")]);
        assert_eq!(result.downstream_nodes, vec![NodeId::new("model.p.d")]);
    }

    #[test]
    fn upstream_direction_excludes_downstream() {
        let project = diamond_project();
        let result = trace(&project, "b", None, Direction::Upstream).expect("b should exist");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
        assert!(result.downstream_nodes.is_empty());
    }

    #[test]
    fn downstream_direction_excludes_upstream() {
        let project = diamond_project();
        let result = trace(&project, "b", None, Direction::Downstream).expect("b should exist");

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
        let result = trace(&project, "a", None, Direction::Downstream).expect("a should exist");

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
        let result = trace(&project, "does_not_exist", None, Direction::Both);

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
        let result =
            trace(&project, "isolated", None, Direction::Both).expect("isolated should exist");

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

        let result = trace(&project, "a", None, Direction::Both);

        assert_eq!(
            result,
            Err(LineageError::AmbiguousTarget {
                name: "a".to_string(),
                ids: vec!["model.p.a".to_string(), "model.other_package.a".to_string()],
            })
        );
    }

    /// The acceptance criterion this ticket exists for: an otherwise
    /// ambiguous bare name resolves cleanly once `package` narrows it to
    /// the one Node that actually lives in that package.
    #[test]
    fn a_package_disambiguates_an_otherwise_ambiguous_target() {
        let mut project = diamond_project();
        project.nodes.push(node("model.other_package.a"));

        let result =
            trace(&project, "a", Some("other_package"), Direction::Both).expect("should resolve");

        // Resolves to `model.other_package.a`, which has none of
        // `diamond_project`'s edges -- distinct from `model.p.a`'s
        // result (asserted in `bare_target_returns_both_directions`),
        // proving the *right* Node was picked, not just *a* Node.
        assert_eq!(result, LineageResult::default());

        let other_way = trace(&project, "a", Some("p"), Direction::Both).expect("should resolve");
        assert_eq!(
            other_way.upstream_origins,
            vec![OriginId::new("source.p.raw")]
        );
    }

    /// A `package` that doesn't match any candidate for the given name
    /// narrows to zero, reported the same as a name that never existed
    /// at all -- not silently falling back to unfiltered (still
    /// ambiguous) matching.
    #[test]
    fn a_package_matching_no_candidate_produces_unknown_target_not_ambiguous() {
        let mut project = diamond_project();
        project.nodes.push(node("model.other_package.a"));

        let result = trace(&project, "a", Some("does_not_exist"), Direction::Both);

        assert_eq!(
            result,
            Err(LineageError::UnknownTarget {
                name: "a".to_string(),
            })
        );
    }

    /// `package` has no effect on an already-unambiguous name -- it's
    /// purely a disambiguator, not a requirement to specify one.
    #[test]
    fn a_package_on_an_already_unambiguous_target_still_resolves() {
        let project = diamond_project();
        let result =
            trace(&project, "b", Some("p"), Direction::Both).expect("b should still resolve");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
    }

    /// `resolve_target` (the public wrapper `zhao lineage --html` uses)
    /// accepts the same disambiguator.
    #[test]
    fn resolve_target_accepts_a_package_disambiguator() {
        let mut project = diamond_project();
        project.nodes.push(node("model.other_package.a"));

        let resolved = resolve_target(&project, "a", Some("other_package"))
            .expect("should resolve with the package given");
        assert_eq!(resolved, NodeId::new("model.other_package.a"));

        let still_ambiguous = resolve_target(&project, "a", None);
        assert!(matches!(
            still_ambiguous,
            Err(LineageError::AmbiguousTarget { .. })
        ));
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

        let result = trace(&project, "a", None, Direction::Downstream).expect("a should exist");

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

    // -------------------------------------------------------------
    // Column-level lineage.
    // -------------------------------------------------------------

    fn node_with_columns(id: &str, columns: &[&str]) -> Node {
        Node {
            columns: columns
                .iter()
                .map(|c| crate::model::Column {
                    name: ColumnName::new(*c),
                    data_type: None,
                    expression: None,
                    struct_fields: None,
                })
                .collect(),
            ..node(id)
        }
    }

    fn column(name: &str) -> ColumnName {
        ColumnName::new(name)
    }

    fn column_edge(
        upstream: &str,
        upstream_column: &str,
        downstream: &str,
        downstream_column: &str,
    ) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Node(NodeId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: Some(crate::model::ColumnLineage {
                upstream_column: column(upstream_column),
                downstream_column: column(downstream_column),
            }),
        }
    }

    fn origin_column_edge(
        upstream_origin: &str,
        upstream_column: &str,
        downstream: &str,
        downstream_column: &str,
    ) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Origin(OriginId::new(upstream_origin)),
            downstream: NodeId::new(downstream),
            column: Some(crate::model::ColumnLineage {
                upstream_column: column(upstream_column),
                downstream_column: column(downstream_column),
            }),
        }
    }

    /// `origin.raw.x` -> `a.x` -> `b.x` -> `c.x`, a clean passthrough
    /// chain, plus `b.y`: a computed column with no resolved source --
    /// `b`'s node-level dependency on `a` is real (the accompanying
    /// `column: None` edge dbt's adapter always adds alongside resolved
    /// ones), but `y` specifically has no `Some` edge naming it.
    fn column_chain_project() -> ParsedProject {
        ParsedProject {
            nodes: vec![
                node_with_columns("model.p.a", &["x"]),
                node_with_columns("model.p.b", &["x", "y"]),
                node_with_columns("model.p.c", &["x"]),
            ],
            origins: vec![origin("source.p.raw")],
            edges: vec![
                origin_column_edge("source.p.raw", "x", "model.p.a", "x"),
                column_edge("model.p.a", "x", "model.p.b", "x"),
                // The always-present node-level fallback edge dbt's
                // adapter adds alongside the resolved one above --
                // proves this alone must NOT trigger "unresolved" for
                // b.x, which fully resolved via the edge above.
                node_edge("model.p.a", "model.p.b"),
                column_edge("model.p.b", "x", "model.p.c", "x"),
            ],
        }
    }

    #[test]
    fn resolved_column_chain_traces_in_both_directions() {
        let project = column_chain_project();
        let result =
            trace_column(&project, "b", None, "x", Direction::Both).expect("b.x should exist");

        assert_eq!(
            result.upstream_columns,
            vec![ColumnRef {
                node: NodeId::new("model.p.a"),
                column: column("x"),
            }]
        );
        assert_eq!(
            result.upstream_origins,
            vec![OriginColumnRef {
                origin: OriginId::new("source.p.raw"),
                column: column("x"),
            }]
        );
        assert_eq!(
            result.downstream_columns,
            vec![ColumnRef {
                node: NodeId::new("model.p.c"),
                column: column("x"),
            }]
        );
        assert!(
            result.unresolved_upstream_at.is_empty(),
            "a fully-resolved column chain must not be flagged unresolved: {result:?}"
        );
    }

    /// The key regression this module's design had to get right: a
    /// node-level `column: None` edge existing alongside a fully-resolved
    /// `Some` edge for the SAME pair (exactly what dbt's adapter always
    /// produces) must never, on its own, mark a column unresolved.
    #[test]
    fn a_resolved_column_is_never_flagged_unresolved_by_the_companion_node_level_edge() {
        let project = column_chain_project();
        let result =
            trace_column(&project, "b", None, "x", Direction::Upstream).expect("b.x exists");

        assert!(result.unresolved_upstream_at.is_empty(), "{result:?}");
    }

    /// Acceptance criterion: a column whose lineage couldn't be resolved
    /// (here, `b.y`, a computed column) is reported as unresolved, not
    /// silently omitted or shown as if fully traced.
    #[test]
    fn an_unresolved_column_is_reported_not_omitted() {
        let project = column_chain_project();
        let result =
            trace_column(&project, "b", None, "y", Direction::Upstream).expect("b.y should exist");

        assert!(
            result.upstream_columns.is_empty(),
            "y has no resolved upstream column: {result:?}"
        );
        assert_eq!(
            result.unresolved_upstream_at,
            vec![NodeId::new("model.p.b")]
        );
    }

    /// Acceptance criterion: `+<model>.<column>` restricts to upstream
    /// only.
    #[test]
    fn plus_prefix_restricts_to_upstream_only() {
        let project = column_chain_project();
        let result =
            trace_column(&project, "b", None, "x", Direction::Upstream).expect("b.x should exist");

        assert!(!result.upstream_columns.is_empty());
        assert!(result.downstream_columns.is_empty());
    }

    /// Acceptance criterion: `<model>.<column>+` restricts to downstream
    /// only.
    #[test]
    fn plus_suffix_restricts_to_downstream_only() {
        let project = column_chain_project();
        let result = trace_column(&project, "b", None, "x", Direction::Downstream)
            .expect("b.x should exist");

        assert!(result.upstream_columns.is_empty());
        assert!(!result.downstream_columns.is_empty());
    }

    /// Acceptance criterion: an unknown `model.column` target produces a
    /// clear, actionable error.
    #[test]
    fn an_unknown_column_produces_a_clear_error() {
        let project = column_chain_project();
        let result = trace_column(&project, "b", None, "does_not_exist", Direction::Both);

        assert_eq!(
            result,
            Err(LineageError::UnknownColumn {
                model: "b".to_string(),
                column: "does_not_exist".to_string(),
            })
        );
    }

    /// Acceptance criterion: model-level targets continue to work
    /// unchanged alongside the new column-level capability.
    #[test]
    fn model_level_trace_is_unaffected_by_column_level_additions() {
        let project = column_chain_project();
        let result = trace(&project, "b", None, Direction::Both).expect("b should exist");

        assert_eq!(result.upstream_nodes, vec![NodeId::new("model.p.a")]);
    }

    /// Column-level parity with `downstream_order_is_genuinely_breadth_first`
    /// (the model-level equivalent): on an asymmetric-depth column chain
    /// (`a.x -> b.x, a.x -> c.x, b.x -> d.x, c.x -> e.x, d.x -> f.x`), true
    /// BFS visits `[b, c, d, e, f]` -- `e` (2 hops via the shorter `c`
    /// branch) before `f` (3 hops via the longer `b -> d` branch) is
    /// exactly what a depth-first/stack-based walk would get wrong.
    #[test]
    fn downstream_column_order_is_genuinely_breadth_first() {
        let project = ParsedProject {
            nodes: vec![
                node_with_columns("model.p.a", &["x"]),
                node_with_columns("model.p.b", &["x"]),
                node_with_columns("model.p.c", &["x"]),
                node_with_columns("model.p.d", &["x"]),
                node_with_columns("model.p.e", &["x"]),
                node_with_columns("model.p.f", &["x"]),
            ],
            origins: Vec::new(),
            edges: vec![
                column_edge("model.p.a", "x", "model.p.b", "x"),
                column_edge("model.p.a", "x", "model.p.c", "x"),
                column_edge("model.p.b", "x", "model.p.d", "x"),
                column_edge("model.p.c", "x", "model.p.e", "x"),
                column_edge("model.p.d", "x", "model.p.f", "x"),
            ],
        };

        let result = trace_column(&project, "a", None, "x", Direction::Downstream)
            .expect("a.x should exist");

        assert_eq!(
            result.downstream_columns,
            vec![
                ColumnRef {
                    node: NodeId::new("model.p.b"),
                    column: column("x")
                },
                ColumnRef {
                    node: NodeId::new("model.p.c"),
                    column: column("x")
                },
                ColumnRef {
                    node: NodeId::new("model.p.d"),
                    column: column("x")
                },
                ColumnRef {
                    node: NodeId::new("model.p.e"),
                    column: column("x")
                },
                ColumnRef {
                    node: NodeId::new("model.p.f"),
                    column: column("x")
                },
            ]
        );
    }
}
