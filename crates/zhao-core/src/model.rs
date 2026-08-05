//! zhao's neutral domain vocabulary: Node, Origin, Lineage Edge, and the
//! container type ([`ParsedProject`]) a [`crate::adapters::TransformationToolAdapter`]
//! produces from a specific project format.
//!
//! Nothing in this module knows about any particular transformation tool.
//! See `ARCHITECTURE.md` at the repository root for how this fits into the
//! rest of the crate.

use std::fmt;

/// The stable identifier a [`Node`] is known by.
///
/// Wraps a `String` rather than exposing one directly so that a Node ID
/// can't be silently swapped for an arbitrary string (a column name, for
/// example) at a call site -- the type system catches the mistake instead
/// of it surfacing as a runtime lookup failure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(String);

impl NodeId {
    /// Creates a `NodeId` from its underlying string form.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a plain string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The stable identifier an [`Origin`] is known by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OriginId(String);

impl OriginId {
    /// Creates an `OriginId` from its underlying string form.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a plain string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OriginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The name of a single column within a [`Node`]'s or [`Origin`]'s schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnName(String);

impl ColumnName {
    /// Creates a `ColumnName` from its underlying string form.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns the column name as a plain string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single column within a [`Node`]'s or [`Origin`]'s schema: its name
/// and, where documented, its data type.
///
/// `data_type` reflects only what the source project happens to document
/// (e.g. a dbt `schema.yml` entry) -- zhao never infers a type by
/// connecting to a real warehouse, so it's absent far more often than
/// present. A type-level [`crate::adapters::TransformationToolAdapter`]-produced
/// comparison is only possible when both sides of a comparison happen to
/// have it documented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// This column's name.
    pub name: ColumnName,
    /// This column's documented data type, if the source project records one.
    pub data_type: Option<String>,
    /// The rendered SQL of this column's defining expression, when it's a
    /// calculated/derived column (a function call, `CAST`, arithmetic,
    /// `CASE`, ...). `None` for a plain passthrough of an upstream column
    /// (a bare identifier reference, possibly renamed) -- there's no
    /// expression more informative than "this is that column" to show.
    /// Re-rendered from the parsed SQL AST, so it's a faithful
    /// representation of the computation but not necessarily
    /// byte-identical to the original source text (whitespace,
    /// capitalization, and equivalent syntax may differ).
    pub expression: Option<String>,
}

/// The kind of join a [`Node`]'s definition uses to combine two relations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// An `INNER JOIN` (or a bare `JOIN`, which means the same thing).
    Inner,
    /// A `LEFT [OUTER] JOIN`.
    Left,
    /// A `RIGHT [OUTER] JOIN`.
    Right,
    /// A `FULL OUTER JOIN`.
    Full,
    /// A `CROSS JOIN`.
    Cross,
}

/// How a [`Node`] is physically persisted when built -- e.g. a dbt model's
/// `config.materialized`. Format-agnostic: any transformation tool with a
/// similar concept maps into these same variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Materialization {
    /// Rebuilt from scratch on every run.
    Table,
    /// Not persisted at all -- always computed live from its definition.
    View,
    /// Persisted and updated in place across runs (e.g. dbt's
    /// `incremental`, including a microbatch strategy -- microbatch is a
    /// strategy *of* incremental materialization, not a distinct kind).
    /// This is the only Materialization a Node's already-existing state in
    /// a target environment can matter for: a schema change here can't
    /// simply be rebuilt away, since the whole point is not rebuilding
    /// from scratch.
    Incremental,
    /// Never actually persisted -- inlined into whatever references it
    /// (e.g. dbt's `ephemeral`).
    Ephemeral,
    /// A materialization kind zhao doesn't specifically recognize, kept
    /// verbatim rather than discarded.
    Other(String),
}

/// The atomic buildable thing zhao's core reasons about.
///
/// A dbt `model` is translated into a Node by the dbt Transformation Tool
/// Adapter; other project formats will translate into Nodes the same way.
/// A Node's `columns` reflect its actual output schema, resolved from its
/// definition -- not merely whichever columns happen to be documented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// This Node's stable identifier.
    pub id: NodeId,
    /// This Node's short, human-facing name (e.g. a dbt model's name).
    pub name: String,
    /// The columns this Node exposes, in output order.
    pub columns: Vec<Column>,
    /// The kind of each join in this Node's definition, in the order they
    /// appear. Only joins whose kind maps to one of [`JoinKind`]'s variants
    /// are included -- a non-standard or unrecognized join is omitted
    /// rather than misrepresented as some other kind.
    pub joins: Vec<JoinKind>,
    /// How this Node is physically persisted when built.
    pub materialization: Materialization,
}

/// An external input a [`Node`] reads from but that zhao does not build.
///
/// A dbt `source` is translated into an Origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// This Origin's stable identifier.
    pub id: OriginId,
    /// This Origin's short, human-facing name (e.g. a dbt source table's name).
    pub name: String,
}

/// Either end of a [`LineageEdge`] that can be an upstream dependency: a
/// Node (something zhao builds) or an Origin (something it only reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// The upstream dependency is a Node zhao builds.
    Node(NodeId),
    /// The upstream dependency is an Origin zhao only reads from.
    Origin(OriginId),
}

/// Column-level detail on a [`LineageEdge`], present when the specific
/// upstream column a downstream column derives from could be resolved.
///
/// Not every column can be resolved this precisely -- a computed column
/// (an expression, a function call, a literal) has no single upstream
/// column to point at, so its `LineageEdge` carries `column: None` even
/// though the node-level dependency is still real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnLineage {
    /// The column on the upstream Node or Origin that this data comes from.
    pub upstream_column: ColumnName,
    /// The column on the downstream Node that receives it.
    pub downstream_column: ColumnName,
}

/// A reference from one [`Node`] to an upstream [`Node`] or [`Origin`] it
/// depends on, at both the whole-node level and, where resolvable, the
/// individual-column level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageEdge {
    /// The upstream dependency: a Node or an Origin.
    pub upstream: Upstream,
    /// The downstream Node that depends on it.
    pub downstream: NodeId,
    /// Column-level detail, when the specific column mapping is resolvable.
    pub column: Option<ColumnLineage>,
}

/// The full set of Nodes, Origins, and Lineage Edges produced by parsing a
/// project through a [`crate::adapters::TransformationToolAdapter`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedProject {
    /// Every Node found in the project.
    pub nodes: Vec<Node>,
    /// Every Origin found in the project.
    pub origins: Vec<Origin>,
    /// Every Lineage Edge found between them.
    pub edges: Vec<LineageEdge>,
}

impl ParsedProject {
    /// Returns the Node with the given ID, if present.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    /// Returns the Origin with the given ID, if present.
    pub fn origin(&self, id: &OriginId) -> Option<&Origin> {
        self.origins.iter().find(|o| &o.id == id)
    }
}
