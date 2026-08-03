//! Rendering a `zhao check` run's results as JSON or brief text.
//!
//! zhao-core's own types ([`zhao_core::diff::Change`],
//! [`zhao_core::rules::Finding`]) intentionally carry no serialization
//! derives -- they're the engine's internal vocabulary, not a wire format
//! commitment. This module owns the JSON shape as its own, separate
//! concern, converting from the engine's types rather than exposing them
//! directly.

use serde::Serialize;
use zhao_core::adapters::AdapterVocabulary;
use zhao_core::diff::Change;
use zhao_core::model::{JoinKind, NodeId, ParsedProject, Upstream};
use zhao_core::rules::{Finding, FindingDetail, Severity};

/// The message included in a [`Report`] when the Baseline's merge-base has
/// fallen behind the target branch's current tip.
pub const STALENESS_WARNING: &str = "analysis may be stale, consider rebasing";

/// The full JSON payload for a `zhao check` run.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Every Change detected between the Baseline and the current state.
    pub changes: Vec<ChangeJson>,
    /// Every Rule that fired against those Changes.
    pub findings: Vec<FindingJson>,
    /// Present when the target branch has moved on since the Baseline's
    /// merge-base, so this run's analysis may not reflect the target
    /// branch's latest state. Purely informational: never affects
    /// [`Report::is_breaking`] or the process exit code, regardless of
    /// Preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staleness_warning: Option<String>,
    /// A ready-to-run command (in the adapter's own selector syntax)
    /// recommending exactly which Nodes named in `findings`' Downstream
    /// impact to validate. `None` when there's nothing to validate: either
    /// no impactful (non-`pass`-severity) Finding fired at all, or this
    /// report was built without [`Report::with_recommended_command`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_command: Option<String>,
    /// The computed dbt `--defer` plan: which Nodes need building (the
    /// same set `recommended_command` selects) and which of their
    /// upstream dependencies can be deferred to an existing state
    /// instead. `None` under the same conditions as `recommended_command`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_plan: Option<DeferPlanJson>,
}

impl Report {
    /// Builds a [`Report`] from the engine's own `Change`/`Finding` output.
    /// No staleness warning, recommended command, or defer plan is set --
    /// chain [`Report::with_staleness_warning`]/
    /// [`Report::with_recommended_command`]/[`Report::with_defer_plan`] to
    /// add them.
    pub fn new(changes: &[Change], findings: &[Finding]) -> Self {
        Self {
            changes: changes.iter().map(ChangeJson::from).collect(),
            findings: findings.iter().map(FindingJson::from).collect(),
            staleness_warning: None,
            recommended_command: None,
            defer_plan: None,
        }
    }

    /// The exact set of Nodes named in the Downstream impact section --
    /// every non-`pass` Finding's [`FindingJson::impacted_node`],
    /// deduplicated, in first-seen order. Shared by
    /// [`Report::with_recommended_command`] and
    /// [`Report::with_defer_plan`], which both need precisely this set:
    /// the former to name it in the adapter's selector syntax, the latter
    /// as the `--defer` plan's "build" set.
    fn impacted_node_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut node_ids = Vec::new();
        for finding in &self.findings {
            if finding.severity() == SeverityJson::Pass {
                continue;
            }
            let node_id = finding.impacted_node().to_string();
            if seen.insert(node_id.clone()) {
                node_ids.push(node_id);
            }
        }
        node_ids
    }

    /// Sets this report's staleness warning: `Some(`[`STALENESS_WARNING`]`)`
    /// if the Baseline's merge-base is behind the target branch's current
    /// tip, `None` otherwise (including when that couldn't be determined
    /// at all, e.g. outside a git repository -- staleness is purely
    /// best-effort and informational, never a hard requirement).
    pub fn with_staleness_warning(mut self, is_stale: bool) -> Self {
        self.staleness_warning = is_stale.then(|| STALENESS_WARNING.to_string());
        self
    }

    /// Sets this report's recommended validation command: the exact set
    /// of Nodes named in the Downstream impact section (every non-`pass`
    /// Finding's [`FindingJson::impacted_node`], deduplicated), handed to
    /// `vocabulary` as zhao's own `NodeId` strings to render in the
    /// adapter's own selector syntax.
    ///
    /// Deliberately does *not* look a Node up by ID first (an earlier
    /// version of this method did, resolving each ID against a
    /// `ParsedProject`): an impacted Node reached only via the Baseline
    /// (e.g. one deleted entirely in the current state, for
    /// `ColumnRemovedWithActiveReferences`) may have no corresponding
    /// `Node` in the current state to look up at all, even though it's
    /// still correctly named in the Downstream impact section from its ID
    /// string alone -- looking it up first would silently drop it from
    /// this command, undermining the "matches Downstream impact exactly"
    /// contract. `vocabulary` is expected to derive its own name straight
    /// from the ID string instead (e.g. dbt's `unique_id` shape already
    /// contains the bare model name).
    pub fn with_recommended_command(mut self, vocabulary: &dyn AdapterVocabulary) -> Self {
        self.recommended_command =
            vocabulary.recommended_validation_command(&self.impacted_node_ids());
        self
    }

    /// Sets this report's `--defer` plan: `build` is the same impacted-Node
    /// set [`Report::with_recommended_command`] selects; `defer` is every
    /// Node those Nodes depend on (directly or transitively, through
    /// `current`'s Lineage Edges) that isn't itself in `build` -- Nodes a
    /// CI job building only the impacted set should treat as already
    /// available (via `dbt ... --defer --state <path>`) rather than
    /// rebuild from scratch. Pure computation: this method never connects
    /// to a warehouse or provisions anything.
    ///
    /// `None` when nothing is impacted at all (nothing to build, so no
    /// plan makes sense); `Some` with an empty `defer` list is meaningful
    /// otherwise -- it means every dependency of the build set is an
    /// Origin, not a Node, so there's genuinely nothing to defer.
    pub fn with_defer_plan(
        mut self,
        current: &ParsedProject,
        vocabulary: &dyn AdapterVocabulary,
    ) -> Self {
        let build = self.impacted_node_ids();
        self.defer_plan = if build.is_empty() {
            None
        } else {
            Some(DeferPlanJson::compute(current, build, vocabulary))
        };
        self
    }

    /// Whether this run's Findings should fail the CI gate: any Finding
    /// at [`Severity::Error`]. A staleness warning never contributes here,
    /// under any Preset.
    pub fn is_breaking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity() == SeverityJson::Error)
    }
}

/// The computed dbt `--defer` plan for a run -- see
/// [`Report::with_defer_plan`].
#[derive(Debug, Serialize)]
pub struct DeferPlanJson {
    /// Nodes that need to be built: the same set named in Downstream
    /// impact / the recommended command's selector.
    pub build: Vec<String>,
    /// Nodes `build`'s Nodes depend on (directly or transitively) that
    /// aren't themselves in `build` -- these should be deferred to an
    /// existing state (`dbt ... --defer --state <path>`) rather than
    /// rebuilt.
    pub defer: Vec<String>,
}

impl DeferPlanJson {
    /// Computes the plan: `build` is used as given (the caller already
    /// determined the impacted set); `defer` is `build`'s full transitive
    /// upstream Node closure (walking `current`'s Lineage Edges), minus
    /// `build` itself. Origins are never included -- dbt never builds a
    /// source in the first place, so there's nothing to defer for one.
    /// The graph walk itself works in zhao's own `NodeId` strings; both
    /// `build` and `defer` are rendered through `vocabulary` at the end,
    /// same as [`Report::with_recommended_command`], so a `--defer` plan
    /// names Nodes the same way the recommended command does rather than
    /// mixing zhao's internal IDs with the adapter's own names.
    fn compute(
        current: &ParsedProject,
        build: Vec<String>,
        vocabulary: &dyn AdapterVocabulary,
    ) -> Self {
        let build_set: std::collections::HashSet<&str> = build.iter().map(String::as_str).collect();
        let mut visited: std::collections::HashSet<String> = build.iter().cloned().collect();
        let mut deferred = std::collections::BTreeSet::new();
        let mut frontier: Vec<NodeId> = build.iter().map(|id| NodeId::new(id.clone())).collect();

        while let Some(node_id) = frontier.pop() {
            for edge in &current.edges {
                if edge.downstream != node_id {
                    continue;
                }
                let Upstream::Node(upstream_id) = &edge.upstream else {
                    continue;
                };
                let upstream_id_string = upstream_id.to_string();
                if visited.insert(upstream_id_string.clone()) {
                    if !build_set.contains(upstream_id_string.as_str()) {
                        deferred.insert(upstream_id_string);
                    }
                    frontier.push(upstream_id.clone());
                }
            }
        }

        Self {
            build: build
                .iter()
                .map(|id| vocabulary.node_display_name(id))
                .collect(),
            defer: deferred
                .iter()
                .map(|id| vocabulary.node_display_name(id))
                .collect(),
        }
    }
}

/// A [`Change`], reshaped for JSON output.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChangeJson {
    /// See [`Change::ColumnAdded`].
    ColumnAdded { node: String, column: String },
    /// See [`Change::ColumnRemoved`].
    ColumnRemoved { node: String, column: String },
    /// See [`Change::ColumnTypeChanged`].
    ColumnTypeChanged {
        node: String,
        column: String,
        from_type: String,
        to_type: String,
    },
    /// See [`Change::JoinChanged`].
    JoinChanged {
        node: String,
        position: usize,
        from_kind: Option<String>,
        to_kind: Option<String>,
    },
}

impl ChangeJson {
    /// The name of the Node this Change belongs to, regardless of variant.
    fn node(&self) -> &str {
        match self {
            ChangeJson::ColumnAdded { node, .. }
            | ChangeJson::ColumnRemoved { node, .. }
            | ChangeJson::ColumnTypeChanged { node, .. }
            | ChangeJson::JoinChanged { node, .. } => node,
        }
    }

    /// A one-line, human-readable description of just this Change (no
    /// Node name -- the "Changed" section groups these under their Node).
    fn describe(&self) -> String {
        match self {
            ChangeJson::ColumnAdded { column, .. } => format!("+ column added: {column}"),
            ChangeJson::ColumnRemoved { column, .. } => format!("- column removed: {column}"),
            ChangeJson::ColumnTypeChanged {
                column,
                from_type,
                to_type,
                ..
            } => format!("~ column type changed: {column} ({from_type} -> {to_type})"),
            ChangeJson::JoinChanged {
                position,
                from_kind,
                to_kind,
                ..
            } => format!(
                "~ join changed at position {position}: {} -> {}",
                from_kind.as_deref().unwrap_or("none"),
                to_kind.as_deref().unwrap_or("none")
            ),
        }
    }

    /// Whether this Change counts toward the summary line's "column(s)
    /// changed" tally -- everything except a join change, which isn't a
    /// column.
    fn is_column_change(&self) -> bool {
        !matches!(self, ChangeJson::JoinChanged { .. })
    }
}

impl From<&Change> for ChangeJson {
    fn from(change: &Change) -> Self {
        match change {
            Change::ColumnAdded { node, column } => ChangeJson::ColumnAdded {
                node: node.to_string(),
                column: column.to_string(),
            },
            Change::ColumnRemoved { node, column } => ChangeJson::ColumnRemoved {
                node: node.to_string(),
                column: column.to_string(),
            },
            Change::ColumnTypeChanged {
                node,
                column,
                from_type,
                to_type,
            } => ChangeJson::ColumnTypeChanged {
                node: node.to_string(),
                column: column.to_string(),
                from_type: from_type.clone(),
                to_type: to_type.clone(),
            },
            Change::JoinChanged {
                node,
                position,
                from_kind,
                to_kind,
            } => ChangeJson::JoinChanged {
                node: node.to_string(),
                position: *position,
                from_kind: from_kind.map(join_kind_slug),
                to_kind: to_kind.map(join_kind_slug),
            },
        }
    }
}

/// A [`Severity`], reshaped for JSON output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityJson {
    /// See [`Severity::Error`].
    Error,
    /// See [`Severity::Warn`].
    Warn,
    /// See [`Severity::Pass`].
    Pass,
}

impl From<Severity> for SeverityJson {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => SeverityJson::Error,
            Severity::Warn => SeverityJson::Warn,
            Severity::Pass => SeverityJson::Pass,
        }
    }
}

/// A [`Finding`], reshaped for JSON output.
///
/// Tagged by `rule` via serde directly from each variant's name (rather
/// than a hand-written slug function): renaming a variant automatically
/// changes its JSON tag with the compiler enforcing every variant stays
/// handled, instead of a parallel string-mapping function that could
/// silently drift out of sync.
#[derive(Debug, Serialize)]
#[serde(tag = "rule", rename_all = "kebab-case")]
pub enum FindingJson {
    /// See [`FindingDetail::ColumnRemovedWithActiveReferences`].
    ColumnRemovedWithActiveReferences {
        severity: SeverityJson,
        node: String,
        column: String,
        reached: String,
        reached_column: String,
    },
    /// See [`FindingDetail::ColumnTypeNarrowed`].
    ColumnTypeNarrowed {
        severity: SeverityJson,
        node: String,
        column: String,
        from_type: String,
        to_type: String,
    },
    /// See [`FindingDetail::JoinCardinalityLoosened`].
    JoinCardinalityLoosened {
        severity: SeverityJson,
        node: String,
        position: usize,
        from_kind: String,
        to_kind: String,
    },
    /// See [`FindingDetail::ColumnAdded`].
    ColumnAdded {
        severity: SeverityJson,
        node: String,
        column: String,
    },
}

impl FindingJson {
    fn severity(&self) -> SeverityJson {
        match self {
            FindingJson::ColumnRemovedWithActiveReferences { severity, .. }
            | FindingJson::ColumnTypeNarrowed { severity, .. }
            | FindingJson::JoinCardinalityLoosened { severity, .. }
            | FindingJson::ColumnAdded { severity, .. } => *severity,
        }
    }

    /// This Finding's Rule, as the same kebab-case slug serde tags it with
    /// in JSON (`#[serde(tag = "rule", rename_all = "kebab-case")]` on
    /// this enum) -- an explicit match rather than deriving it from the
    /// JSON representation, so this and the JSON tag can't independently
    /// drift; `finding_json_rule_name_matches_its_serialized_json_tag`
    /// below cross-checks the two stay in sync.
    fn rule_name(&self) -> &'static str {
        match self {
            FindingJson::ColumnRemovedWithActiveReferences { .. } => {
                "column-removed-with-active-references"
            }
            FindingJson::ColumnTypeNarrowed { .. } => "column-type-narrowed",
            FindingJson::JoinCardinalityLoosened { .. } => "join-cardinality-loosened",
            FindingJson::ColumnAdded { .. } => "column-added",
        }
    }

    /// The Node this Finding's downstream impact is actually reported
    /// against: the downstream Node reached for
    /// [`FindingJson::ColumnRemovedWithActiveReferences`] (the only Rule
    /// that currently reasons about a *separate* downstream Node), or the
    /// changed Node itself for every other Rule, which reason about the
    /// changed Node's own behavior rather than tracing further downstream.
    fn impacted_node(&self) -> &str {
        match self {
            FindingJson::ColumnRemovedWithActiveReferences { reached, .. } => reached,
            FindingJson::ColumnTypeNarrowed { node, .. }
            | FindingJson::JoinCardinalityLoosened { node, .. }
            | FindingJson::ColumnAdded { node, .. } => node,
        }
    }
}

impl From<&Finding> for FindingJson {
    fn from(finding: &Finding) -> Self {
        let severity = finding.severity.into();
        match &finding.detail {
            FindingDetail::ColumnRemovedWithActiveReferences {
                node,
                column,
                reached,
                reached_column,
            } => FindingJson::ColumnRemovedWithActiveReferences {
                severity,
                node: node.to_string(),
                column: column.to_string(),
                reached: reached.to_string(),
                reached_column: reached_column.to_string(),
            },
            FindingDetail::ColumnTypeNarrowed {
                node,
                column,
                from_type,
                to_type,
            } => FindingJson::ColumnTypeNarrowed {
                severity,
                node: node.to_string(),
                column: column.to_string(),
                from_type: from_type.clone(),
                to_type: to_type.clone(),
            },
            FindingDetail::JoinCardinalityLoosened {
                node,
                position,
                from_kind,
                to_kind,
            } => FindingJson::JoinCardinalityLoosened {
                severity,
                node: node.to_string(),
                position: *position,
                from_kind: join_kind_slug(*from_kind),
                to_kind: join_kind_slug(*to_kind),
            },
            FindingDetail::ColumnAdded { node, column } => FindingJson::ColumnAdded {
                severity,
                node: node.to_string(),
                column: column.to_string(),
            },
        }
    }
}

/// Maps a [`JoinKind`] to its stable JSON string, via an explicit,
/// compiler-checked match rather than its `Debug` representation -- a
/// `Debug`-derived string would silently change this stable output if a
/// variant were ever renamed for unrelated internal reasons, with no
/// compiler error to catch it (unlike this match, which fails to compile
/// if a variant is left unhandled).
fn join_kind_slug(kind: JoinKind) -> String {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
        JoinKind::Cross => "cross",
    }
    .to_string()
}

/// The ANSI escape sequence `BREAKING` labels are wrapped in when color is
/// enabled -- bold red.
const BREAKING_COLOR: &str = "\x1b[1;31m";
/// The ANSI escape sequence `WARN` labels are wrapped in when color is
/// enabled -- bold yellow.
const WARN_COLOR: &str = "\x1b[1;33m";
/// Resets any color started by [`BREAKING_COLOR`]/[`WARN_COLOR`].
const COLOR_RESET: &str = "\x1b[0m";

/// Wraps `text` in `color`, or returns it unchanged if `use_color` is
/// `false` -- the single point every color decision in this module goes
/// through, so no ANSI code can leak out when color is supposed to be off.
fn colorize(text: &str, color: &str, use_color: bool) -> String {
    if use_color {
        format!("{color}{text}{COLOR_RESET}")
    } else {
        text.to_string()
    }
}

/// Renders a [`Report`] as the three-part human-readable report: a
/// "Changed" section (each Node that actually changed, and precisely what
/// changed about it), a "Downstream impact" section (only Nodes actually
/// reached by a breaking or warning-level Finding, labeled `BREAKING` or
/// `WARN` with the specific reference and Rule that fired), and a summary
/// line. Every Node reference goes through `vocabulary` (e.g. "model" for
/// dbt), never zhao's own internal "Node"/"Origin" terms.
///
/// When `use_color` is `true`, `BREAKING`/`WARN` labels are wrapped in
/// ANSI color codes (red/yellow); when `false`, the output is plain text
/// with no escape sequences anywhere -- deciding *whether* color is
/// appropriate (a TTY, `--no-color`, `NO_COLOR`, known CI environments
/// like GitHub Actions that render ANSI without being a real TTY, ...) is
/// the caller's responsibility, not this function's.
///
/// ## Known limitation
///
/// `vocabulary.origin_term()` (e.g. "source" for dbt) is never actually
/// used here: neither [`Change`] nor [`FindingDetail`] can reference an
/// Origin today -- `diff()` only ever compares Nodes -- so there's
/// currently no path through this report that could render one. This
/// isn't a gap in this function specifically; it'll start mattering once
/// the diff engine itself gains the ability to detect an Origin-level
/// change (e.g. a source's declared schema changing).
pub fn render_text(report: &Report, vocabulary: &dyn AdapterVocabulary, use_color: bool) -> String {
    let mut out = String::new();
    let node_term = vocabulary.node_term();

    if let Some(warning) = &report.staleness_warning {
        out.push_str(&format!("warning: {warning}\n\n"));
    }

    if report.findings.is_empty() && report.changes.is_empty() {
        out.push_str("No changes detected.\n");
        return out;
    }

    out.push_str("Changed:\n");
    for (node, changes) in group_by_node(&report.changes, ChangeJson::node) {
        out.push_str(&format!("  {node_term} {node}:\n"));
        for change in changes {
            out.push_str(&format!("    {}\n", change.describe()));
        }
    }

    let impactful: Vec<&FindingJson> = report
        .findings
        .iter()
        .filter(|f| f.severity() != SeverityJson::Pass)
        .collect();
    if !impactful.is_empty() {
        out.push_str("\nDownstream impact:\n");
        for (node, findings) in group_by_node(&impactful, |f: &&FindingJson| f.impacted_node()) {
            out.push_str(&format!("  {node_term} {node}:\n"));
            for finding in findings {
                let label = match finding.severity() {
                    SeverityJson::Error => colorize("BREAKING", BREAKING_COLOR, use_color),
                    SeverityJson::Warn => colorize("WARN", WARN_COLOR, use_color),
                    SeverityJson::Pass => unreachable!("filtered out above"),
                };
                out.push_str(&format!(
                    "    [{label}] {} ({})\n",
                    describe_impact(finding, node_term),
                    finding.rule_name()
                ));
            }
        }
    }

    let models_changed = report
        .changes
        .iter()
        .map(ChangeJson::node)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let columns_changed = report
        .changes
        .iter()
        .filter(|c| c.is_column_change())
        .count();
    let breaking = report
        .findings
        .iter()
        .filter(|f| f.severity() == SeverityJson::Error)
        .count();
    let warning = report
        .findings
        .iter()
        .filter(|f| f.severity() == SeverityJson::Warn)
        .count();
    out.push_str(&format!(
        "\nSummary: {models_changed} {node_term}(s) changed, {columns_changed} column(s) \
         changed, {breaking} breaking, {warning} warning\n"
    ));

    if let Some(command) = &report.recommended_command {
        out.push_str(&format!("\nRecommended: {command}\n"));
    }

    if let Some(plan) = &report.defer_plan {
        out.push_str("\nDefer plan:\n");
        out.push_str(&format!("  Build: {}\n", plan.build.join(", ")));
        out.push_str(&format!(
            "  Defer (assumed available): {}\n",
            if plan.defer.is_empty() {
                "none".to_string()
            } else {
                plan.defer.join(", ")
            }
        ));
    }

    out
}

/// Groups `items` by a key derived from each one, preserving each group's
/// first-seen order (both across groups and within a group) rather than
/// sorting -- so the report's ordering follows the underlying `Change`/
/// `Finding` list's own (already-deterministic) order.
fn group_by_node<'a, T, F>(items: &'a [T], key: F) -> Vec<(&'a str, Vec<&'a T>)>
where
    F: Fn(&'a T) -> &'a str,
{
    let mut order: Vec<&'a str> = Vec::new();
    let mut groups: std::collections::HashMap<&'a str, Vec<&'a T>> =
        std::collections::HashMap::new();
    for item in items {
        let node = key(item);
        groups
            .entry(node)
            .or_insert_with(|| {
                order.push(node);
                Vec::new()
            })
            .push(item);
    }
    order
        .into_iter()
        .map(|node| {
            (
                node,
                groups.remove(node).expect("present for every ordered key"),
            )
        })
        .collect()
}

/// A one-line description of a Finding's impact, for the "Downstream
/// impact" section -- no Node name (the section already groups by it) and
/// no Severity label (the caller prefixes `[BREAKING]`/`[WARN]` itself).
fn describe_impact(finding: &FindingJson, node_term: &str) -> String {
    match finding {
        FindingJson::ColumnRemovedWithActiveReferences {
            node,
            column,
            reached_column,
            ..
        } => {
            format!(
                "{column} removed from {node_term} {node} breaks reference via {reached_column}"
            )
        }
        FindingJson::ColumnTypeNarrowed {
            column,
            from_type,
            to_type,
            ..
        } => {
            format!("{column} type narrowed from {from_type} to {to_type}")
        }
        FindingJson::JoinCardinalityLoosened {
            position,
            from_kind,
            to_kind,
            ..
        } => {
            format!("join at position {position} loosened from {from_kind} to {to_kind}")
        }
        FindingJson::ColumnAdded { column, .. } => {
            format!("{column} added")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::adapters::dbt::DbtVocabulary;
    use zhao_core::model::{JoinKind as CoreJoinKind, NodeId};

    fn all_finding_variants() -> Vec<Finding> {
        let node = NodeId::new("model.a");
        vec![
            Finding {
                severity: Severity::Error,
                detail: FindingDetail::ColumnRemovedWithActiveReferences {
                    node: node.clone(),
                    column: zhao_core::model::ColumnName::new("id"),
                    reached: NodeId::new("model.b"),
                    reached_column: zhao_core::model::ColumnName::new("a_id"),
                },
            },
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node: node.clone(),
                    column: zhao_core::model::ColumnName::new("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            },
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::JoinCardinalityLoosened {
                    node: node.clone(),
                    position: 0,
                    from_kind: CoreJoinKind::Inner,
                    to_kind: CoreJoinKind::Left,
                },
            },
            Finding {
                severity: Severity::Pass,
                detail: FindingDetail::ColumnAdded {
                    node,
                    column: zhao_core::model::ColumnName::new("new_col"),
                },
            },
        ]
    }

    /// `FindingJson::rule_name` is a hand-maintained mirror of the same
    /// enum's `#[serde(tag = "rule", rename_all = "kebab-case")]`
    /// derive -- this test is what keeps the two from silently drifting
    /// apart if a variant is ever renamed and only one side is updated.
    #[test]
    fn finding_json_rule_name_matches_its_serialized_json_tag() {
        for finding in &all_finding_variants() {
            let json = FindingJson::from(finding);
            let serialized: serde_json::Value =
                serde_json::to_value(&json).expect("should serialize");
            assert_eq!(
                serialized["rule"]
                    .as_str()
                    .expect("rule should be a string"),
                json.rule_name(),
                "rule_name() drifted from the derived JSON tag for {json:?}"
            );
        }
    }

    #[test]
    fn render_text_reports_no_changes_detected_when_nothing_changed() {
        let report = Report::new(&[], &[]);

        assert_eq!(
            render_text(&report, &DbtVocabulary, false),
            "No changes detected.\n"
        );
    }

    #[test]
    fn render_text_produces_the_three_part_report_using_the_adapters_vocabulary() {
        let changes = vec![
            Change::ColumnAdded {
                node: NodeId::new("model.a"),
                column: zhao_core::model::ColumnName::new("new_col"),
            },
            Change::ColumnRemoved {
                node: NodeId::new("model.a"),
                column: zhao_core::model::ColumnName::new("id"),
            },
        ];
        let findings = vec![Finding {
            severity: Severity::Error,
            detail: FindingDetail::ColumnRemovedWithActiveReferences {
                node: NodeId::new("model.a"),
                column: zhao_core::model::ColumnName::new("id"),
                reached: NodeId::new("model.b"),
                reached_column: zhao_core::model::ColumnName::new("a_id"),
            },
        }];
        let report = Report::new(&changes, &findings);

        let text = render_text(&report, &DbtVocabulary, false);

        // Uses dbt's vocabulary ("model"), never zhao's internal terms.
        assert!(text.contains("model model.a"), "{text}");
        assert!(!text.contains("Node "), "{text}");
        assert!(!text.contains("Origin "), "{text}");

        // Changed section: both changes on model.a, grouped under it.
        assert!(text.contains("Changed:\n  model model.a:\n"), "{text}");
        assert!(text.contains("+ column added: new_col"), "{text}");
        assert!(text.contains("- column removed: id"), "{text}");

        // Downstream impact: the reached model, not the changed one, with
        // the rule name.
        assert!(
            text.contains("Downstream impact:\n  model model.b:\n"),
            "{text}"
        );
        assert!(
            text.contains("[BREAKING]") && text.contains("column-removed-with-active-references"),
            "{text}"
        );

        // Summary counts.
        assert!(
            text.contains(
                "Summary: 1 model(s) changed, 2 column(s) changed, 1 breaking, 0 warning"
            ),
            "{text}"
        );
    }

    /// A `pass`-severity Finding (e.g. `column-added`) is informational,
    /// not impact -- it must not appear in "Downstream impact" at all,
    /// even though the Change it's attached to does appear in "Changed".
    #[test]
    fn render_text_excludes_pass_severity_findings_from_downstream_impact() {
        let changes = vec![Change::ColumnAdded {
            node: NodeId::new("model.a"),
            column: zhao_core::model::ColumnName::new("new_col"),
        }];
        let findings = vec![Finding {
            severity: Severity::Pass,
            detail: FindingDetail::ColumnAdded {
                node: NodeId::new("model.a"),
                column: zhao_core::model::ColumnName::new("new_col"),
            },
        }];
        let report = Report::new(&changes, &findings);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(text.contains("Changed:"), "{text}");
        assert!(
            !text.contains("Downstream impact:"),
            "a pass-severity finding must not produce a Downstream impact section: {text}"
        );
        assert!(text.contains("0 breaking, 0 warning"), "{text}");
    }

    /// With `use_color: false`, no ANSI escape byte appears anywhere in
    /// the output -- the property `--no-color`'s "byte-for-byte plain
    /// text" acceptance criterion ultimately rests on.
    #[test]
    fn render_text_with_use_color_false_contains_no_ansi_escapes() {
        let changes = vec![Change::ColumnRemoved {
            node: NodeId::new("model.a"),
            column: zhao_core::model::ColumnName::new("id"),
        }];
        let findings = vec![Finding {
            severity: Severity::Error,
            detail: FindingDetail::ColumnRemovedWithActiveReferences {
                node: NodeId::new("model.a"),
                column: zhao_core::model::ColumnName::new("id"),
                reached: NodeId::new("model.b"),
                reached_column: zhao_core::model::ColumnName::new("a_id"),
            },
        }];
        let report = Report::new(&changes, &findings);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(
            !text.contains('\x1b'),
            "no ANSI escape byte should appear when use_color is false: {text:?}"
        );
    }

    /// With `use_color: true`, `BREAKING`/`WARN` labels carry an ANSI
    /// escape somewhere in the output -- checked for *presence* only, not
    /// the exact byte sequence, so this doesn't snapshot-lock the specific
    /// color codes chosen.
    #[test]
    fn render_text_with_use_color_true_contains_ansi_escapes_for_breaking_and_warn() {
        let node = NodeId::new("model.a");
        let findings = vec![
            Finding {
                severity: Severity::Error,
                detail: FindingDetail::ColumnRemovedWithActiveReferences {
                    node: node.clone(),
                    column: zhao_core::model::ColumnName::new("id"),
                    reached: NodeId::new("model.b"),
                    reached_column: zhao_core::model::ColumnName::new("a_id"),
                },
            },
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node,
                    column: zhao_core::model::ColumnName::new("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            },
        ];
        let report = Report::new(&[], &findings);

        let text = render_text(&report, &DbtVocabulary, true);

        assert!(
            text.contains('\x1b'),
            "an ANSI escape byte should appear somewhere when use_color is true: {text:?}"
        );
        // Still contains the plain label text -- color wraps it, doesn't
        // replace it.
        assert!(text.contains("BREAKING"), "{text}");
        assert!(text.contains("WARN"), "{text}");
    }

    /// Acceptance criterion 1: the generated selector set exactly matches
    /// the Nodes listed in the Downstream impact section -- no more (a
    /// Node that only appears in "Changed", like `stg_orders` here via a
    /// pass-severity Finding, must be excluded), no less, and
    /// deduplicated (both Findings below share `stg_customers` as their
    /// impacted Node).
    #[test]
    fn with_recommended_command_includes_exactly_the_downstream_impact_nodes() {
        let findings = vec![
            Finding {
                severity: Severity::Error,
                detail: FindingDetail::ColumnRemovedWithActiveReferences {
                    node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                    column: zhao_core::model::ColumnName::new("id"),
                    reached: NodeId::new("model.zhao_dbt_test.dim_customers"),
                    reached_column: zhao_core::model::ColumnName::new("a_id"),
                },
            },
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                    column: zhao_core::model::ColumnName::new("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            },
            Finding {
                severity: Severity::Pass,
                detail: FindingDetail::ColumnAdded {
                    node: NodeId::new("model.zhao_dbt_test.stg_orders"),
                    column: zhao_core::model::ColumnName::new("new_col"),
                },
            },
        ];
        let report = Report::new(&[], &findings).with_recommended_command(&DbtVocabulary);

        assert_eq!(
            report.recommended_command.as_deref(),
            Some("dbt build --select dim_customers stg_customers"),
            "should include dim_customers (via the reached Finding) and stg_customers \
             (via the type-narrowed Finding on itself) exactly once each, and never \
             stg_orders (only a pass-severity Finding, not Downstream impact)"
        );
    }

    /// Regression test for the bug an earlier version of this method had:
    /// a Node reached only via the Baseline (e.g. one that no longer
    /// exists in the current state at all, for
    /// `ColumnRemovedWithActiveReferences`) must still be named in the
    /// recommended command -- this method no longer looks Nodes up
    /// against a `ParsedProject` at all, precisely so there's nothing to
    /// fail to resolve.
    #[test]
    fn with_recommended_command_includes_a_node_that_no_longer_exists_anywhere_but_its_id() {
        let findings = vec![Finding {
            severity: Severity::Error,
            detail: FindingDetail::ColumnRemovedWithActiveReferences {
                node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                column: zhao_core::model::ColumnName::new("id"),
                reached: NodeId::new("model.zhao_dbt_test.deleted_downstream_model"),
                reached_column: zhao_core::model::ColumnName::new("a_id"),
            },
        }];
        let report = Report::new(&[], &findings).with_recommended_command(&DbtVocabulary);

        assert_eq!(
            report.recommended_command.as_deref(),
            Some("dbt build --select deleted_downstream_model"),
        );
    }

    /// Acceptance criterion 2: a run with zero impacted Nodes produces no
    /// recommended command.
    #[test]
    fn with_recommended_command_is_none_when_nothing_is_impactful() {
        // No Findings at all.
        let report = Report::new(&[], &[]).with_recommended_command(&DbtVocabulary);
        assert_eq!(report.recommended_command, None);

        // A Finding exists, but it's pass-severity -- not Downstream
        // impact, so still nothing to recommend.
        let findings = vec![Finding {
            severity: Severity::Pass,
            detail: FindingDetail::ColumnAdded {
                node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                column: zhao_core::model::ColumnName::new("new_col"),
            },
        }];
        let report = Report::new(&[], &findings).with_recommended_command(&DbtVocabulary);
        assert_eq!(report.recommended_command, None);
    }

    /// `render_text` appends the recommended command as a final line when
    /// present, and omits it entirely when absent.
    #[test]
    fn render_text_appends_the_recommended_command_when_present() {
        let findings = vec![Finding {
            severity: Severity::Error,
            detail: FindingDetail::ColumnRemovedWithActiveReferences {
                node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                column: zhao_core::model::ColumnName::new("id"),
                reached: NodeId::new("model.zhao_dbt_test.stg_customers"),
                reached_column: zhao_core::model::ColumnName::new("id"),
            },
        }];
        let report = Report::new(&[], &findings)
            .with_recommended_command(&DbtVocabulary)
            .with_staleness_warning(false);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(
            text.contains("Recommended: dbt build --select stg_customers"),
            "{text}"
        );
    }

    #[test]
    fn render_text_omits_the_recommended_command_line_when_absent() {
        let report = Report::new(&[], &[]);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(!text.contains("Recommended:"), "{text}");
    }

    /// A minimal `ParsedProject` with one Node per `(id, name)` pair and
    /// the given `LineageEdge`s -- enough to exercise
    /// `Report::with_defer_plan`'s graph walk without needing a real
    /// compiled manifest.
    fn project_with_edges(edges: Vec<zhao_core::model::LineageEdge>) -> ParsedProject {
        ParsedProject {
            nodes: Vec::new(),
            origins: Vec::new(),
            edges,
        }
    }

    fn node_edge(upstream: &str, downstream: &str) -> zhao_core::model::LineageEdge {
        zhao_core::model::LineageEdge {
            upstream: Upstream::Node(NodeId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: None,
        }
    }

    /// Acceptance criterion 1: given a Change reaching a subset of Nodes,
    /// the plan correctly separates build (the impacted set) from defer
    /// (their upstream dependencies) -- including a Node reached only
    /// transitively (`model.zhao_dbt_test.raw_base`, two hops upstream of
    /// the single impacted Node), proving this is a real transitive
    /// closure, not just direct parents.
    #[test]
    fn with_defer_plan_separates_build_from_transitive_upstream_dependencies() {
        let current = project_with_edges(vec![
            node_edge(
                "model.zhao_dbt_test.stg_orders",
                "model.zhao_dbt_test.dim_customers",
            ),
            node_edge(
                "model.zhao_dbt_test.raw_base",
                "model.zhao_dbt_test.stg_orders",
            ),
        ]);
        let findings = vec![Finding {
            severity: Severity::Warn,
            detail: FindingDetail::ColumnTypeNarrowed {
                node: NodeId::new("model.zhao_dbt_test.dim_customers"),
                column: zhao_core::model::ColumnName::new("amount"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            },
        }];
        let report = Report::new(&[], &findings).with_defer_plan(&current, &DbtVocabulary);

        let plan = report.defer_plan.expect("plan should be present");
        assert_eq!(plan.build, vec!["dim_customers"]);
        assert_eq!(plan.defer, vec!["raw_base", "stg_orders"]);
    }

    /// A build Node whose only upstream dependency is an Origin (a
    /// source, not a Node) produces an empty (not absent) defer list --
    /// dbt never builds a source, so there's genuinely nothing to defer,
    /// but that's still meaningful information, not "no plan at all."
    #[test]
    fn with_defer_plan_defer_is_empty_not_absent_when_only_an_origin_is_upstream() {
        let current = project_with_edges(vec![zhao_core::model::LineageEdge {
            upstream: Upstream::Origin(zhao_core::model::OriginId::new("source.raw.customers")),
            downstream: NodeId::new("model.zhao_dbt_test.stg_customers"),
            column: None,
        }]);
        let findings = vec![Finding {
            severity: Severity::Warn,
            detail: FindingDetail::ColumnTypeNarrowed {
                node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                column: zhao_core::model::ColumnName::new("amount"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            },
        }];
        let report = Report::new(&[], &findings).with_defer_plan(&current, &DbtVocabulary);

        let plan = report.defer_plan.expect("plan should be present");
        assert_eq!(plan.build, vec!["stg_customers"]);
        assert!(plan.defer.is_empty());
    }

    /// A Node already in the build set is never also listed in defer,
    /// even if another build Node depends on it directly.
    #[test]
    fn with_defer_plan_never_defers_a_node_thats_also_being_built() {
        let current = project_with_edges(vec![node_edge(
            "model.zhao_dbt_test.stg_customers",
            "model.zhao_dbt_test.dim_customers",
        )]);
        let findings = vec![
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node: NodeId::new("model.zhao_dbt_test.stg_customers"),
                    column: zhao_core::model::ColumnName::new("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            },
            Finding {
                severity: Severity::Warn,
                detail: FindingDetail::ColumnTypeNarrowed {
                    node: NodeId::new("model.zhao_dbt_test.dim_customers"),
                    column: zhao_core::model::ColumnName::new("amount"),
                    from_type: "bigint".to_string(),
                    to_type: "int".to_string(),
                },
            },
        ];
        let report = Report::new(&[], &findings).with_defer_plan(&current, &DbtVocabulary);

        let plan = report.defer_plan.expect("plan should be present");
        assert!(!plan.defer.contains(&"stg_customers".to_string()));
    }

    /// A run with zero impacted Nodes produces no defer plan at all.
    #[test]
    fn with_defer_plan_is_none_when_nothing_is_impactful() {
        let current = project_with_edges(Vec::new());
        let report = Report::new(&[], &[]).with_defer_plan(&current, &DbtVocabulary);

        assert!(report.defer_plan.is_none());
    }

    #[test]
    fn render_text_appends_the_defer_plan_when_present() {
        let current = project_with_edges(vec![node_edge(
            "model.zhao_dbt_test.stg_orders",
            "model.zhao_dbt_test.dim_customers",
        )]);
        let findings = vec![Finding {
            severity: Severity::Warn,
            detail: FindingDetail::ColumnTypeNarrowed {
                node: NodeId::new("model.zhao_dbt_test.dim_customers"),
                column: zhao_core::model::ColumnName::new("amount"),
                from_type: "bigint".to_string(),
                to_type: "int".to_string(),
            },
        }];
        let report = Report::new(&[], &findings).with_defer_plan(&current, &DbtVocabulary);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(text.contains("Defer plan:"), "{text}");
        assert!(text.contains("Build: dim_customers"), "{text}");
        assert!(
            text.contains("Defer (assumed available): stg_orders"),
            "{text}"
        );
    }

    #[test]
    fn render_text_omits_the_defer_plan_section_when_absent() {
        let report = Report::new(&[], &[]);

        let text = render_text(&report, &DbtVocabulary, false);

        assert!(!text.contains("Defer plan:"), "{text}");
    }
}
