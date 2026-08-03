//! Trait boundaries between zhao-core's neutral engine and anything
//! specific to a transformation tool or a warehouse.
//!
//! See `ARCHITECTURE.md` at the repository root for the reasoning behind
//! this split. dbt is the first (and, for now, only) implementation of
//! [`TransformationToolAdapter`]; see [`dbt`].

pub mod dbt;

use crate::model::ParsedProject;
use std::error::Error;
use std::path::Path;

/// Reads a specific transformation-tool project format and produces zhao's
/// internal representation of it.
///
/// Implementations own everything specific to how their tool expresses
/// transformations. Nothing outside an adapter's own module should depend
/// on that tool's specific types -- callers only ever see this trait's
/// associated types and methods, never (for example) a raw dbt manifest
/// structure.
pub trait TransformationToolAdapter {
    /// The error type this adapter can fail with while parsing.
    type Error: Error;

    /// Parses the project's compiled state into zhao's internal
    /// representation.
    ///
    /// `path` points at whatever this adapter's tool produces as its
    /// compiled output (for dbt, a `manifest.json` file). Resolving that
    /// path in the first place -- e.g. running a tool's own compile step --
    /// is the caller's responsibility, not this method's.
    fn parse(&self, path: &Path) -> Result<ParsedProject, Self::Error>;

    /// Maps zhao's neutral vocabulary to this tool's own terms, for
    /// surfacing in user-facing output (a dbt user should see "model," not
    /// "Node").
    fn vocabulary(&self) -> &dyn AdapterVocabulary;
}

/// The label mapping a [`TransformationToolAdapter`] owns from zhao's
/// neutral core terms to a transformation tool's own familiar words.
///
/// Applied only at user-facing surfaces (CLI output, reports); the
/// underlying data always uses zhao's neutral terms regardless of which
/// adapter produced it.
pub trait AdapterVocabulary {
    /// This tool's word for a [`crate::model::Node`] (e.g. "model" for dbt).
    fn node_term(&self) -> &'static str;

    /// This tool's word for an [`crate::model::Origin`] (e.g. "source" for dbt).
    fn origin_term(&self) -> &'static str;

    /// A ready-to-run command, in this tool's own selector syntax,
    /// recommending exactly which Nodes to validate -- `node_ids` are
    /// zhao's own qualified `NodeId` strings (e.g. a dbt model's
    /// `unique_id`, `model.<package>.<name>`); this tool derives its own
    /// selectable name for each from that string itself, rather than
    /// requiring a caller to look up a full `Node` first -- a Node
    /// reached only via the Baseline (e.g. one deleted entirely in the
    /// current state) may have no corresponding `Node` to look up at all,
    /// but its ID string is still enough to name it. `None` if `node_ids`
    /// is empty: there's nothing to recommend validating.
    fn recommended_validation_command(&self, node_ids: &[String]) -> Option<String>;
}
