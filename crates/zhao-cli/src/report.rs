//! Rendering a `zhao check` run's results as JSON or brief text.
//!
//! zhao-core's own types ([`zhao_core::diff::Change`],
//! [`zhao_core::rules::Finding`]) intentionally carry no serialization
//! derives -- they're the engine's internal vocabulary, not a wire format
//! commitment. This module owns the JSON shape as its own, separate
//! concern, converting from the engine's types rather than exposing them
//! directly.

use serde::Serialize;
use zhao_core::diff::Change;
use zhao_core::model::JoinKind;
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
}

impl Report {
    /// Builds a [`Report`] from the engine's own `Change`/`Finding` output.
    /// No staleness warning is set -- chain [`Report::with_staleness_warning`]
    /// to add one.
    pub fn new(changes: &[Change], findings: &[Finding]) -> Self {
        Self {
            changes: changes.iter().map(ChangeJson::from).collect(),
            findings: findings.iter().map(FindingJson::from).collect(),
            staleness_warning: None,
        }
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

    /// Whether this run's Findings should fail the CI gate: any Finding
    /// at [`Severity::Error`]. A staleness warning never contributes here,
    /// under any Preset.
    pub fn is_breaking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity() == SeverityJson::Error)
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

/// Renders a [`Report`] as brief, human-readable text.
///
/// This is a placeholder pending the full Changed/Downstream impact
/// report (a later, dedicated ticket) -- just enough to make `zhao
/// check`'s default (non-JSON) output usable today.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();

    if let Some(warning) = &report.staleness_warning {
        out.push_str(&format!("warning: {warning}\n"));
    }

    if report.findings.is_empty() && report.changes.is_empty() {
        out.push_str("No changes detected.\n");
        return out;
    }

    for finding in &report.findings {
        out.push_str(&format!(
            "[{:?}] {}\n",
            finding.severity(),
            describe(finding)
        ));
    }
    if report.findings.is_empty() {
        out.push_str(&format!(
            "{} change(s) detected, none breaking.\n",
            report.changes.len()
        ));
    }
    out
}

fn describe(finding: &FindingJson) -> String {
    match finding {
        FindingJson::ColumnRemovedWithActiveReferences {
            node,
            column,
            reached,
            reached_column,
            ..
        } => {
            format!(
                "{column} removed from {node} breaks {reached} (referenced via {reached_column})"
            )
        }
        FindingJson::ColumnTypeNarrowed {
            node,
            column,
            from_type,
            to_type,
            ..
        } => {
            format!("{node}.{column} type narrowed from {from_type} to {to_type}")
        }
        FindingJson::JoinCardinalityLoosened {
            node,
            position,
            from_kind,
            to_kind,
            ..
        } => {
            format!("{node}'s join at position {position} loosened from {from_kind} to {to_kind}")
        }
        FindingJson::ColumnAdded { node, column, .. } => {
            format!("{column} added to {node}")
        }
    }
}
