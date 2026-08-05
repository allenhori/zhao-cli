//! Format-agnostic domain model and analysis engine for zhao.
//!
//! This crate owns zhao's neutral vocabulary (Nodes, Origins, Lineage Edges,
//! Changes) along with the trait boundaries that transformation-tool
//! adapters (dbt first) and warehouse adapters (Snowflake, Databricks,
//! BigQuery, ...) implement. Nothing in this crate knows about any specific
//! transformation tool, warehouse, or the command-line interface built on
//! top of it -- see the `zhao-cli` crate for that.
//!
//! See `ARCHITECTURE.md` at the repository root for the intended module
//! layout as functionality lands here.

pub mod adapters;
pub mod config;
pub mod diff;
pub mod git;
pub mod lineage;
pub mod model;
pub mod rules;

/// Returns this crate's version, as declared in `Cargo.toml`.
///
/// Exists so that consumers (starting with `zhao-cli`) can report the exact
/// `zhao-core` version in diagnostics without duplicating it.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks the *shape* of the returned version (three numeric,
    /// dot-separated components) rather than re-deriving the expected value
    /// with the same macro the implementation uses -- a test that just
    /// compared `version()` to another `env!("CARGO_PKG_VERSION")` would
    /// pass by construction and could never catch a real regression.
    #[test]
    fn version_is_a_three_part_numeric_string() {
        let v = version();
        let parts: Vec<&str> = v.split('.').collect();

        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {v:?}");
        for part in &parts {
            assert!(
                !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
                "non-numeric version component in {v:?}"
            );
        }
    }
}
