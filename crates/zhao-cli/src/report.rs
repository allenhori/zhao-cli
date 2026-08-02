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
use zhao_core::rules::{Finding, RuleId, Severity};

/// The full JSON payload for a `zhao check` run.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Every Change detected between the Baseline and the current state.
    pub changes: Vec<ChangeJson>,
    /// Every Rule that fired against those Changes.
    pub findings: Vec<FindingJson>,
}

impl Report {
    /// Builds a [`Report`] from the engine's own `Change`/`Finding` output.
    pub fn new(changes: &[Change], findings: &[Finding]) -> Self {
        Self {
            changes: changes.iter().map(ChangeJson::from).collect(),
            findings: findings.iter().map(FindingJson::from).collect(),
        }
    }

    /// Whether this run's Findings should fail the CI gate: any Finding
    /// at [`Severity::Error`].
    pub fn is_breaking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == SeverityJson::Error)
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
#[derive(Debug, Serialize)]
pub struct FindingJson {
    rule: String,
    severity: SeverityJson,
    node: String,
    column: String,
    reached: String,
    reached_column: String,
}

impl From<&Finding> for FindingJson {
    fn from(finding: &Finding) -> Self {
        Self {
            rule: rule_id_slug(finding.rule).to_string(),
            severity: finding.severity.into(),
            node: finding.node.to_string(),
            column: finding.column.to_string(),
            reached: finding.reached.to_string(),
            reached_column: finding.reached_column.to_string(),
        }
    }
}

fn rule_id_slug(rule: RuleId) -> &'static str {
    match rule {
        RuleId::ColumnRemovedWithActiveReferences => "column-removed-with-active-references",
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
    if report.findings.is_empty() && report.changes.is_empty() {
        return "No changes detected.\n".to_string();
    }

    let mut out = String::new();
    for finding in &report.findings {
        out.push_str(&format!(
            "[{:?}] {} removed from {} breaks {} (referenced via {})\n",
            finding.severity, finding.column, finding.node, finding.reached, finding.reached_column
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
