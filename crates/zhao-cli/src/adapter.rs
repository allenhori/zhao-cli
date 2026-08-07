//! Resolves which Transformation Tool Adapter applies to a project
//! directory -- auto-detected by project marker file first, falling back
//! to `zhao.yml`'s `tool:` key only when detection alone can't produce a
//! single answer (see `zhao_core::adapters::resolve_tool_name`, which
//! implements the actual resolution order this module just wraps).
//!
//! [`ResolvedAdapter`] is the single type every zhao-cli call site
//! (`baseline.rs`, `engine.rs`, `lineage.rs`) calls `parse`/`compile`/
//! `deps`/`query_executor` through, instead of hardcoding `DbtAdapter` by
//! name the way they used to. Only one variant exists today -- dbt is the
//! only real adapter -- so adding a second means one new variant here plus
//! one new arm per method, not touching any of those call sites at all.

use std::collections::HashMap;
use std::path::Path;

use zhao_core::adapters::dbt::{DbtAdapter, DbtAdapterError, DbtCommandOutput};
use zhao_core::adapters::warehouse::{QueryExecutor, RelationIdentity};
use zhao_core::adapters::{AdapterVocabulary, ToolResolutionError, TransformationToolAdapter};
use zhao_core::model::ParsedProject;

/// The Transformation Tool Adapter resolved for a project directory --
/// see the module doc comment.
pub enum ResolvedAdapter {
    /// dbt, the only real adapter today.
    Dbt(DbtAdapter),
}

impl ResolvedAdapter {
    /// Resolves the adapter for `project_dir`: auto-detection first, then
    /// `configured_tool` (`zhao.yml`'s `tool:` key, see
    /// [`zhao_core::config::Config::tool`]) only as a fallback for the
    /// undetectable case -- see
    /// [`zhao_core::adapters::resolve_tool_name`] for the exact order.
    pub fn resolve(
        project_dir: &Path,
        configured_tool: Option<&str>,
    ) -> Result<ResolvedAdapter, ToolResolutionError> {
        let name = zhao_core::adapters::resolve_tool_name(project_dir, configured_tool)?;
        match name {
            "dbt" => Ok(ResolvedAdapter::Dbt(DbtAdapter)),
            other => unreachable!(
                "resolve_tool_name only ever returns a registered adapter's own name; \
                 {other:?} isn't one this module knows how to construct"
            ),
        }
    }

    /// See [`TransformationToolAdapter::vocabulary`].
    pub fn vocabulary(&self) -> &dyn AdapterVocabulary {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.vocabulary(),
        }
    }

    /// See [`TransformationToolAdapter::parse`].
    pub fn parse(&self, path: &Path) -> Result<ParsedProject, DbtAdapterError> {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.parse(path),
        }
    }

    /// See [`TransformationToolAdapter::compile`].
    pub fn compile(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.compile(project_dir, command, extra_args),
        }
    }

    /// See [`TransformationToolAdapter::deps`].
    pub fn deps(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.deps(project_dir, command, extra_args),
        }
    }

    /// See [`TransformationToolAdapter::query_executor`].
    pub fn query_executor<'a>(
        &self,
        project_dir: &'a Path,
        command: &'a str,
        extra_args: &'a [String],
    ) -> Box<dyn QueryExecutor + 'a> {
        match self {
            ResolvedAdapter::Dbt(adapter) => {
                adapter.query_executor(project_dir, command, extra_args)
            }
        }
    }

    /// Reads a compiled manifest's declared warehouse -- see
    /// [`DbtAdapter::adapter_type`].
    ///
    /// Deliberately *not* part of [`TransformationToolAdapter`] itself:
    /// this issue's scope is detection plus refresh/deps/query-executor,
    /// not generalizing every dbt-manifest-specific metadata read a
    /// hypothetical second adapter's own compiled-artifact format might
    /// not even have an equivalent of. Kept here, on the resolved
    /// adapter, purely so `engine.rs`'s `--check-relations` path goes
    /// through `ResolvedAdapter` rather than hardcoding `DbtAdapter`
    /// directly for this one read.
    pub fn adapter_type(&self, manifest_path: &Path) -> Result<Option<String>, DbtAdapterError> {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.adapter_type(manifest_path),
        }
    }

    /// Reads a compiled manifest's per-Node relation identities -- see
    /// [`DbtAdapter::relation_identities`]. Same "not on the shared trait
    /// yet" reasoning as [`Self::adapter_type`].
    pub fn relation_identities(
        &self,
        manifest_path: &Path,
    ) -> Result<HashMap<String, RelationIdentity>, DbtAdapterError> {
        match self {
            ResolvedAdapter::Dbt(adapter) => adapter.relation_identities(manifest_path),
        }
    }
}
