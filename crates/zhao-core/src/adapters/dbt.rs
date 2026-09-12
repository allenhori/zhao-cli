//! The dbt [`TransformationToolAdapter`] implementation: reads a compiled
//! `manifest.json` and produces zhao's internal [`ParsedProject`].
//!
//! dbt's manifest only lists *documented* columns (whatever a project's
//! `schema.yml` happens to describe), not a model's actual output schema --
//! so this adapter resolves the real schema, and column-level lineage,
//! itself by parsing each model's `compiled_code` with a SQL parser and
//! tracing column references back through any CTEs to the Nodes/Origins
//! they ultimately come from.
//!
//! ## Known limitations
//!
//! Column-level resolution handles the common shape of dbt-compiled SQL:
//! a chain of CTEs feeding a final `SELECT`, `SELECT *` passthrough, plain
//! and qualified column references, and simple aliasing. It also resolves
//! `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT` (each arm resolved
//! independently, then merged column-by-column -- see
//! `merge_set_operation_arms`) and an inline subquery directly in a `FROM`
//! clause (resolved as a nested scope, the same machinery a `WITH`-defined
//! CTE already uses -- see `resolve_query_in_scope`). It deliberately does
//! not attempt to resolve columns through a table-valued function in
//! `FROM` or a window function -- those cases fall back to an unresolved
//! (but still node-level-tracked) dependency rather than a guessed column
//! mapping. Getting a column mapping wrong silently would be worse than
//! not having one. Likewise, an Origin's real columns are never known from
//! `manifest.json` alone (dbt's manifest doesn't carry a source's actual
//! schema), so a wildcard that would need to enumerate an Origin's columns
//! can't be expanded from the manifest alone -- only identity ("this
//! column, whatever it's called, passes through unchanged") relationships
//! to an Origin are tracked, unless a sibling `catalog.json` (read by this
//! adapter's private `read_catalog`) is also available.
//!
//! A calculated column (a function call, `CAST`, arithmetic, `CASE`, or one
//! of `EXTRACT`/`CEIL`/`FLOOR`/`POSITION`/`SUBSTRING`/`TRIM`/`OVERLAY` --
//! sqlparser gives those their own dedicated `Expr` variants rather than
//! folding them into a generic function call, so they're walked
//! explicitly rather than falling out of the function-call case) is
//! *not* automatically unresolved, though: every plain (optionally
//! qualified) column identifier that expression structurally references --
//! `coalesce(x.y, 0)`, `x.a + x.b`, `case when x.a > 0 then x.b else x.c
//! end`, `extract(year from x.d)`, and arbitrary nesting of these -- is
//! collected and traced, so a
//! calculated column can resolve to *several* upstream columns, not just
//! one (see `collect_expr_sources`). This is a structural walk, not a
//! guess: every identifier found either resolves cleanly through the
//! surrounding `FROM` scope or it doesn't (e.g. it's ambiguous among
//! several relations in scope), and only the ones that do are reported --
//! there's no ranking of "which one is really the source" involved. A
//! calculated column's rendered SQL (re-generated from the parsed
//! expression, so not necessarily byte-identical to the original source)
//! is also recorded on [`crate::model::Column::expression`], `None` for a
//! plain passthrough/rename of a single identifier. Resolution is
//! CTE-aware in both directions: a reference to an earlier CTE's own
//! calculated column (`SELECT cte1.total AS my_column FROM cte1`) carries
//! forward *that* column's already-resolved sources, however many CTE
//! hops away it was actually computed -- the final model's column isn't
//! misattributed to the trivial passthrough reference that happens to sit
//! in the outermost `SELECT`.
//!
//! Struct/nested-field access -- Databricks/Spark, BigQuery, and DuckDB's
//! `STRUCT` dot notation (`payload.user_id`, qualified or not), Snowflake's
//! and Databricks' semi-structured `VARIANT` colon access (`payload:user_id`),
//! and array/map subscript access mixed with either (`events[0].event_type`,
//! `m['key']`) -- all resolve to their base column, the same "trace what's
//! structurally certain, don't guess deeper" trade-off as a calculated
//! column above: the struct/variant/array's own internal shape is never
//! modeled, only which base column a nested reference ultimately reads
//! from. See `collect_expr_sources_into`'s `CompoundIdentifier`,
//! `JsonAccess`, and `CompoundFieldAccess` arms.
//!
//! A `STRUCT`-typed column's own internal field *shape* (as opposed to
//! lineage through it, above) is a separate concern this adapter also
//! handles, for schema/type-evolution detection rather than
//! column-lineage tracing: `extract_struct_shape` (private -- not part of
//! this crate's public API) recognizes a column's immediate defining
//! expression being a `CAST(... AS STRUCT<...>)`, a `STRUCT(...)`
//! constructor, or a `named_struct(...)` call that names every field
//! explicitly, and records that shape on
//! [`crate::model::Column::struct_fields`]. One level deep only (a
//! nested field that's itself a `STRUCT`, an array-of-structs' element
//! shape, and a map's value-type evolution are all out of scope). A
//! shape *is* propagated forward across a CTE hop or a plain
//! passthrough/rename within the same model (`propagate_struct_shape`),
//! the same way lineage `sources` already are -- but not through a
//! wildcard expansion, since a wildcard's only ever working from an
//! upstream Node's/Origin's bare column *names*, never full `Column`
//! detail (see `expand_wildcard_of`). See
//! `ResolvedColumn::struct_fields`'s doc comment (also private) for
//! exactly what does and doesn't carry a shape forward.

use super::warehouse::{QueryExecutor, RELATION_EXISTS_MACRO, RelationIdentity};
use super::{AdapterVocabulary, TransformationToolAdapter};
use crate::model::{
    Column, ColumnLineage, ColumnName, JoinKind, LineageEdge, Materialization, Node, NodeId,
    Origin, OriginId, ParsedProject, StructField, Upstream,
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field as ParquetField;
use serde::Deserialize;
use sqlparser::ast::{
    AccessExpr, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select,
    SelectItem, SetExpr, Statement, Subscript, TableFactor, TableWithJoins, Value,
};
use sqlparser::dialect::{
    BigQueryDialect, DatabricksDialect, Dialect, DuckDbDialect, GenericDialect, MySqlDialect,
    PostgreSqlDialect, RedshiftSqlDialect, SnowflakeDialect, SparkSqlDialect,
};
use sqlparser::parser::Parser as SqlParser;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// The dbt implementation of [`TransformationToolAdapter`].
#[derive(Debug, Default, Clone, Copy)]
pub struct DbtAdapter;

/// The dbt [`AdapterVocabulary`]: Node → "model", Origin → "source".
#[derive(Debug, Default, Clone, Copy)]
pub struct DbtVocabulary;

impl AdapterVocabulary for DbtVocabulary {
    fn node_term(&self) -> &'static str {
        "model"
    }

    fn origin_term(&self) -> &'static str {
        "source"
    }

    fn node_display_name(&self, node_id: &str) -> String {
        // A dbt `unique_id` is always shaped `<resource_type>.<package>.<name>`
        // (dbt itself constructs it that way; model names can't contain
        // `.`), so the bare, selectable name is just the last segment --
        // no need to look up a full `Node`, which may not even exist
        // (e.g. a Node reached only via the Baseline that no longer
        // exists in the current state).
        node_id.rsplit('.').next().unwrap_or(node_id).to_string()
    }
}

/// A successful `dbt compile`/`dbt deps` run's captured stdout/stderr --
/// see [`DbtAdapter::compile`]/[`DbtAdapter::deps`]. Discarded before
/// issue #36; now returned so a caller can route it into the daily run
/// log for post-hoc inspection, the same way a *failing* run's output
/// is already carried on [`DbtAdapterError::CompileFailed`]/
/// [`DbtAdapterError::DepsFailed`].
#[derive(Debug, Clone, Default)]
pub struct DbtCommandOutput {
    /// The subcommand's captured stdout.
    pub stdout: String,
    /// The subcommand's captured stderr.
    pub stderr: String,
}

/// Everything that can go wrong while an adapter reads and parses a dbt
/// project's compiled manifest.
#[derive(Debug, thiserror::Error)]
pub enum DbtAdapterError {
    /// The manifest file couldn't be read from disk.
    #[error("could not read manifest at {path}: {source}")]
    Io {
        /// The path that couldn't be read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest's contents weren't valid dbt manifest JSON.
    #[error("could not parse manifest at {path} as a dbt manifest: {source}")]
    InvalidManifest {
        /// The path whose contents failed to parse.
        path: String,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The configured `dbt` command couldn't be run at all -- most likely
    /// dbt isn't installed, or isn't on `PATH`.
    #[error("could not run {command:?} -- is dbt installed and on PATH? ({source})")]
    CommandNotFound {
        /// The command that couldn't be run (ordinarily just `"dbt"`).
        command: String,
        /// The underlying I/O error from trying to spawn it.
        #[source]
        source: std::io::Error,
    },
    /// `dbt compile` ran but exited with a failure.
    ///
    /// Carries both `stdout` and `stderr`, not just the latter: dbt logs
    /// most of its actual error detail to stdout as part of its normal
    /// logging output, with stderr often near-empty even on a real
    /// failure (reserved for something more fundamentally fatal, like a
    /// crash before dbt's own logging even starts) -- surfacing stderr
    /// alone routinely hides the real reason a compile failed. The
    /// `Display` impl always inserts its own `\n` between the two
    /// (rather than concatenating them directly) -- captured stdout
    /// isn't guaranteed to end in a trailing newline (e.g. a truncated
    /// or killed process), so relying on that would risk fusing stdout's
    /// last line into stderr's first.
    #[error("dbt compile failed in {project_dir}:\n{stdout}\n{stderr}")]
    CompileFailed {
        /// The project directory `dbt compile` was run in.
        project_dir: String,
        /// `dbt compile`'s captured stdout -- where dbt's own logging
        /// (including most real error detail) actually goes.
        stdout: String,
        /// `dbt compile`'s captured stderr.
        stderr: String,
    },
    /// `dbt deps` ran but exited with a failure. See [`Self::CompileFailed`]
    /// for why both `stdout` and `stderr` are carried, and why the
    /// `Display` impl inserts its own separating `\n`.
    #[error("dbt deps failed in {project_dir}:\n{stdout}\n{stderr}")]
    DepsFailed {
        /// The project directory `dbt deps` was run in.
        project_dir: String,
        /// `dbt deps`'s captured stdout -- where dbt's own logging
        /// (including most real error detail) actually goes.
        stdout: String,
        /// `dbt deps`'s captured stderr.
        stderr: String,
    },
    /// `dbt show` ran but exited with a failure -- a compile error in the
    /// target, an unresolvable selector, a warehouse permissions error,
    /// etc. See [`Self::CompileFailed`] for why both `stdout` and
    /// `stderr` are carried, and why the `Display` impl inserts its own
    /// separating `\n`.
    #[error("dbt show failed for {target} in {project_dir}:\n{stdout}\n{stderr}")]
    ShowFailed {
        /// The project directory `dbt show` was run in.
        project_dir: String,
        /// The selector `zhao show` was asked to preview.
        target: String,
        /// `dbt show`'s captured stdout -- where dbt's own logging
        /// (including most real error detail) actually goes.
        stdout: String,
        /// `dbt show`'s captured stderr.
        stderr: String,
    },
}

impl TransformationToolAdapter for DbtAdapter {
    type Error = DbtAdapterError;
    type CommandOutput = DbtCommandOutput;

    fn parse(&self, path: &Path) -> Result<ParsedProject, Self::Error> {
        // Unconditionally uses a sibling `catalog.json` when one's
        // available (see `read_catalog`) -- absent, unreadable, or
        // unparseable all silently degrade to an empty catalog, which
        // makes every catalog-backed lookup a no-op, never an error. A
        // two-sided comparison (`zhao check`/`zhao diff`) must not call
        // this directly -- see `Self::parse_for_comparison`, which this
        // just delegates to with catalog usage always on.
        self.parse_for_comparison(path, true)
    }

    fn vocabulary(&self) -> &dyn AdapterVocabulary {
        &DbtVocabulary
    }

    /// Runs `dbt compile` in `project_dir`, so its `target/manifest.json`
    /// reflects the project's current compiled state.
    ///
    /// `command` is the executable to invoke -- ordinarily just `"dbt"`,
    /// resolved via `PATH` -- exposed as a parameter (rather than
    /// hardcoded) so tests can point it at a stub script instead of
    /// depending on whether a real `dbt` happens to be installed wherever
    /// the tests run. `extra_args` are appended verbatim after `compile`
    /// (e.g. `--target`, `--vars`) -- zhao never interprets or validates
    /// these, dbt does -- except that, when `command` reports itself as
    /// dbt Fusion (via its own `--version` banner), `--write-index
    /// --static-analysis strict` are appended too, unless `extra_args`
    /// already configures `--write-index` itself. Without this, Fusion
    /// never writes `target/index/dbt.node_columns.parquet` at all --
    /// every `--compile` would silently keep reading whatever stale
    /// index happened to already be on disk, or none. Never added for
    /// dbt-core, which doesn't recognize either flag and would fail
    /// outright.
    fn compile(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        let effective_args = with_fusion_index_flags(project_dir, command, extra_args);
        let output = run_dbt_subcommand(command, "compile", project_dir, &effective_args)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(DbtAdapterError::CompileFailed {
                project_dir: project_dir.display().to_string(),
                stdout,
                stderr,
            });
        }
        Ok(DbtCommandOutput { stdout, stderr })
    }

    /// Runs `dbt deps` in `project_dir`, installing any package
    /// dependencies declared in `packages.yml` (or `dependencies.yml`)
    /// before a subsequent [`TransformationToolAdapter::compile`] -- needed
    /// the first time a project is compiled somewhere its packages have
    /// never been installed (e.g. a freshly checked-out git worktree),
    /// since `dbt compile` fails if a `ref()`/macro from an unresolved
    /// package is used. See [`Self::compile`] for `command`/`extra_args`.
    fn deps(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        let output = run_dbt_subcommand(command, "deps", project_dir, extra_args)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(DbtAdapterError::DepsFailed {
                project_dir: project_dir.display().to_string(),
                stdout,
                stderr,
            });
        }
        Ok(DbtCommandOutput { stdout, stderr })
    }

    /// Builds a [`DbtQueryExecutor`] bound to `project_dir`/`command`/
    /// `extra_args`, for `--check-relations`'s live existence checks via
    /// `dbt run-operation`.
    fn query_executor<'a>(
        &self,
        project_dir: &'a Path,
        command: &'a str,
        extra_args: &'a [String],
    ) -> Box<dyn QueryExecutor + 'a> {
        Box::new(DbtQueryExecutor {
            project_dir,
            dbt_command: command,
            extra_args,
        })
    }
}

impl super::AdapterDetector for DbtAdapter {
    fn tool_name(&self) -> &'static str {
        "dbt"
    }

    /// A dbt project always has a `dbt_project.yml` at its root -- the
    /// real `dbt` CLI itself requires one to run at all, so this is the
    /// same marker dbt's own bootstrapping already relies on, not a
    /// zhao-invented convention.
    fn detect(&self, project_dir: &Path) -> bool {
        project_dir.join("dbt_project.yml").is_file()
    }
}

impl DbtAdapter {
    /// Reads a compiled manifest's `metadata.adapter_type` -- dbt's own
    /// name for whichever warehouse it was compiled against (e.g.
    /// `"snowflake"`), the value
    /// [`crate::adapters::warehouse::resolve`] matches against. `None`
    /// when the manifest doesn't record one (an unusually old or
    /// nonstandard manifest) -- callers should treat that the same as an
    /// unsupported warehouse, not an error.
    pub fn adapter_type(&self, path: &Path) -> Result<Option<String>, DbtAdapterError> {
        Ok(read_manifest(path)?.metadata.adapter_type)
    }

    /// Reads every model's fully-qualified relation identity
    /// (`database`/`schema`/`identifier`) from a compiled manifest,
    /// keyed by `NodeId` string -- what
    /// [`crate::adapters::warehouse::WarehouseAdapter::relation_exists`]
    /// needs to check a given Node against a live target. A model
    /// missing a usable qualified name (see `RawNode`'s doc comment) is
    /// simply absent from the result rather than failing the whole
    /// lookup.
    pub fn relation_identities(
        &self,
        path: &Path,
    ) -> Result<HashMap<String, RelationIdentity>, DbtAdapterError> {
        let manifest = read_manifest(path)?;
        Ok(manifest
            .nodes
            .values()
            .filter(|node| node.resource_type == "model")
            .filter_map(|node| {
                let (database, schema, identifier) = node.qualified_name()?;
                Some((
                    node.unique_id.clone(),
                    RelationIdentity {
                        database: Some(database),
                        schema: Some(schema),
                        identifier,
                    },
                ))
            })
            .collect())
    }

    /// Whether a sibling `catalog.json` next to `manifest_path` is both
    /// present and would actually contribute at least one relation's real
    /// column list -- the same "usable" bar this adapter's private
    /// `read_catalog` itself applies (a missing, unreadable, unparseable,
    /// or entirely-empty `catalog.json` all count as unavailable here).
    /// Exposed so a
    /// two-sided comparison (see [`Self::parse_for_comparison`]) can
    /// check both sides' availability *before* deciding whether either
    /// side actually parses with catalog-backed wildcard expansion turned
    /// on.
    pub fn catalog_available(&self, manifest_path: &Path) -> bool {
        !read_catalog(manifest_path).is_empty()
    }

    /// Like [`TransformationToolAdapter::parse`], but with catalog-backed
    /// wildcard expansion forced off when `use_catalog` is `false` --
    /// regardless of whether a real `catalog.json` happens to be sitting
    /// next to `path`.
    ///
    /// This exists for `zhao check`/`zhao diff`'s Baseline-vs-current
    /// comparison specifically: a Baseline is compiled in a throwaway git
    /// worktree that only ever runs `dbt compile`, never `dbt docs
    /// generate`, so it essentially never has a `catalog.json` -- while
    /// the current state, read straight from the real project's own
    /// `target/`, picks up whatever `catalog.json` a user's `dbt docs
    /// generate` run (for any *other* reason) happened to leave there. If
    /// each side's `parse` were left free to independently decide its own
    /// catalog usage, a model reading `SELECT *` from a source would get
    /// real, catalog-expanded columns on the current side and zero
    /// columns on the Baseline side -- reported as a false-positive
    /// column addition on *every single run*, for as long as that
    /// `catalog.json` exists. A diff must never be asymmetric in what
    /// information it had available on each side, so the caller (see
    /// `zhao-cli`'s `engine.rs`/`baseline.rs`) checks
    /// [`Self::catalog_available`] on *both* sides first and only passes
    /// `use_catalog: true` here when both agree; otherwise both sides
    /// call this with `false`, falling back to today's plain `Opaque`
    /// wildcard behavior symmetrically.
    ///
    /// `zhao lineage` (a standalone, one-sided query with no comparison
    /// to keep symmetric) is unaffected -- it still calls plain `parse`,
    /// which always uses a real `catalog.json` when one is available.
    pub fn parse_for_comparison(
        &self,
        path: &Path,
        use_catalog: bool,
    ) -> Result<ParsedProject, DbtAdapterError> {
        let manifest = read_manifest(path)?;
        let catalog = if use_catalog {
            read_catalog(path)
        } else {
            CatalogSchemas::new()
        };
        Ok(build_parsed_project(&manifest, &catalog))
    }

    /// Runs `dbt show --select <target> --limit <limit> [--output json]`
    /// in `project_dir`, previewing that target's query results. See
    /// `crate::show` (in `zhao-cli`) for row-limit resolution and the
    /// cross-engine JSON normalization built on top of this -- this
    /// method itself is a thin, engine-agnostic passthrough, same
    /// division of responsibility as [`Self::compile`]/[`Self::deps`].
    ///
    /// Not part of [`TransformationToolAdapter`] itself, same "dbt-
    /// specific, not generalized to a hypothetical second adapter yet"
    /// reasoning as [`Self::adapter_type`]/[`Self::catalog_available`].
    pub fn show(
        &self,
        project_dir: &Path,
        command: &str,
        target: &str,
        limit: u32,
        output_json: bool,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        let mut args = vec![
            "--select".to_string(),
            target.to_string(),
            "--limit".to_string(),
            limit.to_string(),
        ];
        if output_json {
            args.push("--output".to_string());
            args.push("json".to_string());
        }
        args.extend_from_slice(extra_args);

        let output = run_dbt_subcommand(command, "show", project_dir, &args)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(DbtAdapterError::ShowFailed {
                project_dir: project_dir.display().to_string(),
                target: target.to_string(),
                stdout,
                stderr,
            });
        }
        Ok(DbtCommandOutput { stdout, stderr })
    }
}

/// Reads and parses a manifest at `path` -- the shared first step of
/// [`TransformationToolAdapter::parse`], [`DbtAdapter::adapter_type`],
/// and [`DbtAdapter::relation_identities`].
fn read_manifest(path: &Path) -> Result<RawManifest, DbtAdapterError> {
    let raw = fs::read_to_string(path).map_err(|source| DbtAdapterError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| DbtAdapterError::InvalidManifest {
        path: path.display().to_string(),
        source,
    })
}

/// Every model's or source's real column names, as introspected from the
/// warehouse (dbt-core's `catalog.json`) or statically inferred (dbt
/// Fusion's `dbt.node_columns.parquet` -- see [`read_node_columns_parquet`]),
/// keyed by `unique_id` -- the same key `manifest.json`'s own
/// `nodes`/`sources` maps use, and so directly comparable to an
/// [`Upstream::Node`]'s or [`Upstream::Origin`]'s own id. Column order
/// matches each source's own recorded ordering (`catalog.json`'s `index`,
/// see [`RawCatalogColumn`]; the parquet index's `column_index`).
///
/// Unlike `manifest.json`, whose columns are only ever whatever a project's
/// `schema.yml` happens to document (see the module-level doc comment),
/// this is meant to be the real, complete output schema -- which is what
/// makes it useful for expanding a `SELECT *` read directly from a source
/// (an Origin's real columns are otherwise never knowable at all) and for
/// disambiguating an otherwise-ambiguous duplicate `FROM` alias (see
/// [`LocalSchema::AmbiguousAlias`]). In practice `catalog.json` delivers on
/// that fully (a live warehouse introspection covers every relation), while
/// the Fusion parquet index currently only reliably covers seeds (knowable
/// from local data, no warehouse round-trip needed) -- see
/// [`read_node_columns_parquet`]'s doc comment for why a genuine `source`'s
/// columns still won't resolve under Fusion today.
type CatalogSchemas = HashMap<String, Vec<String>>;

/// Reads real column schemas from whichever artifact is actually sitting
/// next to the manifest at `manifest_path` (dbt always writes its own
/// artifacts to the same `target/` directory): a sibling `catalog.json`
/// (dbt-core) when one is present, else dbt Fusion's own
/// `index/dbt.node_columns.parquet` index. Producing an empty
/// [`CatalogSchemas`] -- exactly as if no catalog-backed resolution were
/// available at all -- when neither is present, or the one that is present
/// can't be read/parsed. This is deliberately infallible (unlike
/// [`read_manifest`]): both artifacts are optional bonus detail a project
/// may never have generated, so their absence must never surface as an
/// adapter error, only as today's existing `Opaque`/unexpanded-wildcard
/// behavior.
///
/// `catalog.json` always wins when both are present (e.g. a `target/` left
/// over from a project mid-migration from dbt-core to Fusion) -- a real,
/// warehouse-verified schema should never be displaced by a statically
/// inferred one. Selection is based on which file actually exists on disk,
/// not on parsing `manifest.json`'s own `dbt_version`, so a future dbt
/// release that keeps writing either artifact in the same shape keeps
/// working here unchanged.
fn read_catalog(manifest_path: &Path) -> CatalogSchemas {
    let Some(dir) = manifest_path.parent() else {
        return CatalogSchemas::new();
    };
    let catalog_json_path = dir.join("catalog.json");
    if catalog_json_path.is_file() {
        return read_catalog_json(&catalog_json_path);
    }
    read_node_columns_parquet(&dir.join("index").join("dbt.node_columns.parquet"))
}

/// Reads and parses `catalog.json` at `path`, producing an empty
/// [`CatalogSchemas`] if it can't be read or its contents aren't valid
/// `catalog.json` JSON. Callers ([`read_catalog`]) only reach this once
/// they've confirmed the file exists -- unlike that unreadable/unparseable
/// case, a genuinely *missing* `catalog.json` doesn't reach here at all, so
/// [`read_catalog`] can fall through to the Fusion parquet index instead.
fn read_catalog_json(path: &Path) -> CatalogSchemas {
    let Ok(raw) = fs::read_to_string(path) else {
        return CatalogSchemas::new();
    };
    let Ok(catalog) = serde_json::from_str::<RawCatalog>(&raw) else {
        return CatalogSchemas::new();
    };
    build_catalog_schemas(&catalog)
}

/// Reads dbt Fusion's `index/dbt.node_columns.parquet` at `path` -- written
/// whenever a project compiles with `--write-index` and
/// `static_analysis: strict` (`dbt docs generate` implies `--write-index`;
/// `dbt compile`/`build` need it passed explicitly), one row per relation
/// per column. Only three of its ~20 columns are consulted --
/// `unique_id`, `column_name`, `column_index` -- read by name via
/// [`parquet`]'s row API rather than positionally, so the many other
/// columns Fusion also writes (types, descriptions, tags, ...) can vary or
/// grow across Fusion releases without breaking this reader. Column
/// ordering mirrors [`build_catalog_schemas`]'s own `catalog.json`
/// convention: rows are grouped by `unique_id`, sorted by `column_index`
/// (missing treated as sorting last, same as a missing `catalog.json`
/// `index`), then flattened to a bare name list.
///
/// Deliberately infallible, the same as [`read_catalog_json`]: a missing
/// index directory/file, a corrupt file, or one with an unexpected schema
/// all degrade silently to an empty [`CatalogSchemas`] rather than an
/// error -- this is undocumented, internal Fusion artifact whose exact
/// shape isn't a stable contract yet.
///
/// Known coverage gap, not a bug in this reader: as observed against a
/// real Fusion build, this index reliably carries real columns for
/// **seeds** (a seed's schema is knowable purely from local CSV data, no
/// warehouse connection needed) but not for genuine `source`-declared
/// external tables -- Fusion's static analysis has no way to know a live,
/// externally managed table's real schema without actually connecting to
/// the warehouse. `catalog.json` closes that gap for dbt-core because it's
/// built from a real warehouse introspection; Fusion has no working
/// equivalent artifact for that case today.
fn read_node_columns_parquet(path: &Path) -> CatalogSchemas {
    (|| -> Option<CatalogSchemas> {
        let file = fs::File::open(path).ok()?;
        let reader = SerializedFileReader::new(file).ok()?;
        let mut raw: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        // `get_row_iter(None)` reads every column of every row, including
        // the ~17 others a real Fusion build also writes here (types,
        // descriptions, tags, ...) that this reader never looks at -- a
        // projected schema naming just the 3 fields below would skip
        // decoding the rest. Left as `None` for now (a projection needs
        // building a `SchemaType` subset from `reader.metadata()`'s own
        // schema, not done here yet); worth revisiting if a real, large
        // project's `dbt.node_columns.parquet` makes this measurably
        // slow.
        for row in reader.get_row_iter(None).ok()? {
            let row = row.ok()?;
            let mut unique_id: Option<String> = None;
            let mut column_name: Option<String> = None;
            let mut column_index: i64 = i64::MAX;
            for (name, field) in row.get_column_iter() {
                match (name.as_str(), field) {
                    ("unique_id", ParquetField::Str(value)) => unique_id = Some(value.clone()),
                    ("column_name", ParquetField::Str(value)) => column_name = Some(value.clone()),
                    ("column_index", ParquetField::Long(value)) => column_index = *value,
                    _ => {}
                }
            }
            if let (Some(unique_id), Some(column_name)) = (unique_id, column_name) {
                raw.entry(unique_id)
                    .or_default()
                    .push((column_index, column_name));
            }
        }
        Some(
            raw.into_iter()
                .map(|(unique_id, mut columns)| {
                    columns.sort_by_key(|(index, _)| *index);
                    (
                        unique_id,
                        columns.into_iter().map(|(_, name)| name).collect(),
                    )
                })
                .collect(),
        )
    })()
    .unwrap_or_default()
}

/// Flattens a parsed [`RawCatalog`]'s `nodes` and `sources` maps (both
/// keyed by `unique_id`, identically shaped) into one [`CatalogSchemas`],
/// each relation's columns ordered by `catalog.json`'s own recorded
/// `index`.
fn build_catalog_schemas(catalog: &RawCatalog) -> CatalogSchemas {
    catalog
        .nodes
        .iter()
        .chain(catalog.sources.iter())
        .map(|(unique_id, relation)| {
            let mut columns: Vec<(i64, String)> = relation
                .columns
                .iter()
                .map(|(key, column)| {
                    let name = column.name.clone().unwrap_or_else(|| key.clone());
                    (column.index.unwrap_or(i64::MAX), name)
                })
                .collect();
            columns.sort_by_key(|(index, _)| *index);
            (
                unique_id.clone(),
                columns.into_iter().map(|(_, name)| name).collect(),
            )
        })
        .collect()
}

/// `catalog.json`'s top-level shape: `nodes` (models) and `sources`, each
/// keyed by the same `unique_id` `manifest.json` itself uses.
#[derive(Debug, Default, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    nodes: HashMap<String, RawCatalogRelation>,
    #[serde(default)]
    sources: HashMap<String, RawCatalogRelation>,
}

/// A single relation's entry in `catalog.json` -- only its real,
/// warehouse-introspected column list is consulted.
#[derive(Debug, Default, Deserialize)]
struct RawCatalogRelation {
    #[serde(default)]
    columns: HashMap<String, RawCatalogColumn>,
}

/// A single column's entry in `catalog.json`. The map key it's stored
/// under (in [`RawCatalogRelation::columns`]) is usually the column's own
/// name, but dbt sometimes normalizes that key's casing per-warehouse
/// (e.g. Snowflake's uppercased keys) -- `name` is the column's real,
/// un-normalized name as dbt itself records it, preferred over the map key
/// whenever present.
#[derive(Debug, Default, Deserialize)]
struct RawCatalogColumn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    index: Option<i64>,
}

/// Splits `dbt_command` into a program plus any leading prefix arguments,
/// shell-word-style -- so a value like `"uv run dbt"`, or a custom wrapper
/// a project's own tooling already uses instead of invoking `dbt`
/// directly (e.g. `"myshell custom-flag"`), works as a genuine multi-word
/// prefix rather than being mistaken for one literal executable named
/// `"uv run dbt"`. Shared by [`run_dbt_subcommand`] and
/// [`DbtQueryExecutor::run_operation`], the two places that actually spawn
/// a `dbt`-shaped subprocess.
///
/// `dbt_command` is checked against the filesystem *first*, before any
/// shell-word splitting is attempted: when it already names a real,
/// existing file on its own, it's used directly as the program with no
/// args, never run through `shell_words::split` at all. This matters
/// specifically on Windows, where `shell_words` (POSIX shell tokenizing
/// rules -- backslash is an *escape character*) and a real Windows path
/// (backslash as the directory separator, routinely containing spaces
/// too, e.g. anything under `%USERPROFILE%\OneDrive\Documents`) are
/// fundamentally incompatible: splitting a raw absolute path like
/// `C:\Users\alex\dbt test\.venv\Scripts\dbt.exe` mangles it (a bare
/// space becomes an argument boundary; backslashes before ordinary
/// characters get silently eaten as escape sequences) long before it
/// ever reaches `Command::new`, no matter how the value is quoted in
/// `zhao.yml`. A path that exists is unambiguous -- there's no reason to
/// tokenize it into a "program" and "args" at all -- so this sidesteps
/// the whole class of problem for the common "point `dbt-command` at one
/// specific binary" case, while still shell-word-splitting exactly as
/// before for a genuine multi-word wrapper (which never names a real
/// file directly, since the file is one of the *later* words).
fn split_dbt_command(dbt_command: &str) -> Result<(String, Vec<String>), DbtAdapterError> {
    if Path::new(dbt_command).is_file() {
        return Ok((dbt_command.to_string(), Vec::new()));
    }

    let mut parts =
        shell_words::split(dbt_command).map_err(|source| DbtAdapterError::CommandNotFound {
            command: dbt_command.to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;
    if parts.is_empty() {
        return Err(DbtAdapterError::CommandNotFound {
            command: dbt_command.to_string(),
            source: std::io::Error::other("dbt-command resolved to an empty command"),
        });
    }
    let program = parts.remove(0);
    Ok((program, parts))
}

/// Returns `extra_args` unchanged unless `command` reports itself as dbt
/// Fusion (see [`command_reports_dbt_fusion`]) *and* doesn't already
/// configure `--write-index` itself -- in which case
/// `--write-index --static-analysis strict` is appended. Only
/// [`DbtAdapter::compile`] calls this, never `deps` (which never writes
/// this artifact regardless) or the Baseline's own compile indirectly
/// through it -- a Baseline compiled this way benefits the same as any
/// other compile, no special-casing needed.
///
/// This is *additive* detection on top of `extra_args`, not a decision
/// zhao-cli makes once and remembers: `command_reports_dbt_fusion` runs
/// again on every `compile()` call. That's deliberately simple over
/// caching the result -- a single extra `--version` invocation per
/// compile is cheap next to the compile itself, and avoids a whole
/// class of "the cached answer went stale when the user swapped dbt
/// versions mid-session" bugs for a saving that wouldn't be
/// measurable anyway.
fn with_fusion_index_flags(
    project_dir: &Path,
    command: &str,
    extra_args: &[String],
) -> Vec<String> {
    let already_configured = extra_args.iter().any(|arg| arg == "--write-index");
    if already_configured || !command_reports_dbt_fusion(project_dir, command) {
        return extra_args.to_vec();
    }
    let mut effective = extra_args.to_vec();
    effective.push("--write-index".to_string());
    effective.push("--static-analysis".to_string());
    effective.push("strict".to_string());
    effective
}

/// Whether `command --version` reports itself as dbt Fusion (its own
/// version banner starts with `dbt-fusion`, e.g. `dbt-fusion
/// 2.0.0-preview.218` -- confirmed against a real Fusion install; dbt-
/// core's own `--version` output never contains this string).
///
/// Run in `project_dir`, the same as [`run_dbt_subcommand`] -- `--version`
/// itself doesn't need an actual dbt project to answer, but `command`
/// isn't necessarily real `dbt`: it can be a user-configured wrapper
/// script (`dbt-command` in `zhao.yml`) that does something else
/// entirely for `--version`, and a test's own stub script always is one
/// (see `stub_fusion_dbt_command`) -- either way, nothing this probe
/// invokes should be free to write into whatever directory zhao-cli's
/// own process happens to have as its CWD.
///
/// Deliberately defaults to `false` (dbt-core's behavior: no extra
/// flags) for anything other than a confirmed Fusion match -- a
/// `command` that can't be split, can't be spawned, exits non-zero, or
/// whose output just doesn't mention Fusion. Getting this wrong in the
/// "add the flags anyway" direction would break every dbt-core
/// project's compile outright (unrecognized flags); getting it wrong in
/// the "don't add them" direction only costs the same missing-index gap
/// this function exists to close, matching today's existing behavior --
/// so an inconclusive answer always resolves to the safer of the two.
fn command_reports_dbt_fusion(project_dir: &Path, command: &str) -> bool {
    let Ok((program, prefix_args)) = split_dbt_command(command) else {
        return false;
    };
    let Ok(output) = std::process::Command::new(&program)
        .args(&prefix_args)
        .arg("--version")
        .current_dir(project_dir)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout.contains("dbt-fusion") || stderr.contains("dbt-fusion")
}

/// Runs `dbt_command <subcommand> <extra_args...>` in `project_dir`,
/// shared by [`DbtAdapter::compile`] and [`DbtAdapter::deps`].
fn run_dbt_subcommand(
    dbt_command: &str,
    subcommand: &str,
    project_dir: &Path,
    extra_args: &[String],
) -> Result<std::process::Output, DbtAdapterError> {
    let (program, prefix_args) = split_dbt_command(dbt_command)?;
    std::process::Command::new(&program)
        .args(&prefix_args)
        .arg(subcommand)
        .args(extra_args)
        .current_dir(project_dir)
        .output()
        .map_err(|source| DbtAdapterError::CommandNotFound {
            command: dbt_command.to_string(),
            source,
        })
}

// ---------------------------------------------------------------------
// Raw manifest.json shape (private -- nothing outside this module should
// ever see these types; `parse` only ever returns the neutral
// `ParsedProject`).
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    nodes: HashMap<String, RawNode>,
    #[serde(default)]
    sources: HashMap<String, RawSource>,
    #[serde(default)]
    metadata: RawManifestMetadata,
}

/// The subset of a manifest's top-level `metadata` block zhao consults --
/// `adapter_type` is dbt's own name for whichever warehouse this
/// manifest's target compiled against (e.g. `"snowflake"`), the value
/// [`crate::adapters::warehouse::resolve`] matches against.
#[derive(Debug, Default, Deserialize)]
struct RawManifestMetadata {
    #[serde(default)]
    adapter_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    unique_id: String,
    resource_type: String,
    name: String,
    // `manifest.nodes` holds every resource type (models, seeds,
    // snapshots, tests, ...) in one map, so this struct must deserialize
    // successfully for all of them even though only "model" entries are
    // used. These three are optional (rather than required, non-`Option`
    // fields) so that some future or older dbt version's non-model node
    // shape lacking one of them doesn't fail parsing the *entire*
    // manifest -- see `build_parsed_project`, which skips any "model"
    // entry missing one, rather than using it with a bogus default.
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    depends_on: RawDependsOn,
    #[serde(default)]
    compiled_code: Option<String>,
    /// Whatever columns this model's `schema.yml` happens to document --
    /// a partial, optional list, never the real output schema (see the
    /// module-level doc comment). Only consulted for `data_type`.
    #[serde(default)]
    columns: HashMap<String, RawColumnDoc>,
    #[serde(default)]
    config: RawNodeConfig,
}

/// The subset of a model's resolved `config` block zhao consults --
/// dbt's manifest embeds all of a model's applied config here (Presets,
/// `dbt_project.yml` defaults, and the model's own `{{ config(...) }}`
/// call all already merged), so this is always the actual materialization
/// in effect, not just what the model itself declared.
#[derive(Debug, Default, Deserialize)]
struct RawNodeConfig {
    #[serde(default)]
    materialized: Option<String>,
}

/// A single documented-column entry from a model's `schema.yml`, as
/// dbt records it in the manifest.
#[derive(Debug, Default, Deserialize)]
struct RawColumnDoc {
    #[serde(default)]
    data_type: Option<String>,
}

impl RawNode {
    /// This model's fully-qualified relation name, if all three parts are
    /// present -- `None` for a malformed or unexpectedly-shaped entry.
    fn qualified_name(&self) -> Option<QualifiedName> {
        Some((
            self.database.clone()?,
            self.schema.clone()?,
            self.alias.clone()?,
        ))
    }
}

#[derive(Debug, Deserialize)]
struct RawSource {
    unique_id: String,
    name: String,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
}

impl RawSource {
    /// This source's fully-qualified relation name, if all three parts
    /// are present -- `None` for a malformed or unexpectedly-shaped entry.
    fn qualified_name(&self) -> Option<QualifiedName> {
        Some((
            self.database.clone()?,
            self.schema.clone()?,
            self.identifier.clone()?,
        ))
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawDependsOn {
    #[serde(default)]
    nodes: Vec<String>,
}

/// A relation's fully-qualified name as it appears in compiled SQL:
/// `"database"."schema"."identifier"`.
type QualifiedName = (String, String, String);

// ---------------------------------------------------------------------
// Orchestration: manifest -> ParsedProject
// ---------------------------------------------------------------------

fn build_parsed_project(manifest: &RawManifest, catalog: &CatalogSchemas) -> ParsedProject {
    // Only "model" entries missing a usable qualified name are skipped
    // (rather than failing the whole manifest) -- see the doc comment on
    // `RawNode`'s `database`/`schema`/`alias` fields for why those are
    // optional in the first place.
    let models: Vec<&RawNode> = manifest
        .nodes
        .values()
        .filter(|n| n.resource_type == "model" && n.qualified_name().is_some())
        .collect();

    // Seeds are never SQL-parsed (no `compiled_code` -- dbt loads them
    // straight from a CSV, see `RawNode`'s doc comment), so they
    // deliberately stay out of `models`/`ordered`/the SQL-resolution loop
    // below: a seed never becomes its own top-level `Node` entry with a
    // resolved column list the way a model does, and never gets a
    // `resolved_schemas` entry either -- unchanged from before this. But a
    // model can still `ref()` a seed directly, and dbt compiles that to
    // the seed's own fully-qualified relation name, same as any other
    // `ref()`/`source()` -- so a seed's *qualified name* still needs to be
    // in `known_relations`, or a `SELECT ... FROM <that seed>` can never
    // even match it to `Upstream::Node(seed_id)` in the first place, no
    // matter what real-schema fallback `expand_wildcard_of` might offer.
    let seeds: Vec<&RawNode> = manifest
        .nodes
        .values()
        .filter(|n| n.resource_type == "seed" && n.qualified_name().is_some())
        .collect();

    let sources: Vec<&RawSource> = manifest
        .sources
        .values()
        .filter(|s| s.qualified_name().is_some())
        .collect();

    let origins: Vec<Origin> = sources
        .iter()
        .map(|s| Origin {
            id: OriginId::new(s.unique_id.clone()),
            name: s.name.clone(),
        })
        .collect();

    // Every known relation's fully-qualified name, so compiled SQL's fully
    // qualified table references (dbt always compiles `ref()`/`source()`
    // to `"database"."schema"."identifier"`) can be matched back to a
    // specific Node or Origin.
    //
    // Insertion order here is a deliberate precedence, not just iteration
    // order: sources, then models, then seeds -- so a seed wins whenever
    // its physical relation happens to collide with a source's (a real,
    // supported dbt pattern: a project can declare both a `source()` and
    // a seed pointing at the same physical table, so either can be
    // `ref()`'d/`source()`'d interchangeably -- jaffle_shop's own
    // tutorial models do exactly this). A `ref('a_seed')` compiles to
    // that seed's own relation name, so attributing it to the seed's own
    // Node -- what the SQL actually called -- is more faithful than
    // attributing it to a same-address source purely because that
    // source's entry happened to be inserted first.
    let mut known_relations: HashMap<QualifiedName, Upstream> = HashMap::new();
    for source in &sources {
        known_relations.insert(
            source.qualified_name().expect("filtered above"),
            Upstream::Origin(OriginId::new(source.unique_id.clone())),
        );
    }
    for model in &models {
        known_relations.insert(
            model.qualified_name().expect("filtered above"),
            Upstream::Node(NodeId::new(model.unique_id.clone())),
        );
    }
    for seed in &seeds {
        known_relations.insert(
            seed.qualified_name().expect("filtered above"),
            Upstream::Node(NodeId::new(seed.unique_id.clone())),
        );
    }

    // Upstream models must be processed before downstream ones: a
    // downstream model's `SELECT *` against an upstream needs that
    // upstream's already-resolved column list to expand.
    let ordered = topological_order(&models);

    // Every model in one manifest was compiled against the same target,
    // so one dialect (picked once, from the manifest's own
    // `metadata.adapter_type`) applies to every model's `compiled_code` --
    // see `resolve_sql_dialect`.
    let dialect = resolve_sql_dialect(manifest.metadata.adapter_type.as_deref());

    let mut nodes = Vec::with_capacity(ordered.len());
    let mut edges = Vec::new();
    let mut resolved_schemas: HashMap<NodeId, Vec<ColumnName>> = HashMap::new();

    for model in ordered {
        let node_id = NodeId::new(model.unique_id.clone());

        let parsed_query = model
            .compiled_code
            .as_deref()
            .and_then(|sql| parse_query_with_dialect(sql, dialect.as_ref()));
        let local_schema = parsed_query
            .as_ref()
            .map(|query| resolve_query(query, &known_relations, &resolved_schemas, catalog))
            .unwrap_or(LocalSchema::Opaque);
        let joins = parsed_query.as_ref().map(extract_joins).unwrap_or_default();

        // A pure passthrough (`SELECT * FROM <one thing>`, propagated
        // unchanged by `resolve_select`) is expanded into a concrete
        // column list the same way any other wildcard is: always possible
        // for an upstream Node (its real columns are already resolved);
        // for an upstream Origin, only when a `catalog.json` covers it
        // (see `expand_wildcard_of`/`read_catalog`) -- absent that, an
        // Origin's real columns still aren't known, same as before
        // catalog-backed resolution existed. `Known` expands to exactly
        // its own already-resolved column list; `Opaque`/`AmbiguousAlias`
        // never expand.
        let expanded = expand_wildcard_of(&local_schema, &resolved_schemas, catalog);

        let columns: Vec<ColumnName> = expanded
            .as_ref()
            .map(|cols| {
                cols.iter()
                    .map(|c| ColumnName::new(c.name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        // Column-level edges, from whatever was resolved. A calculated
        // column can carry more than one source (see the module-level
        // "Known limitations" doc comment) -- one edge per referenced
        // upstream column. `Known` and `Passthrough` (Node or, now,
        // catalog-covered Origin) both flow through `expanded` uniformly;
        // `expanded` is `None` for `Opaque`/`AmbiguousAlias`, or an
        // unresolvable `Passthrough`, so no edges are added for those.
        if let Some(cols) = &expanded {
            for col in cols {
                for (upstream, upstream_col) in &col.sources {
                    edges.push(LineageEdge {
                        upstream: upstream.clone(),
                        downstream: node_id.clone(),
                        column: Some(ColumnLineage {
                            upstream_column: ColumnName::new(upstream_col.clone()),
                            downstream_column: ColumnName::new(col.name.clone()),
                        }),
                    });
                }
            }
        }

        // Baseline node-level edges from dbt's own dependency list, for
        // Node/Origin dependencies (see `resolve_dependency_id`) -- more
        // reliable than our own SQL resolution alone, since it accounts
        // for macro expansions and references our resolution might miss
        // (e.g. in a WHERE clause). Column-level edges above are additive
        // detail, not a replacement for these.
        for dep in &model.depends_on.nodes {
            if let Some(upstream) = resolve_dependency_id(dep, manifest) {
                edges.push(LineageEdge {
                    upstream,
                    downstream: node_id.clone(),
                    column: None,
                });
            }
        }

        resolved_schemas.insert(node_id.clone(), columns.clone());

        let expressions: HashMap<&str, &str> = match &local_schema {
            LocalSchema::Known(cols) => cols
                .iter()
                .filter_map(|c| c.expression.as_deref().map(|e| (c.name.as_str(), e)))
                .collect(),
            _ => HashMap::new(),
        };

        // A column's `STRUCT` field shape, when its immediate defining
        // expression made one statically knowable -- see
        // `extract_struct_shape`. Looked up the same way `expressions`
        // is, immediately above.
        let struct_shapes: HashMap<&str, &[StructField]> = match &local_schema {
            LocalSchema::Known(cols) => cols
                .iter()
                .filter_map(|c| c.struct_fields.as_deref().map(|f| (c.name.as_str(), f)))
                .collect(),
            _ => HashMap::new(),
        };

        let documented_columns: Vec<Column> = columns
            .iter()
            .map(|name| Column {
                name: name.clone(),
                data_type: model
                    .columns
                    .get(name.as_str())
                    .and_then(|doc| doc.data_type.clone()),
                expression: expressions.get(name.as_str()).map(|e| e.to_string()),
                struct_fields: struct_shapes.get(name.as_str()).map(|f| f.to_vec()),
            })
            .collect();

        nodes.push(Node {
            id: node_id,
            name: model.name.clone(),
            columns: documented_columns,
            joins,
            materialization: materialization(model.config.materialized.as_deref()),
        });
    }

    // Seeds are `Upstream::Node` for lineage-edge purposes (see
    // `resolve_dependency_id`) but deliberately never join `models`/
    // `ordered`/the SQL-resolution loop above (no `compiled_code` to
    // resolve -- see this module's own doc comment). Without a matching
    // entry here, though, a seed is reachable in a graph traversal (an
    // edge names it) but was never actually enumerable as a real Node at
    // all -- `full_lineage.json`'s own `nodes` list came from exactly
    // this `nodes` Vec, so a lineage graph (zhao's own HTML export, or a
    // consumer like the VS Code extension) could compute that a seed is
    // in scope, but never had an actual node to draw for it, only a
    // dangling edge endpoint. This closes that gap: one `Node` per seed,
    // real columns from `catalog` when it's available (the same
    // fallback `expand_wildcard_of`'s `Upstream::Node` arm already uses
    // for a seed with no `resolved_schemas` entry), empty otherwise --
    // never an error, matching every other catalog-backed lookup here.
    for seed in &seeds {
        let columns = catalog
            .get(seed.unique_id.as_str())
            .map(|names| {
                names
                    .iter()
                    .map(|name| Column {
                        name: ColumnName::new(name.clone()),
                        data_type: None,
                        expression: None,
                        struct_fields: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        nodes.push(Node {
            id: NodeId::new(seed.unique_id.clone()),
            name: seed.name.clone(),
            columns,
            joins: Vec::new(),
            // A seed has no `config.materialized` of its own to read --
            // it's always, structurally, a real physical table dbt
            // loads from a CSV, so `Table` is simply accurate, not a
            // fallback/default the way it is for a model with no
            // declared materialization.
            materialization: Materialization::Table,
        });
    }

    let seed_node_ids = seeds
        .iter()
        .map(|seed| NodeId::new(seed.unique_id.clone()))
        .collect();

    ParsedProject {
        nodes,
        origins,
        edges,
        seed_node_ids,
    }
}

/// Resolves a `depends_on.nodes` entry (a dbt `unique_id`) to an
/// [`Upstream`]. Returns `None` for a dependency on anything other than a
/// model, seed, or source. Seeds are treated as Nodes (the same way models
/// are), since they're buildable dbt resources that zhao tracks. Origins
/// (sources) and other resource types (snapshots, tests, etc.) are handled
/// separately. This allows seeded data to appear in lineage graphs.
fn resolve_dependency_id(unique_id: &str, manifest: &RawManifest) -> Option<Upstream> {
    if let Some(node) = manifest.nodes.get(unique_id) {
        // Treat both models and seeds as Nodes (buildable resources)
        if node.resource_type == "model" || node.resource_type == "seed" {
            return Some(Upstream::Node(NodeId::new(node.unique_id.clone())));
        }
        return None;
    }
    manifest
        .sources
        .get(unique_id)
        .map(|source| Upstream::Origin(OriginId::new(source.unique_id.clone())))
}

/// Maps a manifest's `config.materialized` string to zhao's neutral
/// [`Materialization`] -- `None` (the field was entirely absent) is
/// treated the same as `"view"`, dbt's own default when a model declares
/// no materialization at all.
fn materialization(materialized: Option<&str>) -> Materialization {
    match materialized.unwrap_or("view") {
        "table" => Materialization::Table,
        "view" => Materialization::View,
        "incremental" => Materialization::Incremental,
        "ephemeral" => Materialization::Ephemeral,
        other => Materialization::Other(other.to_string()),
    }
}

/// Orders models so that every model appears after all the other models it
/// (transitively) depends on, via a straightforward depth-first
/// post-order traversal. Falls back to input order for any cycle (which
/// shouldn't occur in a valid dbt project's DAG).
fn topological_order<'a>(models: &[&'a RawNode]) -> Vec<&'a RawNode> {
    let by_id: HashMap<&str, &RawNode> =
        models.iter().map(|m| (m.unique_id.as_str(), *m)).collect();
    let mut visited: HashMap<&str, bool> = HashMap::new();
    let mut ordered = Vec::with_capacity(models.len());

    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a RawNode>,
        visited: &mut HashMap<&'a str, bool>,
        ordered: &mut Vec<&'a RawNode>,
    ) {
        match visited.get(id) {
            Some(true) => return,  // already emitted
            Some(false) => return, // mid-traversal: a cycle, skip re-entering
            None => {}
        }
        visited.insert(id, false);
        if let Some(node) = by_id.get(id) {
            for dep in &node.depends_on.nodes {
                visit(dep.as_str(), by_id, visited, ordered);
            }
            ordered.push(node);
        }
        visited.insert(id, true);
    }

    for model in models {
        visit(model.unique_id.as_str(), &by_id, &mut visited, &mut ordered);
    }
    ordered
}

// ---------------------------------------------------------------------
// SQL resolution
// ---------------------------------------------------------------------

/// A resolved column: its output name, every upstream Node/Origin column
/// it's traceable to (zero, one, or several -- see the module-level
/// "Known limitations" doc comment), and, for a calculated/derived column,
/// its rendered defining SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedColumn {
    name: String,
    sources: Vec<(Upstream, String)>,
    expression: Option<String>,
    /// This column's `STRUCT` internal field shape. Set when either:
    /// - this column's own *immediate* defining expression is a
    ///   `CAST(... AS STRUCT<...>)` or a `STRUCT(...)`/`named_struct(...)`
    ///   constructor that names every field explicitly -- see
    ///   [`extract_struct_shape`]; or
    /// - it's a plain (optionally qualified) passthrough/rename of a
    ///   single already-resolved `Known` column that itself has a shape --
    ///   see [`propagate_struct_shape`], which carries this forward the
    ///   same way `sources` already carries lineage forward across a CTE
    ///   hop (via [`source_of`]).
    ///
    /// `None` otherwise -- a calculated expression with no explicit struct
    /// constructor, a struct-field access one level in (`payload.user_id`
    /// doesn't carry `payload`'s own shape), a rename sourced from a
    /// `Passthrough`/`Opaque`/`AmbiguousAlias` relation (an Origin's or an
    /// unresolved relation's real shape is never known), or a wildcard
    /// expansion (see [`expand_wildcard_of`], which only ever has bare
    /// column *names* to work with, never full `Column` detail).
    struct_fields: Option<Vec<StructField>>,
}

/// The resolved shape of a query or CTE, as far as this adapter could
/// determine it.
#[derive(Debug, Clone)]
enum LocalSchema {
    /// An identity passthrough of a single upstream Node or Origin --
    /// produced only when resolving a base table reference. Any column
    /// name looked up against this resolves to the same-named column on
    /// `upstream`, without needing to enumerate what those columns are.
    Passthrough(Upstream),
    /// An explicit, fully-known projection list.
    Known(Vec<ResolvedColumn>),
    /// Couldn't resolve (e.g. a table-valued function in `FROM`, a window
    /// function, or an ambiguous unqualified wildcard) -- opaque past this
    /// point.
    Opaque,
    /// Two or more relations in a `FROM` clause ended up sharing the same
    /// alias (two unaliased relations whose trailing name collides, or a
    /// base table shadowing an earlier CTE) -- produced by [`resolve_from`]
    /// instead of immediately collapsing to `Opaque`, so a later *qualified*
    /// reference to this alias still has a chance at column-by-column
    /// disambiguation: if a `catalog.json` (see [`read_catalog`]) is
    /// available and shows the referenced column exists on only one of
    /// these candidates, [`resolve_path_on_schema`] resolves through that
    /// one; if it's on more than one (or catalog data doesn't cover all of
    /// them), it still correctly comes out unresolved, same as before this
    /// variant existed. An *unqualified* reference never disambiguates
    /// through this -- see `resolve_unqualified`, which only ever matches
    /// `Known`/`Passthrough` schemas, so an ambiguous alias simply
    /// contributes nothing there, exactly as an opaque one already did.
    AmbiguousAlias(Vec<LocalSchema>),
}

/// Parses `sql` against [`GenericDialect`] -- the default every existing
/// caller/test in this module used before dialect-aware parsing existed,
/// and still what every test that isn't specifically exercising a
/// non-generic construct uses. [`build_parsed_project`] itself uses
/// [`parse_query_with_dialect`] instead, selecting whichever dialect the
/// manifest's own `metadata.adapter_type` calls for (see
/// [`resolve_sql_dialect`]). `#[cfg(test)]`-only: nothing in production
/// code parses against a hardcoded dialect anymore.
#[cfg(test)]
fn parse_query(sql: &str) -> Option<Query> {
    parse_query_with_dialect(sql, &GenericDialect {})
}

/// Parses `sql` against a specific `dialect` -- see [`resolve_sql_dialect`]
/// for how a compiled model's own SQL picks one.
fn parse_query_with_dialect(sql: &str, dialect: &dyn Dialect) -> Option<Query> {
    let statements = SqlParser::parse_sql(dialect, sql).ok()?;
    statements.into_iter().find_map(|stmt| match stmt {
        Statement::Query(query) => Some(*query),
        _ => None,
    })
}

/// Selects the `sqlparser` [`Dialect`] matching a manifest's own
/// `metadata.adapter_type` (dbt's name for whichever warehouse it
/// compiled against -- see [`RawManifestMetadata`]), so a warehouse-
/// specific SQL construct a compiled model actually uses parses the way
/// that warehouse's own SQL dialect defines it, rather than however
/// [`GenericDialect`] happens to interpret (or reject) the same syntax.
/// Matched case-insensitively against dbt's own
/// lowercase adapter names. Falls back to [`GenericDialect`] for an
/// adapter type this crate doesn't have (or doesn't recognize) a more
/// specific dialect for -- including `adapter_type` being entirely
/// absent (an unusually old or nonstandard manifest) -- never an error;
/// `GenericDialect` is also what this crate always used before
/// dialect-aware parsing existed, so an unrecognized adapter type is no
/// worse off than before.
fn resolve_sql_dialect(adapter_type: Option<&str>) -> Box<dyn Dialect> {
    match adapter_type.map(str::to_ascii_lowercase).as_deref() {
        Some("snowflake") => Box::new(SnowflakeDialect),
        Some("bigquery") => Box::new(BigQueryDialect),
        Some("databricks") => Box::new(DatabricksDialect),
        Some("spark") => Box::new(SparkSqlDialect),
        Some("postgres") => Box::new(PostgreSqlDialect {}),
        Some("redshift") => Box::new(RedshiftSqlDialect {}),
        Some("duckdb") => Box::new(DuckDbDialect),
        Some("mysql") => Box::new(MySqlDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

/// The kind of each join in a query's final `SELECT`'s `FROM` clause, in
/// order. Joins inside CTEs are not included -- only the outermost query's
/// own joins, which is what actually determines the Node's own output row
/// set. A join whose kind doesn't map to one of [`JoinKind`]'s variants
/// (a non-standard construct like `SEMI JOIN`) is omitted rather than
/// misrepresented as some other kind.
fn extract_joins(query: &Query) -> Vec<JoinKind> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Vec::new();
    };
    select
        .from
        .iter()
        .flat_map(|twj| &twj.joins)
        .filter_map(|join| join_kind_of(&join.join_operator))
        .collect()
}

fn join_kind_of(op: &sqlparser::ast::JoinOperator) -> Option<JoinKind> {
    use sqlparser::ast::JoinOperator;
    match op {
        JoinOperator::Join(_) | JoinOperator::Inner(_) => Some(JoinKind::Inner),
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => Some(JoinKind::Left),
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => Some(JoinKind::Right),
        JoinOperator::FullOuter(_) => Some(JoinKind::Full),
        JoinOperator::CrossJoin(_) => Some(JoinKind::Cross),
        _ => None,
    }
}

/// Resolves a full query (its CTEs, in order, then its final body) against
/// the project's known relations and already-resolved upstream Node
/// schemas.
fn resolve_query(
    query: &Query,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> LocalSchema {
    resolve_query_in_scope(
        query,
        &HashMap::new(),
        known_relations,
        resolved_schemas,
        catalog,
    )
}

/// The shared implementation behind [`resolve_query`] and an inline `FROM`
/// subquery ([`collect_table_factor`]'s `TableFactor::Derived` arm): resolves
/// `query`'s own CTEs, in order, on top of an `outer_scope` that's already in
/// effect (empty at the top-level model query; the enclosing query's own
/// scope for a nested subquery, since a CTE defined in an outer query is
/// visible to a subquery nested in its `FROM` clause, same as real SQL name
/// resolution), then resolves `query`'s final body against the combined
/// scope.
fn resolve_query_in_scope(
    query: &Query,
    outer_scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> LocalSchema {
    let mut scope = outer_scope.clone();

    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let resolved = resolve_set_expr(
                &cte.query.body,
                &scope,
                known_relations,
                resolved_schemas,
                catalog,
            );
            scope.insert(cte.alias.name.value.clone(), resolved);
        }
    }

    resolve_set_expr(
        &query.body,
        &scope,
        known_relations,
        resolved_schemas,
        catalog,
    )
}

fn resolve_set_expr(
    body: &SetExpr,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> LocalSchema {
    match body {
        SetExpr::Select(select) => {
            resolve_select(select, scope, known_relations, resolved_schemas, catalog)
        }
        // A parenthesized subquery body: resolves exactly like the query
        // it wraps.
        SetExpr::Query(query) => {
            resolve_query_in_scope(query, scope, known_relations, resolved_schemas, catalog)
        }
        // UNION/INTERSECT/EXCEPT: each arm is resolved independently, then
        // merged column-by-column (see `merge_set_operation_arms`) --
        // standard SQL set-operation semantics require both arms to already
        // have the same column count, in the same positional order.
        SetExpr::SetOperation { left, right, .. } => {
            let left_schema =
                resolve_set_expr(left, scope, known_relations, resolved_schemas, catalog);
            let right_schema =
                resolve_set_expr(right, scope, known_relations, resolved_schemas, catalog);
            merge_set_operation_arms(&left_schema, &right_schema, resolved_schemas, catalog)
        }
        // VALUES, and DML bodies that can't legally appear here anyway:
        // not attempted, see module-level "Known limitations" doc comment.
        _ => LocalSchema::Opaque,
    }
}

/// Merges two already-resolved `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT` arms
/// into the result's own [`LocalSchema`]. Real SQL requires both arms to
/// project the same number of columns, in the same positional order -- the
/// *names* don't have to match (the result takes the left arm's names,
/// exactly as every warehouse does), and at runtime a given output row (and
/// so a given output column's value) can come from either arm, so a result
/// column's `sources` is the union of what each arm resolves that same
/// positional column to. This applies uniformly to `INTERSECT`/`EXCEPT` too,
/// not just `UNION` -- keeping one merge rule for all three set operators
/// rather than modeling `EXCEPT`'s row-filtering semantics more precisely.
///
/// Either arm can itself already be a `Passthrough` (e.g. `select * from a
/// union select * from b`) -- those are expanded via
/// [`expand_wildcard_of`] first, using the same already-resolved-Node
/// column lists a plain wildcard expansion uses. If both arms can't be
/// reduced to an explicit, equal-length column list, the whole union is
/// `Opaque` -- a mismatched shape isn't valid SQL to begin with, and
/// guessing which columns line up would be worse than admitting we don't
/// know.
fn merge_set_operation_arms(
    left: &LocalSchema,
    right: &LocalSchema,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> LocalSchema {
    let left_cols = expand_wildcard_of(left, resolved_schemas, catalog);
    let right_cols = expand_wildcard_of(right, resolved_schemas, catalog);

    match (left_cols, right_cols) {
        (Some(left_cols), Some(right_cols))
            if !left_cols.is_empty() && left_cols.len() == right_cols.len() =>
        {
            let merged = left_cols
                .into_iter()
                .zip(right_cols)
                .map(|(l, r)| {
                    let mut sources = l.sources.clone();
                    for candidate in r.sources {
                        if !sources.contains(&candidate) {
                            sources.push(candidate);
                        }
                    }
                    ResolvedColumn {
                        name: l.name,
                        sources,
                        // A calculated column's rendered SQL, or a struct's
                        // internal shape, can legitimately differ between
                        // arms -- only report either when both arms agree,
                        // rather than picking one arm's arbitrarily.
                        expression: if l.expression == r.expression {
                            l.expression
                        } else {
                            None
                        },
                        struct_fields: if l.struct_fields == r.struct_fields {
                            l.struct_fields
                        } else {
                            None
                        },
                    }
                })
                .collect();
            LocalSchema::Known(merged)
        }
        _ => LocalSchema::Opaque,
    }
}

fn resolve_select(
    select: &Select,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> LocalSchema {
    let from_scope = resolve_from(
        &select.from,
        scope,
        known_relations,
        resolved_schemas,
        catalog,
    );

    // `SELECT * FROM <one thing>` and nothing else is a pure passthrough:
    // propagate whatever LocalSchema that one thing already resolved to,
    // unchanged, rather than forcing an enumeration we may not be able to
    // do (e.g. the one thing is itself a passthrough of an Origin whose
    // real columns aren't known -- either because no catalog.json is
    // available at all, or [`build_parsed_project`]'s later expansion,
    // shared with every other Passthrough, will use it if there is one).
    if let [SelectItem::Wildcard(_)] = select.projection.as_slice() {
        if from_scope.len() == 1 {
            return from_scope.into_values().next().unwrap();
        }
    }

    let mut columns = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                match expand_wildcard(&from_scope, resolved_schemas, catalog) {
                    Some(mut expanded) => columns.append(&mut expanded),
                    None => return LocalSchema::Opaque,
                }
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let alias = qualified_wildcard_alias(kind);
                match alias.and_then(|a| from_scope.get(&a)) {
                    Some(schema) => match expand_wildcard_of(schema, resolved_schemas, catalog) {
                        Some(mut expanded) => columns.append(&mut expanded),
                        None => return LocalSchema::Opaque,
                    },
                    None => return LocalSchema::Opaque,
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                columns.push(resolve_expr_column(expr, None, &from_scope, catalog));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                columns.push(resolve_expr_column(
                    expr,
                    Some(alias.value.clone()),
                    &from_scope,
                    catalog,
                ));
            }
            // Spark/Databricks' parenthesized multi-column alias syntax,
            // `expr AS (a, b, c)` (e.g. `stack(2, 'a', 'b') AS (col1,
            // col2)`) -- `sqlparser` gates this behind
            // `Dialect::supports_select_item_multi_column_alias`, which
            // `GenericDialect`, `DatabricksDialect`, and `SparkSqlDialect`
            // all enable (only `SnowflakeDialect` -- despite this arm's
            // previous doc comment calling it Snowflake-specific -- does
            // not). `expr` is one expression producing every aliased
            // output column at once (e.g. a table function's multiple
            // return columns), so every alias shares the same traced
            // sources and rendered expression text -- there's no way to
            // attribute a *specific* upstream column to a *specific*
            // alias from the SQL alone.
            SelectItem::ExprWithAliases { expr, aliases } => {
                let sources = collect_expr_sources(expr, &from_scope, catalog);
                let expression = Some(expr.to_string());
                for alias in aliases {
                    columns.push(ResolvedColumn {
                        name: alias.value.clone(),
                        sources: sources.clone(),
                        expression: expression.clone(),
                        struct_fields: None,
                    });
                }
            }
        }
    }

    LocalSchema::Known(columns)
}

/// Resolves the `FROM` clause into a map of alias -> that relation's
/// [`LocalSchema`], covering plain tables, sources, and CTE references.
fn resolve_from(
    from: &[TableWithJoins],
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> HashMap<String, LocalSchema> {
    let mut collected: Vec<(String, LocalSchema)> = Vec::new();
    for twj in from {
        collect_table_factor(
            &twj.relation,
            scope,
            known_relations,
            resolved_schemas,
            catalog,
            &mut collected,
        );
        for join in &twj.joins {
            collect_table_factor(
                &join.relation,
                scope,
                known_relations,
                resolved_schemas,
                catalog,
                &mut collected,
            );
        }
    }

    // A duplicate alias (two unaliased relations that happen to share a
    // trailing name, or a base table shadowing an earlier CTE) means we
    // can no longer trust which relation a later *unqualified* reference
    // meant. Collapsed into `AmbiguousAlias` (carrying every candidate
    // that shared this alias) rather than immediately discarding them as
    // `Opaque` -- a *qualified* reference (`alias.column`) still has a
    // chance at resolving through `resolve_path_on_schema`, if a
    // `catalog.json` shows the column exists on only one candidate. An
    // unqualified reference never gets that chance (see
    // `resolve_unqualified`), so this is strictly additive, not a
    // relaxation of the existing guard against a wrong lineage guess.
    let mut by_alias: HashMap<String, Vec<LocalSchema>> = HashMap::new();
    for (alias, schema) in collected {
        by_alias.entry(alias).or_default().push(schema);
    }
    by_alias
        .into_iter()
        .map(|(alias, mut candidates)| {
            let schema = if candidates.len() > 1 {
                LocalSchema::AmbiguousAlias(candidates)
            } else {
                candidates.pop().expect("just checked len == 1")
            };
            (alias, schema)
        })
        .collect()
}

fn collect_table_factor(
    factor: &TableFactor,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
    collected: &mut Vec<(String, LocalSchema)>,
) {
    if let TableFactor::Derived {
        subquery, alias, ..
    } = factor
    {
        // An inline subquery directly in `FROM` (as opposed to a
        // `WITH`-defined CTE, handled in `resolve_query_in_scope`):
        // resolved the same way a CTE's own query is, as a nested scope
        // that inherits whatever CTEs are already visible here, then
        // treated exactly like a CTE's `Known` schema by the outer query.
        let resolved =
            resolve_query_in_scope(subquery, scope, known_relations, resolved_schemas, catalog);
        let effective_alias = alias
            .as_ref()
            .map(|a| a.name.value.clone())
            .unwrap_or_default();
        collected.push((effective_alias, resolved));
        return;
    }

    if let TableFactor::Table {
        name, alias, args, ..
    } = factor
    {
        // `Ident::to_string()` re-serializes *with* whatever quote
        // character that identifier was originally written with -- `"`
        // for ANSI/Postgres/Snowflake/BigQuery, `` ` `` for
        // MySQL/Databricks/Spark, `[`/`]` for SQL Server. Stripping only
        // `"` left a backtick-quoted relation (e.g. Databricks'
        // `` `catalog`.`schema`.`table` `` -- how dbt-databricks always
        // renders a compiled relation reference) never matching
        // `known_relations`' clean, quote-free keys, so every such
        // relation resolved as `Opaque` instead of a real `Passthrough` --
        // silently breaking column-level lineage for every Databricks
        // project, dbt-core and Fusion alike, not something specific to
        // either.
        let parts: Vec<String> = name
            .0
            .iter()
            .map(|p| p.to_string().replace(['"', '`', '[', ']'], ""))
            .collect();
        let effective_alias = alias
            .as_ref()
            .map(|a| a.name.value.clone())
            .unwrap_or_else(|| parts.last().cloned().unwrap_or_default());

        // `args: Some(..)` means this is actually a table-valued function
        // call (`generate_series(1, 10) AS g`), not a plain table/CTE
        // reference -- sqlparser folds that shape into `TableFactor::Table`
        // rather than giving it its own variant in every case. A TVF's
        // name lives in a different SQL namespace than a table/CTE name,
        // so it must never be looked up against `scope`/`known_relations`
        // (a TVF that happens to share its bare name with an in-scope CTE
        // is legal SQL, and must not silently resolve to that CTE) --
        // stays `Opaque` unconditionally, the same "not attempted" outcome
        // as `TableFactor::TableFunction` below.
        let resolved = if args.is_some() {
            LocalSchema::Opaque
        } else if parts.len() == 3 {
            let qualified = (parts[0].clone(), parts[1].clone(), parts[2].clone());
            match known_relations.get(&qualified) {
                Some(upstream) => LocalSchema::Passthrough(upstream.clone()),
                None => LocalSchema::Opaque,
            }
        } else if parts.len() == 1 {
            // An unqualified name: only meaningful as a reference to an
            // earlier CTE in this same query.
            scope.get(&parts[0]).cloned().unwrap_or(LocalSchema::Opaque)
        } else {
            LocalSchema::Opaque
        };

        collected.push((effective_alias, resolved));
    }
    // A table-valued function represented as `TableFactor::TableFunction`
    // (rather than `TableFactor::Table` with `args: Some(..)`, handled
    // above): not attempted, see module-level "Known limitations" doc
    // comment. Simply not pushed into `collected` -- its alias never
    // enters `from_scope`, so any reference to it resolves the same way
    // any other unrecognized name would.
}

/// Expands a bare `SELECT *` mixed with other projections, or where
/// multiple relations are in scope: valid only when there's exactly one
/// relation (otherwise which table's columns come first is ambiguous, and
/// guessing would be worse than admitting we don't know).
fn expand_wildcard(
    from_scope: &HashMap<String, LocalSchema>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> Option<Vec<ResolvedColumn>> {
    if from_scope.len() != 1 {
        return None;
    }
    let only = from_scope.values().next()?;
    expand_wildcard_of(only, resolved_schemas, catalog)
}

/// Enumerates a [`LocalSchema`]'s columns as a concrete list, when
/// possible. A `Passthrough` of a Node whose SQL has already been resolved
/// (an ordinary model) always enumerates from that -- its real columns are
/// already known. A `Passthrough` of a Node with *no* SQL of its own (a
/// seed: dbt loads it straight from a CSV, so it's a [`Upstream::Node`]
/// with no `compiled_code` to parse, and so no `resolved_schemas` entry
/// either -- see the module-level doc comment's discussion of seeds) falls
/// back to `catalog`, the same real-schema source an Origin already relies
/// on entirely. A `Passthrough` of an Origin can only be enumerated when a
/// `catalog.json` (or, for a dbt Fusion project, its native parquet
/// equivalent -- see [`read_node_columns_parquet`]) covers it -- absent
/// that, an Origin's real columns still aren't known.
fn expand_wildcard_of(
    schema: &LocalSchema,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    catalog: &CatalogSchemas,
) -> Option<Vec<ResolvedColumn>> {
    match schema {
        LocalSchema::Passthrough(upstream @ Upstream::Node(id)) => {
            if let Some(cols) = resolved_schemas.get(id) {
                return Some(
                    cols.iter()
                        .map(|c| ResolvedColumn {
                            name: c.as_str().to_string(),
                            sources: vec![(upstream.clone(), c.as_str().to_string())],
                            expression: None,
                            // A wildcard expansion only ever has an
                            // upstream Node's resolved column *names* to
                            // work with (`resolved_schemas:
                            // HashMap<NodeId, Vec<ColumnName>>` never
                            // carried full `Column` detail) -- there's no
                            // shape to carry forward even if the upstream
                            // column had one.
                            struct_fields: None,
                        })
                        .collect(),
                );
            }
            // No `resolved_schemas` entry: this Node has no SQL of its own
            // to have resolved one from (a seed) -- same "real names,
            // catalog-only" treatment as an Origin, same empty-entry guard
            // as below (an empty catalog entry means the seed just isn't
            // covered, not that it genuinely has zero columns).
            let cols = catalog.get(id.as_str()).filter(|cols| !cols.is_empty())?;
            Some(
                cols.iter()
                    .map(|name| ResolvedColumn {
                        name: name.clone(),
                        sources: vec![(upstream.clone(), name.clone())],
                        expression: None,
                        struct_fields: None,
                    })
                    .collect(),
            )
        }
        LocalSchema::Passthrough(upstream @ Upstream::Origin(id)) => {
            // An *absent* key already falls back to `Opaque` via `?`
            // below -- but a key that's *present* with an empty columns
            // list is just as unusable (a real relation never has zero
            // columns; an empty entry means catalog.json didn't actually
            // introspect this relation's columns, not that it genuinely
            // has none) and must fall back the same way, not be treated
            // as "known: zero columns." Same guard
            // `merge_set_operation_arms` already applies to its own
            // `expand_wildcard_of` results.
            let cols = catalog.get(id.as_str()).filter(|cols| !cols.is_empty())?;
            Some(
                cols.iter()
                    .map(|name| ResolvedColumn {
                        name: name.clone(),
                        sources: vec![(upstream.clone(), name.clone())],
                        expression: None,
                        struct_fields: None,
                    })
                    .collect(),
            )
        }
        LocalSchema::Known(cols) => Some(cols.clone()),
        // A genuinely ambiguous alias can't be enumerated as a column
        // list even with a catalog -- catalog-backed disambiguation (see
        // `resolve_path_on_schema`) only ever resolves one *specific,
        // already-named* column at a time, never "all of this alias's
        // columns" as a set.
        LocalSchema::Opaque | LocalSchema::AmbiguousAlias(_) => None,
    }
}

fn qualified_wildcard_alias(
    kind: &sqlparser::ast::SelectItemQualifiedWildcardKind,
) -> Option<String> {
    match kind {
        sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
            name.0.last().map(|p| p.to_string().replace('"', ""))
        }
        _ => None,
    }
}

/// Resolves a single projection expression to a named column, tracing
/// every plain (optionally qualified) column identifier the expression
/// structurally references -- see [`collect_expr_sources`] and the
/// module-level "Known limitations" doc comment.
fn resolve_expr_column(
    expr: &Expr,
    alias: Option<String>,
    from_scope: &HashMap<String, LocalSchema>,
    catalog: &CatalogSchemas,
) -> ResolvedColumn {
    let sources = collect_expr_sources(expr, from_scope, catalog);

    // A plain (optionally qualified) identifier is a passthrough/rename,
    // not a calculation -- no expression text is worth showing for it.
    // Everything else (function calls, arithmetic, `CASE`, literals, ...)
    // gets its rendered SQL recorded.
    let expression = match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => None,
        other => Some(other.to_string()),
    };

    let name = alias.unwrap_or_else(|| match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => {
            parts.last().map(|p| p.value.clone()).unwrap_or_default()
        }
        other => other.to_string(),
    });

    // A column's own immediate defining expression (`extract_struct_shape`)
    // takes priority when it states a shape explicitly; otherwise, a plain
    // passthrough/rename of an upstream struct column
    // (`propagate_struct_shape`) carries that upstream column's
    // already-resolved shape forward -- the same way `sources` already
    // carries lineage forward across a CTE hop (see `source_of`) -- so a
    // struct's field shape now survives a rename or a CTE hop within the
    // same model, not just its own defining SQL.
    let struct_fields =
        extract_struct_shape(expr).or_else(|| propagate_struct_shape(expr, from_scope));

    ResolvedColumn {
        name,
        sources,
        expression,
        struct_fields,
    }
}

/// Carries a struct-typed column's already-resolved field shape forward
/// across a plain passthrough/rename -- `expr` is a bare (optionally
/// qualified) identifier referencing a single already-resolved `Known`
/// column that itself has a shape (from that column's own immediate
/// defining expression *or*, transitively, from an earlier hop of this
/// same propagation -- since a `Known` relation's `struct_fields` is
/// itself whatever this function already produced for it). Deliberately
/// narrower than `collect_expr_sources`'s own identifier resolution: only
/// an unqualified reference against a single relation in scope, or a
/// 2-part qualified reference (`alias.column`), is attempted -- a struct
/// shape is a single, specific fact about one column, not something
/// several ambiguous candidates could plausibly share the way a
/// calculated column's several *sources* can, so this stays conservative
/// rather than reusing `resolve_unqualified`/`resolve_qualified`'s
/// broader (dotted-prefix, multi-source) matching. `None` for anything
/// else -- a calculated expression, a struct-field access one level in
/// (`payload.user_id` doesn't carry `payload`'s own shape), a
/// `Passthrough`/`Opaque`/`AmbiguousAlias` relation, or a column with no
/// recorded shape at all.
fn propagate_struct_shape(
    expr: &Expr,
    from_scope: &HashMap<String, LocalSchema>,
) -> Option<Vec<StructField>> {
    let struct_fields_on = |schema: &LocalSchema, column: &str| match schema {
        LocalSchema::Known(cols) => cols
            .iter()
            .find(|c| c.name == column)
            .and_then(|c| c.struct_fields.clone()),
        _ => None,
    };

    match expr {
        Expr::Identifier(ident) if from_scope.len() == 1 => {
            struct_fields_on(from_scope.values().next()?, &ident.value)
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let schema = from_scope.get(&parts[0].value)?;
            struct_fields_on(schema, &parts[1].value)
        }
        _ => None,
    }
}

/// Extracts a `STRUCT`-typed column's internal field shape from its
/// *immediate* defining expression, when -- and only when -- that shape is
/// stated explicitly enough to trust. Three shapes are recognized, all
/// dialect-general (Databricks/Spark, BigQuery, and DuckDB all use some
/// combination of these -- see the module-level "Known limitations" doc
/// comment):
///
/// - `CAST(expr AS STRUCT<field_name field_type, ...>)` -- sqlparser's
///   [`DataType::Struct`], produced by `CAST ... AS STRUCT<...>` on every
///   dialect above.
/// - `STRUCT(expr1 [AS name1] [, ...])`, with or without a leading
///   `STRUCT<field_name field_type, ...>` type definition -- sqlparser's
///   dedicated [`Expr::Struct`] (BigQuery's and Databricks' `STRUCT(...)`
///   constructor).
/// - `named_struct('field1', expr1, 'field2', expr2, ...)` -- Databricks'/
///   Spark's alternating-key-value-argument constructor function, which
///   sqlparser has no dedicated `Expr` variant for (it parses as an
///   ordinary [`Expr::Function`]), so this walks its argument list itself.
///
/// Anything else -- most importantly, a plain (possibly qualified)
/// identifier reference, which is what a struct column passed through
/// unchanged via `SELECT *` or a bare `ref()` column reference compiles
/// to -- returns `None`. `None` is also returned, deliberately, whenever a
/// recognized shape doesn't name *every* one of its fields explicitly
/// (e.g. a typeless `STRUCT(1, 2)` with no `AS name` on either value, or a
/// `named_struct(...)` call whose key argument isn't a literal string):
/// reporting only the fields that *do* have a name would misrepresent the
/// struct's real shape, and reporting none of them under a `Some(vec![])`
/// would misrepresent "we don't know" as "it's empty" -- both worse than
/// admitting the whole shape isn't confidently knowable here. See
/// [`Column::struct_fields`]'s doc comment for why `None` is the only
/// value this crate ever uses for "unknown."
fn extract_struct_shape(expr: &Expr) -> Option<Vec<StructField>> {
    match expr {
        Expr::Cast {
            data_type: DataType::Struct(fields, _bracket_kind),
            ..
        } => {
            let mut out = Vec::with_capacity(fields.len());
            for field in fields {
                let name = field.field_name.as_ref()?;
                out.push(StructField {
                    name: ColumnName::new(name.value.clone()),
                    data_type: Some(field.field_type.to_string()),
                });
            }
            Some(out)
        }
        Expr::Struct { values, fields } if !fields.is_empty() => {
            // Typed `STRUCT<field_name field_type, ...>(expr1, ...)`:
            // field names/types come from the type definition, not the
            // values (typed syntax forbids a value-level `AS name` --
            // sqlparser itself rejects it, see `parse_struct_field_expr`).
            let mut out = Vec::with_capacity(fields.len());
            for field in fields {
                let name = field.field_name.as_ref()?;
                out.push(StructField {
                    name: ColumnName::new(name.value.clone()),
                    data_type: Some(field.field_type.to_string()),
                });
            }
            Some(out)
        }
        Expr::Struct { values, fields: _ } => {
            // Typeless `STRUCT(expr1 [AS name1], ...)`: only a value
            // explicitly aliased with `AS name` names its field at all --
            // sqlparser represents that as `Expr::Named`. A value with no
            // `AS` has no stated name to report.
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                let Expr::Named { name, .. } = value else {
                    return None;
                };
                out.push(StructField {
                    name: ColumnName::new(name.value.clone()),
                    // A constructor's value expression only ever states a
                    // *name*, never a type -- the type is merely implied
                    // by the value, which this adapter does not attempt
                    // to infer (see `StructField::data_type`'s doc
                    // comment).
                    data_type: None,
                });
            }
            Some(out)
        }
        Expr::Function(function) if is_named_struct_call(function) => {
            extract_named_struct_shape(function)
        }
        _ => None,
    }
}

/// Whether `function` is a call to Databricks'/Spark's `named_struct`
/// constructor -- matched by name only (case-insensitively, the same way
/// SQL identifiers are themselves case-insensitive by default), since
/// sqlparser has no dedicated `Expr` variant for it.
fn is_named_struct_call(function: &sqlparser::ast::Function) -> bool {
    function
        .name
        .0
        .last()
        .map(|part| part.to_string().eq_ignore_ascii_case("named_struct"))
        .unwrap_or(false)
}

/// Extracts a `named_struct('field1', expr1, 'field2', expr2, ...)` call's
/// field names -- every even-indexed (0-based) argument must be a single-
/// quoted string literal naming the field; every odd-indexed argument is
/// that field's value (its type isn't stated, so `data_type` is always
/// `None`). Returns `None` for anything that doesn't match this shape
/// exactly (an odd argument count, a non-literal or non-string key, a
/// named/wildcard argument, ...) -- see [`extract_struct_shape`]'s doc
/// comment for why a partial match is never reported as a partial result.
fn extract_named_struct_shape(function: &sqlparser::ast::Function) -> Option<Vec<StructField>> {
    let FunctionArguments::List(list) = &function.args else {
        return None;
    };
    if list.args.len() % 2 != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(list.args.len() / 2);
    for pair in list.args.chunks_exact(2) {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(key))) = &pair[0] else {
            return None;
        };
        let Value::SingleQuotedString(field_name) = &key.value else {
            return None;
        };
        // The value argument itself isn't inspected further -- only its
        // presence (confirming this is a real key/value pair, not some
        // other two-argument function that happens to share the name) is
        // needed; its type is never knowable from this call alone.
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(_value)) = &pair[1] else {
            return None;
        };
        out.push(StructField {
            name: ColumnName::new(field_name.clone()),
            data_type: None,
        });
    }
    Some(out)
}

/// Recursively collects every distinct upstream column that `expr`
/// structurally references, in the order first encountered. A plain
/// identifier or qualified identifier contributes (at most) one; a
/// function call, `CAST`, unary/binary operator, `CASE`, or parenthesized
/// expression contributes the union of its sub-expressions' sources.
/// Anything else (a literal, a subquery expression, ...) contributes
/// nothing. Deliberately structural, not a guess: a sub-expression that
/// can't be resolved (e.g. an unqualified name ambiguous among several
/// relations in scope) simply contributes nothing, rather than a wrong
/// guess at what it might be.
fn collect_expr_sources(
    expr: &Expr,
    from_scope: &HashMap<String, LocalSchema>,
    catalog: &CatalogSchemas,
) -> Vec<(Upstream, String)> {
    let mut found = Vec::new();
    collect_expr_sources_into(expr, from_scope, catalog, &mut found);
    found
}

fn collect_expr_sources_into(
    expr: &Expr,
    from_scope: &HashMap<String, LocalSchema>,
    catalog: &CatalogSchemas,
    found: &mut Vec<(Upstream, String)>,
) {
    let push_dedup = |mut new: Vec<(Upstream, String)>, found: &mut Vec<(Upstream, String)>| {
        new.retain(|candidate| !found.contains(candidate));
        found.append(&mut new);
    };

    match expr {
        Expr::Identifier(ident) => {
            push_dedup(
                resolve_unqualified(std::slice::from_ref(&ident.value), from_scope, catalog),
                found,
            );
        }
        // A dotted identifier chain, N >= 2 parts. Ordinarily `table.column`,
        // but this is also exactly how a struct/`STRUCT`-typed column's
        // nested field access compiles on Databricks, BigQuery, and DuckDB
        // (`t.payload.user_id`, or even `payload.user_id` with no table
        // alias at all if `payload` itself is a struct column) -- sqlparser
        // represents both shapes identically, as a flat `CompoundIdentifier`.
        // If `parts[0]` resolves as a real table alias, the rest of the
        // path is resolved against that relation; otherwise the whole path
        // is tried as an unqualified (possibly struct-field-accessing)
        // reference. See [`resolve_qualified`]/[`resolve_unqualified`] and
        // [`resolve_path_on_schema`] for how a multi-part path collapses to
        // its base column when no exact longer match exists in scope.
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let path: Vec<String> = parts.iter().map(|p| p.value.clone()).collect();
            if from_scope.contains_key(&path[0]) {
                push_dedup(
                    resolve_qualified(&path[0], &path[1..], from_scope, catalog),
                    found,
                );
            } else {
                push_dedup(resolve_unqualified(&path, from_scope, catalog), found);
            }
        }
        // Snowflake's and Databricks' semi-structured (`VARIANT`) colon
        // access, e.g. `payload:user_id` or `t.payload:user.id`. Unlike a
        // struct's dot-notation field access above, a `VARIANT` field has
        // no fixed schema at all -- there's no scenario (documented or
        // otherwise) where a longer dotted name could be a real column, so
        // this only traces `value` (the base column reference being
        // accessed) and drops the JSON path entirely, rather than attempting to
        // collapse it into a longer dotted name the way `CompoundIdentifier`
        // does.
        Expr::JsonAccess { value, .. } => {
            collect_expr_sources_into(value, from_scope, catalog, found);
        }
        // Bracket/subscript access mixed with (or instead of) dot access --
        // array indexing (`arr[0]`), map access (`m['key']`), or a chain
        // combining both with struct field access (`t.events[0].event_type`).
        //
        // sqlparser only folds a *pure* dot chain into `CompoundIdentifier`
        // -- the moment a `[...]` appears anywhere in the chain, `root`
        // stays just the single leading identifier (`t`) and every
        // subsequent dotted field name (`events`, `event_type`) lives in
        // `access_chain` as its own `AccessExpr::Dot`, not folded into
        // `root`. So `t.events[0].event_type` is
        // `CompoundFieldAccess { root: Identifier(t), access_chain:
        // [Dot(events), Subscript(0), Dot(event_type)] }` -- resolving
        // `root` alone would treat the table alias `t` itself as a bare
        // column reference. Instead, the leading run of plain-identifier
        // `Dot` entries (up to the first `Subscript`) is folded back onto
        // `root` into one path and resolved exactly like a
        // `CompoundIdentifier` of the same shape (base-column collapse,
        // same as above). Any `Dot` entries *after* the first `Subscript`
        // aren't attempted (there's no relation to resolve them against --
        // the value's actual type past a subscript isn't tracked). Every
        // subscript's own index/slice expressions are still walked, since
        // those can themselves reference a column (e.g. `arr[other_col]`),
        // the same way a function argument is.
        Expr::CompoundFieldAccess { root, access_chain } => {
            let mut path = match root.as_ref() {
                Expr::Identifier(ident) => Some(vec![ident.value.clone()]),
                Expr::CompoundIdentifier(parts) => {
                    Some(parts.iter().map(|p| p.value.clone()).collect())
                }
                other => {
                    collect_expr_sources_into(other, from_scope, catalog, found);
                    None
                }
            };

            let mut chain = access_chain.iter().peekable();
            if let Some(path) = &mut path {
                while let Some(AccessExpr::Dot(Expr::Identifier(ident))) = chain.peek() {
                    path.push(ident.value.clone());
                    chain.next();
                }
                if from_scope.contains_key(&path[0]) {
                    push_dedup(resolve_qualified(&path[0], &path[1..], from_scope, catalog), found);
                } else {
                    push_dedup(resolve_unqualified(path, from_scope, catalog), found);
                }
            }

            // Whatever's left in `chain` (any subscripts, plus any
            // non-identifier `Dot` access after the path-building run
            // above stopped) -- only subscripts' own index/slice
            // expressions are walked; see the doc comment above for why a
            // trailing non-identifier `Dot` isn't otherwise attempted.
            for access in chain {
                if let AccessExpr::Subscript(subscript) = access {
                    match subscript {
                        Subscript::Index { index } => {
                            collect_expr_sources_into(index, from_scope, catalog, found);
                        }
                        Subscript::Slice {
                            lower_bound,
                            upper_bound,
                            stride,
                        } => {
                            for bound in [lower_bound, upper_bound, stride].into_iter().flatten() {
                                collect_expr_sources_into(bound, from_scope, catalog, found);
                            }
                        }
                    }
                }
            }
        }
        // A window function (`OVER (...)`) is deliberately not attempted
        // (see the module-level "Known limitations" doc comment): its
        // `PARTITION BY`/`ORDER BY` columns aren't walked at all, so
        // tracing only the call's own arguments would silently report a
        // partial, misleadingly-confident source set. Skip the whole
        // expression instead.
        Expr::Function(f) if f.over.is_none() => {
            if let FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(arg_expr),
                        ..
                    } = arg
                    {
                        collect_expr_sources_into(arg_expr, from_scope, catalog, found);
                    }
                }
            }
        }
        Expr::Cast { expr: inner, .. }
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        // `EXTRACT(field FROM expr)`, `CEIL`/`FLOOR(expr [TO field])` --
        // sqlparser gives these their own `Expr` variants rather than
        // folding them into `Expr::Function`, so without this arm a
        // macro-expanded date/rounding call (a common real shape --
        // issue #34) fell through to the catch-all below and silently
        // lost its source, even though it's structurally no different
        // from any other single-argument function call.
        | Expr::Extract { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. }
        | Expr::Floor { expr: inner, .. } => {
            collect_expr_sources_into(inner, from_scope, catalog, found);
        }
        // `POSITION(expr IN expr)` -- same reasoning, two operands
        // instead of one.
        Expr::Position { expr: inner, r#in } => {
            collect_expr_sources_into(inner, from_scope, catalog, found);
            collect_expr_sources_into(r#in, from_scope, catalog, found);
        }
        // `SUBSTRING(expr [FROM expr] [FOR expr])` (or its comma-arg
        // form) -- another dedicated variant, same reasoning.
        Expr::Substring {
            expr: inner,
            substring_from,
            substring_for,
            ..
        } => {
            collect_expr_sources_into(inner, from_scope, catalog, found);
            if let Some(from) = substring_from {
                collect_expr_sources_into(from, from_scope, catalog, found);
            }
            if let Some(for_) = substring_for {
                collect_expr_sources_into(for_, from_scope, catalog, found);
            }
        }
        // `TRIM([BOTH|LEADING|TRAILING] [expr FROM] expr)` -- walks the
        // trimmed expression, the optional `what`-to-trim expression, and
        // any dialect-specific `trim_characters` list.
        Expr::Trim {
            expr: inner,
            trim_what,
            trim_characters,
            ..
        } => {
            collect_expr_sources_into(inner, from_scope, catalog, found);
            if let Some(what) = trim_what {
                collect_expr_sources_into(what, from_scope, catalog, found);
            }
            for c in trim_characters.iter().flatten() {
                collect_expr_sources_into(c, from_scope, catalog, found);
            }
        }
        // `OVERLAY(expr PLACING expr FROM expr [FOR expr])`.
        Expr::Overlay {
            expr: inner,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            collect_expr_sources_into(inner, from_scope, catalog, found);
            collect_expr_sources_into(overlay_what, from_scope, catalog, found);
            collect_expr_sources_into(overlay_from, from_scope, catalog, found);
            if let Some(for_) = overlay_for {
                collect_expr_sources_into(for_, from_scope, catalog, found);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_sources_into(left, from_scope, catalog, found);
            collect_expr_sources_into(right, from_scope, catalog, found);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_expr_sources_into(operand, from_scope, catalog, found);
            }
            for when in conditions {
                collect_expr_sources_into(&when.condition, from_scope, catalog, found);
                collect_expr_sources_into(&when.result, from_scope, catalog, found);
            }
            if let Some(else_result) = else_result {
                collect_expr_sources_into(else_result, from_scope, catalog, found);
            }
        }
        // Literals, subquery expressions, window functions, and anything
        // else: not attempted (see module-level "Known limitations").
        _ => {}
    }
}

/// Resolves a plain unqualified column reference -- possibly a nested
/// struct/`STRUCT`-field path (e.g. Databricks/Spark `payload.user_id`,
/// where `payload` is a struct-typed column, not a table alias). Returns
/// every source it carries when it resolves through an already-multi-
/// sourced calculated column (see [`source_of`]); returns nothing when
/// ambiguous among several relations in scope.
///
/// `path` is the full dotted identifier, e.g. `["payload", "user_id"]`.
/// See [`resolve_path_on_schema`] for how a multi-part path is matched.
fn resolve_unqualified(
    path: &[String],
    from_scope: &HashMap<String, LocalSchema>,
    catalog: &CatalogSchemas,
) -> Vec<(Upstream, String)> {
    if from_scope.len() == 1 {
        return match from_scope.values().next() {
            Some(only) => resolve_path_on_schema(only, path, catalog),
            None => Vec::new(),
        };
    }

    // A `Known` relation's columns are enumerated -- it either definitely
    // has this (possibly dotted) column or definitely doesn't, so a
    // `Known` hit is authoritative and wins over merely-possible matches.
    // Tried longest dotted prefix first (most specific -- an upstream
    // CTE/subquery whose own SELECT list literally aliases a dotted name,
    // e.g. `select payload.user_id as "payload.user_id" from ...`, is the
    // only way a `Known` relation's column list ever contains a dotted
    // name -- `schema.yml` docs never feed into `LocalSchema` at all, see
    // [`resolve_path_on_schema`]), falling back to shorter prefixes down
    // to just the base column name.
    for len in (1..=path.len()).rev() {
        let candidate = path[..len].join(".");
        let known_hits: Vec<&LocalSchema> = from_scope
            .values()
            .filter(|schema| matches!(schema, LocalSchema::Known(cols) if cols.iter().any(|c| column_names_match(&c.name, &candidate))))
            .collect();
        match known_hits.len() {
            1 => return source_of(known_hits[0], &candidate),
            n if n > 1 => return Vec::new(), // genuinely ambiguous among Known relations
            _ => {}
        }
    }

    // No `Known` relation claims any dotted prefix -- fall back to
    // `Passthrough` relations, whose real columns we can't enumerate to
    // either confirm or rule out, so only resolve if exactly one is in
    // scope (more than one is genuinely ambiguous). Collapses to the base
    // column name only -- see [`resolve_path_on_schema`] for why a
    // `Passthrough` never trusts a longer dotted name.
    let passthrough_hits: Vec<&Upstream> = from_scope
        .values()
        .filter_map(|schema| match schema {
            LocalSchema::Passthrough(upstream) => Some(upstream),
            _ => None,
        })
        .collect();
    match passthrough_hits.len() {
        1 => vec![(passthrough_hits[0].clone(), path[0].clone())],
        _ => Vec::new(),
    }
}

/// Resolves `path` (the identifier parts after the qualifier) against
/// whichever relation `qualifier` names in `from_scope` -- possibly a
/// nested struct-field path (e.g. `t.payload.user_id`, `t` a table alias,
/// `payload` a struct-typed column). See [`resolve_path_on_schema`].
fn resolve_qualified(
    qualifier: &str,
    path: &[String],
    from_scope: &HashMap<String, LocalSchema>,
    catalog: &CatalogSchemas,
) -> Vec<(Upstream, String)> {
    match from_scope.get(qualifier) {
        Some(schema) => resolve_path_on_schema(schema, path, catalog),
        None => Vec::new(),
    }
}

/// Resolves a (possibly multi-part, struct-field-accessing) dotted path
/// against a single already-identified relation.
///
/// - `Known`: tries the longest dotted prefix first (`path.join(".")`),
///   falling back to progressively shorter prefixes down to just
///   `path[0]` if no longer prefix is an actual column on this relation.
///   `Known` columns come entirely from the query's own already-resolved
///   CTEs/subqueries (never from `schema.yml`, which this SQL-parsing
///   layer never reads at all -- see `RawColumnDoc`'s own use, applied
///   only afterward to enrich already-resolved columns with a documented
///   `data_type`), so a dotted match here only ever fires when an
///   upstream CTE's own SELECT list literally aliased a dotted name
///   (`select payload.user_id as "payload.user_id" from ...`). Otherwise
///   this correctly falls through to `path[0]` -- e.g. `payload.user_id`
///   resolving to the base `payload` struct column when nothing in scope
///   is named exactly `"payload.user_id"`.
/// - `Passthrough`: an Origin's (or an unprocessed upstream Node's) real
///   columns are never known (see the module's "Known limitations" doc
///   comment), so there's no way to confirm whether a longer dotted name
///   is itself the real column -- always collapses to `path[0]`, the
///   base column, rather than guessing a longer name is correct.
/// - `Opaque`: never resolves, same as a single-part reference.
/// - `AmbiguousAlias`: resolves `path[0]` (the base column -- the same
///   collapse a `Passthrough` already does for a dotted path) only if
///   [`column_exists_on`] can confirm, for *every* candidate that shared
///   this alias, whether it has that column -- and exactly one of them
///   does. See [`LocalSchema::AmbiguousAlias`]'s doc comment.
fn resolve_path_on_schema(
    schema: &LocalSchema,
    path: &[String],
    catalog: &CatalogSchemas,
) -> Vec<(Upstream, String)> {
    match schema {
        LocalSchema::Known(_) => {
            for len in (1..=path.len()).rev() {
                let candidate = path[..len].join(".");
                let result = source_of(schema, &candidate);
                if !result.is_empty() {
                    return result;
                }
            }
            Vec::new()
        }
        LocalSchema::Passthrough(upstream) => vec![(upstream.clone(), path[0].clone())],
        LocalSchema::Opaque => Vec::new(),
        LocalSchema::AmbiguousAlias(candidates) => {
            let column = &path[0];
            let mut confirmed: Vec<&LocalSchema> = Vec::new();
            for candidate in candidates {
                match column_exists_on(candidate, column, catalog) {
                    Some(true) => confirmed.push(candidate),
                    Some(false) => {}
                    // A candidate catalog.json doesn't cover at all: we
                    // can't rule out it also has this column, so the whole
                    // reference stays unresolved -- see the doc comment
                    // above and `LocalSchema::AmbiguousAlias`'s.
                    None => return Vec::new(),
                }
            }
            match confirmed.len() {
                1 => resolve_path_on_schema(confirmed[0], path, catalog),
                _ => Vec::new(),
            }
        }
    }
}

/// Whether `schema` has a column named exactly `column`, when that's
/// knowable at all -- `Some(true)`/`Some(false)` when it is, `None` when
/// it isn't (an `Opaque`/`AmbiguousAlias` candidate, or a `Passthrough`
/// whose upstream isn't covered by `catalog`). Used only to disambiguate
/// an [`LocalSchema::AmbiguousAlias`]'s candidates -- a `Known` schema
/// answers from its own already-resolved column list; a `Passthrough`
/// (Node or Origin alike) answers from `catalog` alone.
///
/// Known, deliberately accepted gap: a `Passthrough(Node)` candidate
/// whose real columns are *already* resolved via `resolved_schemas`
/// (e.g. the ambiguous alias is a sibling model already fully known from
/// the manifest, not just from `catalog.json`) still can't be
/// disambiguated unless `catalog.json` *also* happens to cover it --
/// `resolved_schemas` isn't threaded this deep (through
/// `resolve_unqualified`/`resolve_qualified`/`resolve_path_on_schema`)
/// purely to widen this one narrow case. Never produces a *wrong*
/// answer, only a more conservative "stays unresolved" one than
/// theoretically possible.
fn column_exists_on(schema: &LocalSchema, column: &str, catalog: &CatalogSchemas) -> Option<bool> {
    let upstream_id = match schema {
        LocalSchema::Known(cols) => {
            return Some(cols.iter().any(|c| column_names_match(&c.name, column)));
        }
        LocalSchema::Passthrough(Upstream::Node(id)) => id.as_str(),
        LocalSchema::Passthrough(Upstream::Origin(id)) => id.as_str(),
        LocalSchema::Opaque | LocalSchema::AmbiguousAlias(_) => return None,
    };
    catalog
        .get(upstream_id)
        .map(|cols| cols.iter().any(|c| column_names_match(c, column)))
}

/// Whether `a` and `b` name the same column -- compared
/// case-insensitively (ASCII only; dbt-supported warehouses' unquoted
/// identifiers don't extend case-folding beyond ASCII), matching how an
/// unquoted identifier actually resolves on every warehouse zhao's SQL
/// dialects target (Snowflake, Databricks, BigQuery, ...): a model's SQL
/// commonly references a column in whatever case its author typed it
/// (`order_total`), while the real, catalog- or index-sourced column name
/// reflects however the warehouse itself stores it (`ORDER_TOTAL`, e.g. a
/// seed loaded from an upper-cased CSV header) -- an exact-case comparison
/// between the two would wrongly treat a real column as nonexistent. Used
/// everywhere a catalog- or index-derived real name is compared against a
/// SQL-parsed reference; never needed for two names both drawn from the
/// same compiled SQL text (already guaranteed to agree on case there).
///
/// Known, accepted trade-off: on a warehouse that allows two genuinely
/// distinct, case-differing quoted columns to coexist on the same
/// relation (e.g. Postgres's `"Total"` and `"total"` as separate,
/// deliberately-quoted columns), an unqualified reference to one now
/// matches both here -- callers ([`source_of`]'s `find`,
/// `column_exists_on`'s `any`) don't disambiguate, so this can pick
/// whichever the real-column list happens to list first rather than
/// reporting a genuine ambiguity. Accepted because that shape is rare (it
/// requires a project to deliberately create two case-only-differing
/// quoted columns on one relation) next to the alternative this fixes --
/// case-insensitive identifiers are the norm across every warehouse zhao
/// targets, so treating case as always significant would silently miss
/// far more real matches than this occasionally mis-picks.
fn column_names_match(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Every upstream source `column` traces to on `schema`. A `Passthrough`
/// always contributes exactly one (identity passthrough of a single
/// upstream relation). A `Known` relation looks up that column's own
/// already-resolved sources -- carrying forward however many there are,
/// which is how a reference to an earlier CTE's calculated column (e.g.
/// `SELECT cte1.total AS my_column FROM cte1`) ends up attributed to
/// whatever `cte1.total` itself resolved to, not to the trivial
/// passthrough reference.
fn source_of(schema: &LocalSchema, column: &str) -> Vec<(Upstream, String)> {
    match schema {
        LocalSchema::Passthrough(upstream) => vec![(upstream.clone(), column.to_string())],
        LocalSchema::Known(cols) => cols
            .iter()
            .find(|c| column_names_match(&c.name, column))
            .map(|c| c.sources.clone())
            .unwrap_or_default(),
        // `source_of` is only ever reached via a `Known` relation's own
        // already-resolved columns (see `resolve_path_on_schema`'s
        // `Known` arm and `resolve_unqualified`'s dotted-prefix search) --
        // never directly on an `Opaque` or `AmbiguousAlias` schema.
        LocalSchema::Opaque | LocalSchema::AmbiguousAlias(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------
// QueryExecutor: runs a WarehouseAdapter's relation-existence check via
// `dbt run-operation`, using whatever connection the project's own dbt
// profile already has -- never a connection zhao holds itself.
// ---------------------------------------------------------------------

/// The distinctive marker `DbtQueryExecutor` greps a `dbt run-operation`
/// invocation's stdout for -- dbt's own framing text around a `log(...,
/// info=True)` call varies by version (including between dbt v1 "classic
/// core" and v2 "Fusion," both of which zhao is expected to work
/// against), so this only ever looks for this one marker rather than
/// trying to parse dbt's full log format.
const RESULT_MARKER: &str = "ZHAO_RELATION_EXISTS_RESULT:";

/// The macro body written to a temporary file before `dbt run-operation`
/// runs, then removed -- zhao owns no macro namespace inside a user's
/// project, so a transient file is the only way to make an arbitrary
/// macro available to `run-operation` without requiring the user to
/// install anything (e.g. a dbt package) first. Uses `adapter.get_relation`
/// -- see `crate::adapters::warehouse`'s module doc comment for why this
/// is genuinely cross-warehouse and for the relation-cache caveat this
/// invocation path (a standalone `run-operation`, not a full `dbt run`)
/// avoids: the relations cache isn't populated ahead of time for
/// `run-operation`, so `get_relation` always takes its live,
/// cache-miss-triggered query path here rather than trusting a
/// potentially-stale cache.
const RELATION_EXISTS_MACRO_BODY: &str = r#"{% macro zhao_relation_exists(relation_database=none, relation_schema=none, relation_identifier=none) %}
  {% set relation = adapter.get_relation(database=relation_database, schema=relation_schema, identifier=relation_identifier) %}
  {{ log("ZHAO_RELATION_EXISTS_RESULT:" ~ ("true" if relation is not none else "false"), info=True) }}
{% endmacro %}
"#;

/// A [`QueryExecutor`] that runs `RELATION_EXISTS_MACRO` via `dbt
/// run-operation`. See [`DbtAdapter::compile`] for why
/// `dbt_command`/`extra_args` are parameters rather than hardcoded.
pub struct DbtQueryExecutor<'a> {
    /// The dbt project directory to run in.
    pub project_dir: &'a Path,
    /// The `dbt` executable to invoke.
    pub dbt_command: &'a str,
    /// Extra arguments (`--target`, `--vars`, ...) appended to the
    /// `run-operation` invocation -- the same passthrough zhao's other
    /// dbt invocations already support.
    pub extra_args: &'a [String],
}

impl QueryExecutor for DbtQueryExecutor<'_> {
    fn run_macro(
        &self,
        macro_name: &str,
        args: &HashMap<String, String>,
    ) -> Result<String, String> {
        if macro_name != RELATION_EXISTS_MACRO {
            return Err(format!(
                "DbtQueryExecutor only knows how to run {RELATION_EXISTS_MACRO:?}, not {macro_name:?}"
            ));
        }

        let macros_dir = self.project_dir.join("macros");
        fs::create_dir_all(&macros_dir)
            .map_err(|err| format!("could not create {}: {err}", macros_dir.display()))?;
        // Suffixed with this process's PID -- the macro file's *name*
        // never matters to dbt (it discovers macros by their declared
        // `{% macro %}` name, not by filename), only its content -- so a
        // unique-per-process name is free insurance against two
        // concurrent `--check-relations` invocations against the same
        // project directory racing each other's write/cleanup.
        let macro_path =
            macros_dir.join(format!("__zhao_relation_exists_{}.sql", std::process::id()));
        fs::write(&macro_path, RELATION_EXISTS_MACRO_BODY).map_err(|err| {
            format!(
                "could not write temporary macro at {}: {err}",
                macro_path.display()
            )
        })?;

        // Always remove the temporary macro file afterward, success or
        // failure -- it must never be left behind in the user's project.
        let result = self.run_operation(args);
        let _ = fs::remove_file(&macro_path);
        result
    }
}

impl DbtQueryExecutor<'_> {
    fn run_operation(&self, args: &HashMap<String, String>) -> Result<String, String> {
        let args_json = serde_json::to_string(args)
            .map_err(|err| format!("could not encode macro args as JSON: {err}"))?;

        let (program, prefix_args) =
            split_dbt_command(self.dbt_command).map_err(|err| err.to_string())?;
        let output = std::process::Command::new(&program)
            .args(&prefix_args)
            .arg("run-operation")
            .arg(RELATION_EXISTS_MACRO)
            .arg("--args")
            .arg(args_json)
            .args(self.extra_args)
            .current_dir(self.project_dir)
            .output()
            .map_err(|err| format!("could not run {:?}: {err}", self.dbt_command))?;

        if !output.status.success() {
            return Err(format!(
                "dbt run-operation {RELATION_EXISTS_MACRO} failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.split(RESULT_MARKER).nth(1))
            .map(|result| result.trim().to_string())
            .ok_or_else(|| {
                format!("dbt run-operation {RELATION_EXISTS_MACRO} produced no parseable result:\n{stdout}")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(id: &str) -> Upstream {
        Upstream::Origin(OriginId::new(id))
    }

    /// Regression test: two unaliased relations that happen to share a
    /// trailing name (e.g. `"s1"."t"` and `"s2"."t"`) must not let the
    /// second silently shadow the first in `from_scope` -- a reference to
    /// the shared alias should come out unresolved, not confidently (and
    /// wrongly) attributed to whichever one happened to be inserted last.
    #[test]
    fn duplicate_unaliased_relation_names_become_unresolved_not_silently_wrong() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s1".to_string(), "t".to_string()),
            origin("origin.s1.t"),
        );
        known_relations.insert(
            ("db".to_string(), "s2".to_string(), "t".to_string()),
            origin("origin.s2.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select t.x from "db"."s1"."t", "db"."s2"."t""#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert!(
                    cols[0].sources.is_empty(),
                    "an alias collision between two relations must not silently resolve to either one"
                );
            }
            other => panic!("expected Known([x]) with an unresolved source, got {other:?}"),
        }
    }

    /// Regression test: when one relation's columns are fully known (a CTE
    /// with an explicit projection list) and another is a passthrough of
    /// an upstream we can't enumerate, a column that's definitely on the
    /// known relation must resolve there -- not get discarded as
    /// "ambiguous" just because the passthrough can't be ruled out.
    #[test]
    fn known_relation_match_wins_over_an_unruled_out_passthrough_candidate() {
        // "tbl" backs CTE "a", which has an explicit projection list -- so
        // "a" resolves to `Known([id, name])` in the outer query's scope,
        // not a `Passthrough`. "t2" is a raw base-table reference, which
        // always resolves to `Passthrough` (its real columns are never
        // enumerated). The outer query's unqualified `name` must resolve
        // to the `Known` side, since that's the only one we can actually
        // confirm has a `name` column.
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "tbl".to_string()),
            origin("origin.s.tbl"),
        );
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t2".to_string()),
            origin("origin.s.t2"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"with a as (select id, name from "db"."s"."tbl") select name from a, "db"."s"."t2""#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "name");
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.tbl"), "name".to_string())]
                );
            }
            other => panic!("expected Known([name sourced from tbl via CTE a]), got {other:?}"),
        }
    }

    /// A two-arm `UNION` resolves each result column to the union of what
    /// *both* arms resolve that same positional column to -- at runtime the
    /// value could come from either arm, so both are real lineage, not just
    /// the left arm's.
    #[test]
    fn a_two_arm_union_resolves_each_column_to_both_arms_sources() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t1".to_string()),
            origin("origin.s.t1"),
        );
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t2".to_string()),
            origin("origin.s.t2"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select a, b from "db"."s"."t1" union select a, b from "db"."s"."t2""#)
                .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].name, "a");
                assert_eq!(
                    cols[0].sources,
                    vec![
                        (origin("origin.s.t1"), "a".to_string()),
                        (origin("origin.s.t2"), "a".to_string()),
                    ]
                );
                assert_eq!(cols[1].name, "b");
                assert_eq!(
                    cols[1].sources,
                    vec![
                        (origin("origin.s.t1"), "b".to_string()),
                        (origin("origin.s.t2"), "b".to_string()),
                    ]
                );
            }
            other => panic!("expected Known([a, b] each sourced from both arms), got {other:?}"),
        }
    }

    /// `UNION ALL` resolves exactly like `UNION` for lineage purposes --
    /// the `ALL`/`DISTINCT` quantifier only affects duplicate-row handling,
    /// not which columns a value can come from.
    #[test]
    fn a_union_all_resolves_each_column_to_both_arms_sources() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t1".to_string()),
            origin("origin.s.t1"),
        );
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t2".to_string()),
            origin("origin.s.t2"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select a from "db"."s"."t1" union all select a from "db"."s"."t2""#)
                .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![
                        (origin("origin.s.t1"), "a".to_string()),
                        (origin("origin.s.t2"), "a".to_string()),
                    ]
                );
            }
            other => panic!("expected Known([a] sourced from both arms), got {other:?}"),
        }
    }

    /// An inline subquery directly in `FROM` (as opposed to a
    /// `WITH`-defined CTE) resolves its own `SELECT` list as a nested
    /// scope, then is treated exactly like a CTE's `Known` schema by the
    /// outer query.
    #[test]
    fn an_inline_from_subquery_resolves_correctly() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"select inner_alias.id as outer_id from (select id from "db"."s"."t") as inner_alias"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "outer_id");
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "id".to_string())]
                );
            }
            other => panic!("expected Known([outer_id] sourced from t.id), got {other:?}"),
        }
    }

    /// Regression guard: a table-valued function in `FROM` (as opposed to a
    /// plain table/CTE reference) must still correctly fall back to
    /// `Opaque` now that `UNION` and inline `FROM` subqueries resolve --
    /// this case stays deliberately unsolved (see the module-level "Known
    /// limitations" doc comment).
    #[test]
    fn a_table_valued_function_in_from_still_falls_back_to_opaque() {
        let known_relations = HashMap::new();
        let resolved_schemas = HashMap::new();

        let query = parse_query("select * from generate_series(1, 10) as g").expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        assert!(
            matches!(schema, LocalSchema::Opaque),
            "expected Opaque for a table-valued function in FROM, got {schema:?}"
        );
    }

    /// Regression test: a table-valued function call in `FROM` whose bare
    /// name happens to collide with an in-scope CTE's name (a legal SQL
    /// namespace collision -- a TVF call and a table/CTE reference are
    /// distinguished by the presence of `(...)`, not by name) must not
    /// silently resolve to that CTE's schema.
    #[test]
    fn a_table_valued_function_sharing_a_ctes_name_does_not_resolve_to_that_cte() {
        let known_relations = HashMap::new();
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            "with generate_series as (select id from t) \
             select g.id from generate_series(1, 10) as g",
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert!(
                    cols[0].sources.is_empty(),
                    "a table-valued function call must never resolve as though it were a \
                     reference to a same-named CTE, got {:?}",
                    cols[0].sources
                );
            }
            other => panic!("expected Known([id]) with an unresolved source, got {other:?}"),
        }
    }

    /// `SELECT *` reading directly from a source resolves correctly when a
    /// `catalog.json` is present and covers that source's real column
    /// list -- otherwise unknowable from `manifest.json` alone (see the
    /// module-level doc comment).
    #[test]
    fn a_wildcard_from_a_source_expands_via_catalog_when_present() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();
        let mut catalog = CatalogSchemas::new();
        catalog.insert(
            "origin.s.t".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );

        let query = parse_query(r#"select * from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        match schema {
            LocalSchema::Passthrough(upstream) => {
                assert_eq!(upstream, origin("origin.s.t"));
            }
            other => panic!(
                "expected the top-level SELECT * to stay a Passthrough (expansion happens in \
                 build_parsed_project), got {other:?}"
            ),
        }
    }

    /// The same `SELECT *` from a source, with no `catalog.json` at all
    /// (an empty [`CatalogSchemas`], exactly what [`read_catalog`]
    /// produces when the file is absent), falls back to today's existing
    /// behavior -- a `Passthrough` that never gets enumerated into
    /// concrete columns.
    #[test]
    fn a_wildcard_from_a_source_falls_back_to_opaque_columns_without_catalog() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select * from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        assert!(
            expand_wildcard_of(&schema, &resolved_schemas, &CatalogSchemas::new()).is_none(),
            "expected no catalog to mean the wildcard still can't be expanded, got {schema:?}"
        );
    }

    /// Regression test: a catalog.json entry that's *present* for a
    /// source but lists zero columns must fall back to `Opaque`, the same
    /// as an entirely-absent entry -- not `Known([])`, which would
    /// silently misrepresent "catalog.json didn't actually introspect
    /// this relation's real columns" as "this relation genuinely has no
    /// columns."
    #[test]
    fn a_wildcard_from_a_source_falls_back_to_opaque_when_the_catalog_entry_is_empty() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();
        let mut catalog = CatalogSchemas::new();
        catalog.insert("origin.s.t".to_string(), Vec::new());

        let query = parse_query(r#"select * from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        assert!(
            expand_wildcard_of(&schema, &resolved_schemas, &catalog).is_none(),
            "expected an empty-but-present catalog entry to still mean the wildcard can't be \
             expanded, got {schema:?}"
        );
    }

    /// A real column reference resolves against a catalog- or
    /// index-sourced real column name even when the two disagree on case
    /// (`ORDER_TOTAL` in the catalog, `order_total` in the SQL) -- the
    /// common shape on a case-insensitive warehouse (Databricks,
    /// Snowflake, ...) where a seed loaded from an upper-cased CSV header
    /// is referenced in lower-case, unquoted SQL. Applies equally to
    /// dbt-core's `catalog.json` and dbt Fusion's parquet index, since
    /// both feed the same [`CatalogSchemas`].
    #[test]
    fn a_wildcard_from_a_source_expands_and_matches_regardless_of_case() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();
        let mut catalog = CatalogSchemas::new();
        catalog.insert("origin.s.t".to_string(), vec!["ORDER_TOTAL".to_string()]);

        let query = parse_query(r#"select order_total from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "order_total".to_string())],
                    "a lower-case SQL reference should still resolve against the catalog's \
                     upper-cased real column name, got {cols:?}"
                );
            }
            other => {
                panic!("expected Known([order_total] resolved case-insensitively), got {other:?}")
            }
        }
    }

    /// A backtick-quoted, fully-qualified relation reference (MySQL's/
    /// Databricks'/Spark's own quoting -- how dbt-databricks always
    /// renders a compiled relation) matches `known_relations` the same as
    /// a double-quoted one already does. Before this, only `"` was
    /// stripped from a parsed identifier, so a backtick-quoted relation
    /// never matched `known_relations`' quote-free keys at all --
    /// silently breaking every Databricks project's column-level lineage.
    #[test]
    fn a_backtick_quoted_relation_reference_resolves_the_same_as_a_double_quoted_one() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();
        let catalog = CatalogSchemas::new();

        let query = parse_query_with_dialect(
            "select * from `db`.`s`.`t`",
            resolve_sql_dialect(Some("databricks")).as_ref(),
        )
        .expect("should parse under DatabricksDialect");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        match schema {
            LocalSchema::Passthrough(upstream) => assert_eq!(upstream, origin("origin.s.t")),
            other => panic!(
                "expected a backtick-quoted relation to resolve to the known Origin, not \
                 Opaque, got {other:?}"
            ),
        }
    }

    /// Two ambiguously-aliased relations (a duplicate `FROM` alias) still
    /// resolve a qualified column reference deterministically when
    /// `catalog.json` shows the column exists on only one of them.
    #[test]
    fn a_duplicate_alias_resolves_via_catalog_when_the_column_is_on_only_one_relation() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s1".to_string(), "t".to_string()),
            origin("origin.s1.t"),
        );
        known_relations.insert(
            ("db".to_string(), "s2".to_string(), "t".to_string()),
            origin("origin.s2.t"),
        );
        let resolved_schemas = HashMap::new();
        let mut catalog = CatalogSchemas::new();
        catalog.insert("origin.s1.t".to_string(), vec!["x".to_string()]);
        catalog.insert("origin.s2.t".to_string(), vec!["y".to_string()]);

        let query =
            parse_query(r#"select t.x from "db"."s1"."t", "db"."s2"."t""#).expect("should parse");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s1.t"), "x".to_string())],
                    "expected t.x to resolve deterministically to s1.t, the only relation whose \
                     catalog.json column list has an x column"
                );
            }
            other => panic!("expected Known([x] resolved via catalog), got {other:?}"),
        }
    }

    /// The same duplicate-alias case, but the column exists on *both*
    /// relations per `catalog.json` -- must still correctly fall back to
    /// unresolved, the same as the no-catalog regression test above,
    /// rather than arbitrarily picking one.
    #[test]
    fn a_duplicate_alias_stays_unresolved_via_catalog_when_the_column_is_on_both_relations() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s1".to_string(), "t".to_string()),
            origin("origin.s1.t"),
        );
        known_relations.insert(
            ("db".to_string(), "s2".to_string(), "t".to_string()),
            origin("origin.s2.t"),
        );
        let resolved_schemas = HashMap::new();
        let mut catalog = CatalogSchemas::new();
        catalog.insert("origin.s1.t".to_string(), vec!["x".to_string()]);
        catalog.insert("origin.s2.t".to_string(), vec!["x".to_string()]);

        let query =
            parse_query(r#"select t.x from "db"."s1"."t", "db"."s2"."t""#).expect("should parse");
        let schema = resolve_query(&query, &known_relations, &resolved_schemas, &catalog);

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert!(
                    cols[0].sources.is_empty(),
                    "a column present on both ambiguously-aliased relations must not resolve to \
                     either one, even with catalog.json coverage"
                );
            }
            other => panic!("expected Known([x]) with an unresolved source, got {other:?}"),
        }
    }

    /// End-to-end through [`build_parsed_project`] and
    /// [`crate::diff::diff`]/[`crate::rules::evaluate`]: a struct-internal
    /// field added between two versions of a model is now correctly
    /// classified as a breaking change (`RuleId::StructFieldAdded`), even
    /// though the struct column itself is only ever *renamed* in this
    /// model's own outermost `SELECT` -- its real shape is defined two CTE
    /// hops earlier. Before struct-shape propagation, that column's
    /// `struct_fields` would have been `None` in both the Baseline and
    /// current state, so this change would have gone entirely undetected.
    #[test]
    fn a_struct_field_added_behind_a_cte_hop_is_classified_as_a_breaking_change() {
        let manifest_json = |fields: &str| {
            format!(
                r#"{{
                    "nodes": {{
                        "model.p.m": {{
                            "unique_id": "model.p.m",
                            "resource_type": "model",
                            "name": "m",
                            "database": "db",
                            "schema": "s",
                            "alias": "m",
                            "depends_on": {{"nodes": ["source.p.raw.t"]}},
                            "compiled_code": "with base as (select cast(x.payload as struct<{fields}>) as payload from \"db\".\"raw\".\"t\" as x) select b.payload as renamed_payload from base as b"
                        }}
                    }},
                    "sources": {{
                        "source.p.raw.t": {{
                            "unique_id": "source.p.raw.t",
                            "name": "t",
                            "database": "db",
                            "schema": "raw",
                            "identifier": "t"
                        }}
                    }}
                }}"#
            )
        };

        let baseline_manifest: RawManifest =
            serde_json::from_str(&manifest_json("user_id int64")).expect("should parse");
        let current_manifest: RawManifest =
            serde_json::from_str(&manifest_json("user_id int64, email string"))
                .expect("should parse");

        let baseline = build_parsed_project(&baseline_manifest, &CatalogSchemas::new());
        let current = build_parsed_project(&current_manifest, &CatalogSchemas::new());

        let changes = crate::diff::diff(&baseline, &current);
        let findings =
            crate::rules::evaluate(&baseline, &changes, &crate::config::Config::default());

        let node_id = NodeId::new("model.p.m");
        let expected = crate::rules::Finding {
            severity: crate::rules::Severity::Error,
            detail: crate::rules::FindingDetail::StructFieldAdded {
                node: node_id,
                column: ColumnName::new("renamed_payload"),
                field: ColumnName::new("email"),
            },
        };
        assert!(
            findings.contains(&expected),
            "expected a StructFieldAdded finding for renamed_payload.email, got {findings:?}"
        );
    }

    /// End-to-end through [`build_parsed_project`] (not just
    /// [`resolve_query`]): a model that's a pure `SELECT * FROM <source>`
    /// gets its real columns -- and one identity-passthrough edge per
    /// column -- only once a `catalog.json` covering that source is
    /// supplied; an empty one (what a missing/unparseable file falls back
    /// to, see [`read_catalog`]) reproduces today's existing empty-columns
    /// behavior exactly.
    #[test]
    fn build_parsed_project_expands_a_sources_wildcard_model_only_with_catalog() {
        let manifest: RawManifest = serde_json::from_str(
            r#"{
                "nodes": {
                    "model.p.m": {
                        "unique_id": "model.p.m",
                        "resource_type": "model",
                        "name": "m",
                        "database": "db",
                        "schema": "s",
                        "alias": "m",
                        "depends_on": {"nodes": ["source.p.raw.t"]},
                        "compiled_code": "select * from \"db\".\"raw\".\"t\""
                    }
                },
                "sources": {
                    "source.p.raw.t": {
                        "unique_id": "source.p.raw.t",
                        "name": "t",
                        "database": "db",
                        "schema": "raw",
                        "identifier": "t"
                    }
                }
            }"#,
        )
        .expect("manifest fixture should parse");

        let without_catalog = build_parsed_project(&manifest, &CatalogSchemas::new());
        let model = without_catalog
            .nodes
            .iter()
            .find(|n| n.name == "m")
            .expect("model m should exist");
        assert!(
            model.columns.is_empty(),
            "expected no columns without a catalog, got {:?}",
            model.columns
        );

        let mut catalog = CatalogSchemas::new();
        catalog.insert(
            "source.p.raw.t".to_string(),
            vec!["id".to_string(), "amount".to_string()],
        );
        let with_catalog = build_parsed_project(&manifest, &catalog);
        let model = with_catalog
            .nodes
            .iter()
            .find(|n| n.name == "m")
            .expect("model m should exist");
        let column_names: Vec<&str> = model.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(column_names, vec!["id", "amount"]);

        let origin_id = OriginId::new("source.p.raw.t");
        let node_id = NodeId::new("model.p.m");
        for column in ["id", "amount"] {
            let has_edge = with_catalog.edges.iter().any(|e| {
                e.upstream == Upstream::Origin(origin_id.clone())
                    && e.downstream == node_id
                    && e.column.as_ref().is_some_and(|c| {
                        c.upstream_column.as_str() == column
                            && c.downstream_column.as_str() == column
                    })
            });
            assert!(has_edge, "expected an identity edge for {column}");
        }
    }

    /// [`DbtAdapter::parse_for_comparison`] with `use_catalog: false`
    /// forces `Opaque`-style wildcard behavior even when a real,
    /// non-empty `catalog.json` is sitting right there -- the whole point
    /// being to let a caller suppress catalog usage on demand for a
    /// two-sided comparison, regardless of what's actually on disk. See
    /// [`DbtAdapter::catalog_available`] for the presence check callers
    /// use to decide.
    #[test]
    fn parse_for_comparison_suppresses_catalog_expansion_when_told_to() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            r#"{
                "nodes": {
                    "model.p.m": {
                        "unique_id": "model.p.m",
                        "resource_type": "model",
                        "name": "m",
                        "database": "db",
                        "schema": "s",
                        "alias": "m",
                        "depends_on": {"nodes": ["source.p.raw.t"]},
                        "compiled_code": "select * from \"db\".\"raw\".\"t\""
                    }
                },
                "sources": {
                    "source.p.raw.t": {
                        "unique_id": "source.p.raw.t",
                        "name": "t",
                        "database": "db",
                        "schema": "raw",
                        "identifier": "t"
                    }
                }
            }"#,
        )
        .expect("should write stub manifest.json");
        std::fs::write(
            dir.path().join("catalog.json"),
            r#"{
                "sources": {
                    "source.p.raw.t": {
                        "columns": {
                            "id": {"name": "id", "index": 1}
                        }
                    }
                }
            }"#,
        )
        .expect("should write stub catalog.json");

        assert!(
            DbtAdapter.catalog_available(&manifest_path),
            "expected the stub catalog.json to count as available"
        );

        let suppressed = DbtAdapter
            .parse_for_comparison(&manifest_path, false)
            .expect("should parse");
        let model = suppressed
            .nodes
            .iter()
            .find(|n| n.name == "m")
            .expect("model m should exist");
        assert!(
            model.columns.is_empty(),
            "expected use_catalog: false to suppress catalog-backed expansion even though a \
             real catalog.json exists, got {:?}",
            model.columns
        );

        let allowed = DbtAdapter
            .parse_for_comparison(&manifest_path, true)
            .expect("should parse");
        let model = allowed
            .nodes
            .iter()
            .find(|n| n.name == "m")
            .expect("model m should exist");
        assert_eq!(
            model
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id"],
            "expected use_catalog: true to expand normally"
        );
    }

    /// [`DbtAdapter::catalog_available`] is `false` for a project with no
    /// `catalog.json` at all -- the common case for a Baseline compiled
    /// in a throwaway worktree that never runs `dbt docs generate`.
    #[test]
    fn catalog_available_is_false_with_no_catalog_json() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");

        assert!(!DbtAdapter.catalog_available(&manifest_path));
    }

    /// [`read_catalog`] is fully optional: no sibling `catalog.json` at
    /// all produces an empty [`CatalogSchemas`], not an error.
    #[test]
    fn read_catalog_returns_empty_when_no_catalog_file_exists() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");

        let catalog = read_catalog(&manifest_path);

        assert!(catalog.is_empty());
    }

    /// A `catalog.json` that fails to parse as valid JSON (or doesn't
    /// match the expected shape) degrades the same way a missing file
    /// does -- empty, not an error.
    #[test]
    fn read_catalog_returns_empty_when_the_file_is_not_valid_json() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(dir.path().join("catalog.json"), "not valid json")
            .expect("should write stub catalog.json");

        let catalog = read_catalog(&manifest_path);

        assert!(catalog.is_empty());
    }

    /// A valid sibling `catalog.json` is read and flattened into real
    /// column lists, keyed by `unique_id`, ordered by each column's own
    /// `index` (not object-map iteration order, which JSON doesn't
    /// guarantee).
    #[test]
    fn read_catalog_reads_a_valid_sibling_catalog_json() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(
            dir.path().join("catalog.json"),
            r#"{
                "nodes": {
                    "model.p.m": {
                        "columns": {
                            "NAME": {"name": "name", "type": "varchar", "index": 2},
                            "ID": {"name": "id", "type": "integer", "index": 1}
                        }
                    }
                },
                "sources": {
                    "source.p.raw.t": {
                        "columns": {
                            "AMOUNT": {"name": "amount", "type": "numeric", "index": 1}
                        }
                    }
                }
            }"#,
        )
        .expect("should write stub catalog.json");

        let catalog = read_catalog(&manifest_path);

        assert_eq!(
            catalog.get("model.p.m"),
            Some(&vec!["id".to_string(), "name".to_string()])
        );
        assert_eq!(
            catalog.get("source.p.raw.t"),
            Some(&vec!["amount".to_string()])
        );
    }

    /// Writes a minimal, valid `dbt.node_columns.parquet` fixture at
    /// `path` -- three real columns ([`read_node_columns_parquet`] cares
    /// about) plus one it doesn't (`inferred_type`), and column rows
    /// deliberately out of `column_index` order, to prove selection is by
    /// name (not position) and rows are re-sorted rather than trusted as
    /// already ordered.
    fn write_node_columns_fixture(path: &Path, rows: &[(&str, &str, i64, &str)]) {
        use parquet::data_type::ByteArray;
        use parquet::file::properties::WriterProperties;
        use parquet::file::writer::SerializedFileWriter;
        use parquet::schema::parser::parse_message_type;
        use std::sync::Arc;

        std::fs::create_dir_all(path.parent().expect("fixture path should have a parent"))
            .expect("should create fixture's parent dir");

        let schema = Arc::new(
            parse_message_type(
                "message schema {
                    REQUIRED BYTE_ARRAY unique_id (UTF8);
                    REQUIRED BYTE_ARRAY column_name (UTF8);
                    REQUIRED INT64 column_index;
                    REQUIRED BYTE_ARRAY inferred_type (UTF8);
                }",
            )
            .expect("schema literal should parse"),
        );
        let file = fs::File::create(path).expect("should create fixture file");
        let mut writer = SerializedFileWriter::new(file, schema, Arc::new(WriterProperties::new()))
            .expect("should create parquet writer");
        let mut row_group = writer.next_row_group().expect("should open a row group");

        let unique_ids: Vec<ByteArray> = rows
            .iter()
            .map(|(id, _, _, _)| ByteArray::from(*id))
            .collect();
        let mut col = row_group
            .next_column()
            .expect("should open unique_id column")
            .expect("unique_id column should exist");
        col.typed::<parquet::data_type::ByteArrayType>()
            .write_batch(&unique_ids, None, None)
            .expect("should write unique_id column");
        col.close().expect("should close unique_id column");

        let names: Vec<ByteArray> = rows
            .iter()
            .map(|(_, name, _, _)| ByteArray::from(*name))
            .collect();
        let mut col = row_group
            .next_column()
            .expect("should open column_name column")
            .expect("column_name column should exist");
        col.typed::<parquet::data_type::ByteArrayType>()
            .write_batch(&names, None, None)
            .expect("should write column_name column");
        col.close().expect("should close column_name column");

        let indices: Vec<i64> = rows.iter().map(|(_, _, index, _)| *index).collect();
        let mut col = row_group
            .next_column()
            .expect("should open column_index column")
            .expect("column_index column should exist");
        col.typed::<parquet::data_type::Int64Type>()
            .write_batch(&indices, None, None)
            .expect("should write column_index column");
        col.close().expect("should close column_index column");

        let types: Vec<ByteArray> = rows
            .iter()
            .map(|(_, _, _, ty)| ByteArray::from(*ty))
            .collect();
        let mut col = row_group
            .next_column()
            .expect("should open inferred_type column")
            .expect("inferred_type column should exist");
        col.typed::<parquet::data_type::ByteArrayType>()
            .write_batch(&types, None, None)
            .expect("should write inferred_type column");
        col.close().expect("should close inferred_type column");

        row_group.close().expect("should close row group");
        writer.close().expect("should close parquet writer");
    }

    /// [`read_node_columns_parquet`] is fully optional, same bar as
    /// [`read_catalog_json`]: no index file at all produces an empty
    /// [`CatalogSchemas`], not an error.
    #[test]
    fn read_node_columns_parquet_returns_empty_when_no_index_file_exists() {
        let dir = tempfile::tempdir().expect("should create temp dir");

        let schemas = read_node_columns_parquet(&dir.path().join("index/dbt.node_columns.parquet"));

        assert!(schemas.is_empty());
    }

    /// A file that exists at the expected path but isn't valid parquet
    /// degrades the same way a missing file does -- empty, not a panic or
    /// an error.
    #[test]
    fn read_node_columns_parquet_returns_empty_when_the_file_is_not_valid_parquet() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let index_path = dir.path().join("index").join("dbt.node_columns.parquet");
        fs::create_dir_all(index_path.parent().unwrap()).expect("should create index dir");
        fs::write(&index_path, b"not a parquet file").expect("should write stub index file");

        let schemas = read_node_columns_parquet(&index_path);

        assert!(schemas.is_empty());
    }

    /// A valid `dbt.node_columns.parquet` is read and flattened into real
    /// column lists, keyed by `unique_id`, ordered by each row's own
    /// `column_index` (not row-write order, which the fixture deliberately
    /// scrambles) -- selected by field *name*, so the fixture's extra
    /// `inferred_type` column (present in every real Fusion build, unused
    /// here) doesn't interfere.
    #[test]
    fn read_node_columns_parquet_reads_a_valid_index() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let index_path = dir.path().join("index").join("dbt.node_columns.parquet");
        write_node_columns_fixture(
            &index_path,
            &[
                ("seed.p.raw_orders", "customer", 1, "Utf8"),
                ("seed.p.raw_orders", "order_total", 6, "Int64"),
                ("seed.p.raw_orders", "id", 0, "Utf8"),
                ("model.p.stg_orders", "order_id", 0, "Utf8"),
            ],
        );

        let schemas = read_node_columns_parquet(&index_path);

        assert_eq!(
            schemas.get("seed.p.raw_orders"),
            Some(&vec![
                "id".to_string(),
                "customer".to_string(),
                "order_total".to_string()
            ])
        );
        assert_eq!(
            schemas.get("model.p.stg_orders"),
            Some(&vec!["order_id".to_string()])
        );
    }

    /// [`read_catalog`] prefers a real `catalog.json` over the Fusion
    /// parquet index whenever both happen to be present (e.g. a `target/`
    /// left over from a project mid-migration from dbt-core to Fusion) --
    /// a warehouse-verified schema must never be displaced by a statically
    /// inferred one.
    #[test]
    fn read_catalog_prefers_catalog_json_when_both_are_present() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(
            dir.path().join("catalog.json"),
            r#"{"nodes": {"seed.p.raw_orders": {"columns": {"A": {"name": "from_catalog_json", "index": 0}}}}, "sources": {}}"#,
        )
        .expect("should write catalog.json");
        write_node_columns_fixture(
            &dir.path().join("index").join("dbt.node_columns.parquet"),
            &[("seed.p.raw_orders", "from_parquet", 0, "Utf8")],
        );

        let catalog = read_catalog(&manifest_path);

        assert_eq!(
            catalog.get("seed.p.raw_orders"),
            Some(&vec!["from_catalog_json".to_string()])
        );
    }

    /// [`read_catalog`] falls back to the Fusion parquet index when no
    /// `catalog.json` is present at all -- the common Fusion case, since
    /// Fusion never writes one.
    #[test]
    fn read_catalog_falls_back_to_node_columns_parquet_when_no_catalog_json() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let manifest_path = dir.path().join("manifest.json");
        write_node_columns_fixture(
            &dir.path().join("index").join("dbt.node_columns.parquet"),
            &[("seed.p.raw_orders", "order_total", 0, "Int64")],
        );

        let catalog = read_catalog(&manifest_path);

        assert_eq!(
            catalog.get("seed.p.raw_orders"),
            Some(&vec!["order_total".to_string()])
        );
    }

    /// [`resolve_sql_dialect`] picks a recognized adapter type's dedicated
    /// `sqlparser` dialect (matched case-insensitively), and falls back to
    /// [`GenericDialect`] for anything it doesn't recognize -- including
    /// no `adapter_type` at all.
    #[test]
    fn resolve_sql_dialect_matches_known_adapter_types_case_insensitively() {
        use std::any::TypeId;

        assert_eq!(
            resolve_sql_dialect(Some("Snowflake")).dialect(),
            TypeId::of::<SnowflakeDialect>()
        );
        assert_eq!(
            resolve_sql_dialect(Some("bigquery")).dialect(),
            TypeId::of::<BigQueryDialect>()
        );
        assert_eq!(
            resolve_sql_dialect(Some("DATABRICKS")).dialect(),
            TypeId::of::<DatabricksDialect>()
        );
        assert_eq!(
            resolve_sql_dialect(Some("some-unknown-adapter")).dialect(),
            TypeId::of::<GenericDialect>()
        );
        assert_eq!(
            resolve_sql_dialect(None).dialect(),
            TypeId::of::<GenericDialect>()
        );
    }

    /// Databricks'/Spark's parenthesized multi-column alias `SELECT`
    /// syntax (`expr AS (a, b, c)`) -- gated behind
    /// `Dialect::supports_select_item_multi_column_alias`, which
    /// `resolve_sql_dialect` now correctly selects `DatabricksDialect`
    /// for -- resolves each aliased output column to whatever the shared
    /// expression structurally references, instead of the whole query
    /// falling back to `Opaque` the way `SelectItem::ExprWithAliases`
    /// used to unconditionally.
    #[test]
    fn a_multi_column_alias_select_resolves_under_databricks_dialect() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query_with_dialect(
            r#"select x.payload as (a, b) from "db"."s"."t" as x"#,
            resolve_sql_dialect(Some("databricks")).as_ref(),
        )
        .expect("should parse under DatabricksDialect");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].name, "a");
                assert_eq!(cols[1].name, "b");
                for col in &cols {
                    assert_eq!(
                        col.sources,
                        vec![(origin("origin.s.t"), "payload".to_string())],
                        "expected both aliases to share the tuple-expansion \
                         expression's own traced sources"
                    );
                }
            }
            other => panic!("expected Known([a, b] both sourced from payload), got {other:?}"),
        }

        // Unlike a genuinely dialect-exclusive construct, this syntax
        // also parses under `GenericDialect` (it enables the same
        // `supports_select_item_multi_column_alias` flag) -- confirming
        // the fix here is `resolve_select`'s own handling of
        // `ExprWithAliases`, not merely dialect selection succeeding
        // where it previously failed to parse at all.
        assert!(parse_query(r#"select x.payload as (a, b) from "db"."s"."t" as x"#).is_some());
    }

    /// The same syntax genuinely fails to parse under `SnowflakeDialect`
    /// -- it does not enable
    /// `Dialect::supports_select_item_multi_column_alias` -- confirming
    /// this construct is Databricks'/Spark's, not Snowflake's, contrary
    /// to what an earlier version of this code's comments assumed.
    #[test]
    fn a_multi_column_alias_select_does_not_parse_under_snowflake_dialect() {
        assert!(
            parse_query_with_dialect(
                r#"select x.payload as (a, b) from "db"."s"."t" as x"#,
                &SnowflakeDialect,
            )
            .is_none()
        );
    }

    /// A calculated column that references two distinct upstream columns
    /// (`x.a + x.b`) resolves to *both* sources, not zero and not an
    /// arbitrary pick of one -- and its rendered SQL is recorded as its
    /// expression.
    #[test]
    fn a_calculated_column_over_two_distinct_columns_resolves_to_both_sources() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select x.a + x.b as total from "db"."s"."t" as x"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "total");
                assert_eq!(
                    cols[0].sources,
                    vec![
                        (origin("origin.s.t"), "a".to_string()),
                        (origin("origin.s.t"), "b".to_string()),
                    ]
                );
                assert!(
                    cols[0].expression.is_some(),
                    "a calculated column should carry its rendered SQL"
                );
            }
            other => panic!("expected Known([total sourced from a and b]), got {other:?}"),
        }
    }

    /// Issue #34's repro: a macro that expands to `EXTRACT(field FROM
    /// expr)` -- sqlparser gives `EXTRACT` its own dedicated `Expr`
    /// variant rather than folding it into a generic function call
    /// (unlike `coalesce`/`round`/etc.), so before this fix it fell
    /// through to the unresolved catch-all despite structurally
    /// referencing a single, perfectly resolvable upstream column.
    #[test]
    fn a_macro_expanded_extract_call_resolves_its_source_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select extract(year from x.created_at) as year from "db"."s"."t" as x"#)
                .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "year");
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "created_at".to_string())]
                );
            }
            other => panic!("expected Known([year sourced from created_at]), got {other:?}"),
        }
    }

    /// The same gap, for the other dedicated `Expr` variants sqlparser
    /// carves out of what would otherwise read as ordinary function
    /// calls: `TRIM`, `SUBSTRING`, `POSITION`, `CEIL`/`FLOOR`, `OVERLAY`
    /// -- all common shapes for string-cleaning/date-rounding dbt
    /// macros. Each should resolve its inner column reference(s) the
    /// same as a plain function call would.
    #[test]
    fn other_dedicated_sql_expr_variants_resolve_their_source_columns() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let cases: &[(&str, &str)] = &[
            (
                r#"select trim(x.raw_name) as clean_name from "db"."s"."t" as x"#,
                "raw_name",
            ),
            (
                r#"select substring(x.code from 1 for 3) as prefix from "db"."s"."t" as x"#,
                "code",
            ),
            (
                r#"select position('a' in x.text) as idx from "db"."s"."t" as x"#,
                "text",
            ),
            (
                r#"select ceil(x.amount) as rounded from "db"."s"."t" as x"#,
                "amount",
            ),
            (
                r#"select floor(x.amount) as rounded from "db"."s"."t" as x"#,
                "amount",
            ),
        ];

        for (sql, expected_column) in cases {
            let query = parse_query(sql).expect("should parse");
            let schema = resolve_query(
                &query,
                &known_relations,
                &resolved_schemas,
                &CatalogSchemas::new(),
            );
            match schema {
                LocalSchema::Known(cols) => {
                    assert_eq!(cols.len(), 1, "{sql}");
                    assert_eq!(
                        cols[0].sources,
                        vec![(origin("origin.s.t"), expected_column.to_string())],
                        "{sql}"
                    );
                }
                other => panic!("{sql}: expected a resolved source, got {other:?}"),
            }
        }
    }

    /// No regression in the already-working macro-adjacent case this
    /// ticket explicitly called out: a plain function call
    /// (`round(x / 100.0, 2)`, `zhao-dbt-test`'s `cents_to_dollars`
    /// shape) keeps resolving exactly as before.
    #[test]
    fn a_plain_function_call_macro_shape_still_resolves_unaffected() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select round(x.amount / 100.0, 2) as amount from "db"."s"."t" as x"#)
                .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "amount".to_string())]
                );
            }
            other => panic!("expected Known([amount sourced from amount]), got {other:?}"),
        }
    }

    /// A plain (optionally qualified) identifier reference -- a
    /// passthrough or rename, not a calculation -- carries no expression
    /// text: there's nothing more informative to show than "this is that
    /// column."
    #[test]
    fn a_plain_identifier_reference_carries_no_expression() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select x.a as renamed from "db"."s"."t" as x"#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].expression, None);
            }
            other => panic!("expected Known([renamed]), got {other:?}"),
        }
    }

    /// A `CASE` expression's every branch (operand, each `WHEN`/`THEN`
    /// pair, and `ELSE`) is walked for column references.
    #[test]
    fn a_case_expression_collects_sources_from_every_branch() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"select case when x.a > 0 then x.b else x.c end as result from "db"."s"."t" as x"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![
                        (origin("origin.s.t"), "a".to_string()),
                        (origin("origin.s.t"), "b".to_string()),
                        (origin("origin.s.t"), "c".to_string()),
                    ]
                );
            }
            other => panic!("expected Known([result sourced from a, b, c]), got {other:?}"),
        }
    }

    /// A window function is not attempted at all -- its `PARTITION
    /// BY`/`ORDER BY` clause isn't walked, so tracing only its own
    /// argument would silently report a partial, misleadingly-confident
    /// source set (see the module-level "Known limitations" doc
    /// comment).
    #[test]
    fn a_window_function_stays_entirely_unresolved() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"select sum(x.a) over (partition by x.b) as running_total from "db"."s"."t" as x"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert!(
                    cols[0].sources.is_empty(),
                    "a window function should stay fully unresolved, not partially traced: {:?}",
                    cols[0].sources
                );
            }
            other => panic!("expected Known([running_total, unresolved]), got {other:?}"),
        }
    }

    /// A column referenced more than once in the same expression (e.g.
    /// `coalesce(x.a, x.a)`) is only reported once -- `collect_expr_sources`
    /// dedupes rather than double-counting.
    #[test]
    fn a_repeated_column_reference_is_deduplicated() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select x.a + x.a as doubled from "db"."s"."t" as x"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "a".to_string())]
                );
            }
            other => panic!("expected Known([doubled sourced from a once]), got {other:?}"),
        }
    }

    /// An unqualified column ambiguous among several relations in scope
    /// contributes nothing to a larger expression, rather than a wrong
    /// guess at which relation it meant.
    /// Databricks/Spark, BigQuery, and DuckDB all compile a `STRUCT`
    /// column's nested field access to plain dot notation -- indistinguishable
    /// at the parser level from `table.column`. A qualified 3-part chain
    /// (`t.payload.user_id`) must resolve to the base `payload` column on
    /// `t`, not fall through unresolved the way a `parts.len() == 2`-only
    /// guard would.
    #[test]
    fn a_qualified_struct_field_access_resolves_to_the_base_struct_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select t.payload.user_id from "db"."s"."t" as t"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "user_id");
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "payload".to_string())],
                    "a struct field access should trace to its base struct column"
                );
            }
            other => panic!("expected Known([user_id sourced from payload]), got {other:?}"),
        }
    }

    /// The same struct field access, but with no table alias at all --
    /// `payload` isn't a real relation alias, so `payload.user_id` must
    /// fall back to unqualified resolution (treating `payload` as the
    /// base column) rather than being dropped as an unresolvable
    /// qualifier lookup.
    #[test]
    fn an_unqualified_struct_field_access_resolves_to_the_base_struct_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select payload.user_id from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "payload".to_string())]
                );
            }
            other => panic!("expected Known([user_id sourced from payload]), got {other:?}"),
        }
    }

    /// Snowflake's (and Databricks') semi-structured `VARIANT` colon
    /// access (`t.payload:user_id`) parses as a distinct `JsonAccess`
    /// expression wrapping a `CompoundIdentifier`, not as a longer
    /// `CompoundIdentifier` itself -- must still resolve to the base
    /// `payload` column, dropping the JSON path.
    #[test]
    fn a_qualified_variant_colon_access_resolves_to_the_base_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select t.payload:user_id from "db"."s"."t" as t"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "payload".to_string())]
                );
            }
            other => panic!("expected Known([.. sourced from payload]), got {other:?}"),
        }
    }

    /// Same colon access, unqualified -- single table in scope, `payload`
    /// resolved directly as the base column.
    #[test]
    fn an_unqualified_variant_colon_access_resolves_to_the_base_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query =
            parse_query(r#"select payload:user_id from "db"."s"."t""#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "payload".to_string())]
                );
            }
            other => panic!("expected Known([.. sourced from payload]), got {other:?}"),
        }
    }

    /// A struct field access chained after an array subscript (a common
    /// BigQuery/DuckDB/Databricks shape for `ARRAY<STRUCT<...>>` columns,
    /// e.g. an `events` column holding an array of event structs) parses
    /// as `CompoundFieldAccess`, not `CompoundIdentifier` -- must still
    /// trace back to the base `events` column.
    #[test]
    fn a_struct_field_access_after_an_array_subscript_resolves_to_the_base_column() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select t.events[0].event_type from "db"."s"."t" as t"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "events".to_string())]
                );
            }
            other => panic!("expected Known([.. sourced from events]), got {other:?}"),
        }
    }

    /// A subscript's own index expression can itself reference a column
    /// (`arr[other_col]`) -- must be traced the same way a function
    /// argument is, not silently dropped as part of "the subscript key
    /// isn't attempted."
    #[test]
    fn a_column_referenced_inside_a_subscript_index_is_traced() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select t.arr[t.idx] as picked from "db"."s"."t" as t"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "picked");
                assert!(
                    cols[0]
                        .sources
                        .contains(&(origin("origin.s.t"), "arr".to_string()))
                );
                assert!(
                    cols[0]
                        .sources
                        .contains(&(origin("origin.s.t"), "idx".to_string()))
                );
            }
            other => panic!("expected Known([picked sourced from arr and idx]), got {other:?}"),
        }
    }

    /// If an upstream CTE's own SELECT list happens to alias a column
    /// under a literal dotted name matching the full struct-field path,
    /// that more specific match wins over collapsing to the base column --
    /// the longest-dotted-prefix-first search in `resolve_path_on_schema`.
    #[test]
    fn an_exact_dotted_alias_on_an_upstream_cte_wins_over_the_base_column_collapse() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"with a as (select payload.user_id as "payload.user_id", payload as payload from "db"."s"."t")
               select x.payload.user_id from a as x"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s.t"), "payload".to_string())],
                    "should resolve through the CTE's own \"payload.user_id\" alias (itself \
                     sourced from the base \"payload\" column), not collapse straight to the \
                     CTE's separate \"payload\" passthrough column"
                );
            }
            other => panic!("expected Known([.. resolved via the dotted alias]), got {other:?}"),
        }
    }

    #[test]
    fn an_ambiguous_sub_reference_inside_a_larger_expression_contributes_nothing() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s1".to_string(), "t".to_string()),
            origin("origin.s1.t"),
        );
        known_relations.insert(
            ("db".to_string(), "s2".to_string(), "u".to_string()),
            origin("origin.s2.u"),
        );
        let resolved_schemas = HashMap::new();

        // "shared" is ambiguous between t and u (neither is `Known`, so
        // both are `Passthrough` candidates); "x.a" is unambiguous.
        let query = parse_query(
            r#"select x.a + shared as result from "db"."s1"."t" as x, "db"."s2"."u" as y"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].sources,
                    vec![(origin("origin.s1.t"), "a".to_string())],
                    "the ambiguous half of the expression should contribute nothing, not a guess"
                );
            }
            other => panic!("expected Known([result sourced from a only]), got {other:?}"),
        }
    }

    /// A reference to an earlier CTE's own multi-sourced calculated column
    /// carries forward *all* of that column's sources, however many CTE
    /// hops away it was actually computed -- not just the trivial
    /// passthrough reference in the outer query.
    #[test]
    fn a_multi_sourced_calculated_column_propagates_through_a_cte_hop() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"with a as (select x.a + x.b as total from "db"."s"."t" as x) select a.total as my_column from a"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "my_column");
                assert_eq!(
                    cols[0].sources,
                    vec![
                        (origin("origin.s.t"), "a".to_string()),
                        (origin("origin.s.t"), "b".to_string()),
                    ],
                    "my_column should be attributed to what actually computed total, not to the passthrough reference"
                );
            }
            other => {
                panic!("expected Known([my_column sourced from a and b via CTE]), got {other:?}")
            }
        }
    }

    /// Writes an executable shell script to a fresh temp dir and returns
    /// its path -- stands in for a real `dbt` binary so `compile`'s tests
    /// don't depend on whether a real `dbt` happens to be installed
    /// wherever they run.
    #[cfg(unix)]
    fn stub_dbt_command(dir: &Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("dbt");
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("should write stub dbt script");
        let mut perms = fs::metadata(&path)
            .expect("should stat stub script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("should chmod stub script");
        // A brief pause before this script gets exec'd by whichever test
        // called this: on CI runners using overlayfs (every GitHub-hosted
        // Linux runner's container root), a file just written and
        // chmod'd can still intermittently report `ETXTBSY`/"Text file
        // busy" for a few milliseconds if it's exec'd immediately --
        // overlayfs's own copy-up bookkeeping lagging behind the fd
        // actually closing, not anything wrong with the script itself.
        // Seen intermittently in CI as `compile`'s tests spuriously
        // getting `CommandNotFound { source: ExecutableFileBusy }`
        // instead of actually running. This is test-only: real `dbt`
        // installs are long-existing binaries nothing is concurrently
        // writing to, so this race is specific to writing-then-
        // immediately-executing a throwaway script in the same process,
        // never a real production scenario worth handling in
        // `TransformationToolAdapter::compile` itself.
        std::thread::sleep(std::time::Duration::from_millis(50));
        path
    }

    #[cfg(unix)]
    #[test]
    fn compile_runs_the_configured_dbt_command_in_the_project_dir() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            "mkdir -p target && echo '{}' > target/manifest.json",
        );

        DbtAdapter
            .compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("compile should succeed");

        assert!(
            project_dir
                .path()
                .join("target")
                .join("manifest.json")
                .exists(),
            "the stub dbt's output should have landed inside the project dir"
        );
    }

    /// Issue #36's core change: a successful `compile()` call's captured
    /// stdout/stderr is returned on `Ok`, not discarded -- previously
    /// there was no way for a caller to route it into the run log at
    /// all, even though it was already sitting in memory during the
    /// run.
    #[cfg(unix)]
    #[test]
    fn compile_returns_its_captured_stdout_and_stderr_on_success() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            "mkdir -p target && echo '{}' > target/manifest.json\n\
             echo 'compiling...'\n\
             echo 'a warning' >&2",
        );

        let output = DbtAdapter
            .compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("compile should succeed");

        assert!(
            output.stdout.contains("compiling..."),
            "{:?}",
            output.stdout
        );
        assert!(output.stderr.contains("a warning"), "{:?}", output.stderr);
    }

    #[cfg(unix)]
    #[test]
    fn compile_reports_a_clear_error_when_dbt_compile_fails() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'boom' >&2\nexit 1");

        let result = DbtAdapter.compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[]);

        match result {
            Err(DbtAdapterError::CompileFailed { stderr, .. }) => {
                assert!(stderr.contains("boom"), "stderr should surface: {stderr:?}");
            }
            other => panic!("expected CompileFailed, got {other:?}"),
        }
    }

    /// dbt logs most of its actual error detail to stdout, not stderr --
    /// stderr alone is routinely empty even on a real compile failure.
    /// The error's `Display` (what a user actually sees) must include
    /// stdout, not just stderr, or the real reason a compile failed is
    /// silently dropped.
    #[cfg(unix)]
    #[test]
    fn compile_reports_a_clear_error_when_dbts_real_error_is_only_on_stdout() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            "echo 'Compilation Error: column does not exist'\nexit 1",
        );

        let result = DbtAdapter.compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[]);
        let message: Option<String> = result.as_ref().err().map(ToString::to_string);

        match result {
            Err(DbtAdapterError::CompileFailed { stdout, .. }) => {
                assert!(
                    stdout.contains("Compilation Error: column does not exist"),
                    "stdout should surface: {stdout:?}"
                );
                let message = message.expect("should be an error");
                assert!(
                    message.contains("Compilation Error: column does not exist"),
                    "the error's own Display should include dbt's real error, not just \
                     project_dir: {message:?}"
                );
            }
            other => panic!("expected CompileFailed, got {other:?}"),
        }
    }

    /// `printf` (unlike `echo`) writes no trailing newline -- proves the
    /// `Display` impl's own separator, not an assumption that dbt's
    /// output always happens to end in `\n`, is what keeps captured
    /// stdout and stderr from fusing into one garbled line when both are
    /// non-empty.
    #[cfg(unix)]
    #[test]
    fn compile_error_message_separates_stdout_from_stderr_even_without_a_trailing_newline() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            "printf 'STDOUT_END'\necho 'STDERR_START' >&2\nexit 1",
        );

        let result = DbtAdapter.compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[]);
        let message: Option<String> = result.as_ref().err().map(ToString::to_string);

        match result {
            Err(DbtAdapterError::CompileFailed { .. }) => {
                let message = message.expect("should be an error");
                assert!(
                    !message.contains("STDOUT_ENDSTDERR_START"),
                    "stdout and stderr should never be fused into one line: {message:?}"
                );
                assert!(message.contains("STDOUT_END"), "{message:?}");
                assert!(message.contains("STDERR_START"), "{message:?}");
            }
            other => panic!("expected CompileFailed, got {other:?}"),
        }
    }

    #[test]
    fn compile_reports_a_clear_error_when_the_command_cannot_be_run_at_all() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");

        let result = DbtAdapter.compile(
            project_dir.path(),
            "definitely-not-a-real-command-zhao-test",
            &[],
        );

        assert!(matches!(
            result,
            Err(DbtAdapterError::CommandNotFound { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn compile_appends_extra_args_after_the_subcommand() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        // Echoes its own argv (minus argv[0]) to a file the test can read
        // back, so this proves the exact args dbt actually received --
        // not just that compile() returned Ok.
        let dbt = stub_dbt_command(stub_dir.path(), "echo \"$@\" > args.txt");

        DbtAdapter
            .compile(
                project_dir.path(),
                dbt.to_str().expect("utf8 path"),
                &["--target".to_string(), "ci".to_string()],
            )
            .expect("compile should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "compile --target ci");
    }

    /// A stub `dbt` that answers `--version` like a real Fusion install
    /// (`dbt-fusion 2.0.0-preview.218`) and otherwise just echoes its
    /// own argv to `args.txt`, the same recording trick
    /// `compile_appends_extra_args_after_the_subcommand` uses.
    #[cfg(unix)]
    fn stub_fusion_dbt_command(dir: &Path) -> std::path::PathBuf {
        stub_dbt_command(
            dir,
            r#"if [ "$1" = "--version" ]; then echo "dbt-fusion 2.0.0-preview.218"; exit 0; fi
echo "$@" > args.txt"#,
        )
    }

    /// [`DbtAdapter::compile`]'s core new behavior: when `dbt --version`
    /// reports Fusion, `--write-index --static-analysis strict` are
    /// appended -- without this, Fusion never writes the column-schema
    /// index `read_node_columns_parquet` depends on.
    #[cfg(unix)]
    #[test]
    fn compile_appends_write_index_flags_when_dbt_reports_fusion() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_fusion_dbt_command(stub_dir.path());

        DbtAdapter
            .compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("compile should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(
            recorded_args.trim(),
            "compile --write-index --static-analysis strict"
        );
    }

    /// The dbt-core case (today's existing stub scripts never answer
    /// `--version` with anything Fusion-shaped): no flags are added --
    /// dbt-core doesn't recognize `--write-index`/`--static-analysis`
    /// and would fail outright if they were.
    #[cfg(unix)]
    #[test]
    fn compile_does_not_append_write_index_flags_for_dbt_core() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        // A dbt-core-shaped --version answer -- no mention of Fusion.
        let dbt = stub_dbt_command(
            stub_dir.path(),
            r#"if [ "$1" = "--version" ]; then echo "Core: - installed: 1.7.4"; exit 0; fi
echo "$@" > args.txt"#,
        );

        DbtAdapter
            .compile(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("compile should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "compile");
    }

    /// A user who already configured their own `--write-index` (via
    /// `zhao.yml`'s `dbt-args`) never gets a duplicate appended, even
    /// against a Fusion install.
    #[cfg(unix)]
    #[test]
    fn compile_does_not_duplicate_write_index_when_already_configured() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_fusion_dbt_command(stub_dir.path());

        DbtAdapter
            .compile(
                project_dir.path(),
                dbt.to_str().expect("utf8 path"),
                &["--write-index".to_string()],
            )
            .expect("compile should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "compile --write-index");
    }

    /// [`DbtAdapter::deps`] never gets the Fusion index flags -- `dbt
    /// deps` doesn't compile or write any index at all, so there's
    /// nothing for them to do there.
    #[cfg(unix)]
    #[test]
    fn deps_does_not_append_write_index_flags_even_for_fusion() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_fusion_dbt_command(stub_dir.path());

        DbtAdapter
            .deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("deps should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "deps");
    }

    #[cfg(unix)]
    #[test]
    fn deps_runs_the_configured_dbt_command_in_the_project_dir() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo \"$@\" > args.txt");

        DbtAdapter
            .deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("deps should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "deps");
    }

    /// A multi-word `dbt_command` (e.g. `"uv run dbt"`, or a custom
    /// wrapper a project's own tooling already uses instead of invoking
    /// `dbt` directly) works as a genuine prefix -- the wrapper's own
    /// leading flags land before the subcommand, not swallowed into one
    /// literal (nonexistent) executable name.
    #[cfg(unix)]
    #[test]
    fn a_multi_word_dbt_command_is_shell_split_into_a_program_plus_prefix_args() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo \"$@\" > args.txt");
        let dbt_command = format!("{} --wrapper-flag", dbt.to_str().expect("utf8 path"));

        DbtAdapter
            .deps(project_dir.path(), &dbt_command, &[])
            .expect("deps should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "--wrapper-flag deps");
    }

    /// A `dbt-command` that's a real, existing file path -- e.g. an
    /// absolute path into a venv's `Scripts`/`bin` directory, the
    /// standard way to disambiguate multiple installed dbt versions
    /// (see this module's own doc comment on why case-insensitive
    /// column matching and dialect resolution exist) -- is used
    /// directly, never run through shell-word splitting at all. Proven
    /// here with a space in the containing directory name (a routine
    /// occurrence on Windows -- anything under `OneDrive\Documents`,
    /// `Program Files`, ...): shell-word splitting a raw path
    /// containing a space would mangle it into two garbage fragments at
    /// that space, long before it ever reached `Command::new`, no
    /// matter how the value was quoted in `zhao.yml`.
    #[cfg(unix)]
    #[test]
    fn a_dbt_command_that_is_itself_an_existing_path_is_used_directly_even_with_a_space_in_it() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_root = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = stub_root
            .path()
            .join("dbt test")
            .join(".venv")
            .join("Scripts");
        fs::create_dir_all(&stub_dir).expect("should create nested stub dir");
        let dbt = stub_dbt_command(&stub_dir, "echo \"$@\" > args.txt");

        DbtAdapter
            .deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("deps should succeed -- the space-containing path must resolve as one token");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "deps");
    }

    /// Same as `compile`'s equivalent (see issue #36): a successful
    /// `deps()` call's captured output is returned, not discarded.
    #[cfg(unix)]
    #[test]
    fn deps_returns_its_captured_stdout_and_stderr_on_success() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            "echo 'installing packages...'\necho 'a deps warning' >&2",
        );

        let output = DbtAdapter
            .deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[])
            .expect("deps should succeed");

        assert!(
            output.stdout.contains("installing packages..."),
            "{:?}",
            output.stdout
        );
        assert!(
            output.stderr.contains("a deps warning"),
            "{:?}",
            output.stderr
        );
    }

    #[cfg(unix)]
    #[test]
    fn deps_appends_extra_args_after_the_subcommand() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo \"$@\" > args.txt");

        DbtAdapter
            .deps(
                project_dir.path(),
                dbt.to_str().expect("utf8 path"),
                &["--target".to_string(), "ci".to_string()],
            )
            .expect("deps should succeed");

        let recorded_args =
            fs::read_to_string(project_dir.path().join("args.txt")).expect("should read args.txt");
        assert_eq!(recorded_args.trim(), "deps --target ci");
    }

    #[cfg(unix)]
    #[test]
    fn deps_reports_a_clear_error_when_dbt_deps_fails() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'boom' >&2\nexit 1");

        let result = DbtAdapter.deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[]);

        match result {
            Err(DbtAdapterError::DepsFailed { stderr, .. }) => {
                assert!(stderr.contains("boom"), "stderr should surface: {stderr:?}");
            }
            other => panic!("expected DepsFailed, got {other:?}"),
        }
    }

    /// Same reasoning as `compile`'s equivalent test: dbt's real error
    /// detail routinely lands on stdout, not stderr.
    #[cfg(unix)]
    #[test]
    fn deps_reports_a_clear_error_when_dbts_real_error_is_only_on_stdout() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'Could not resolve package'\nexit 1");

        let result = DbtAdapter.deps(project_dir.path(), dbt.to_str().expect("utf8 path"), &[]);
        let message: Option<String> = result.as_ref().err().map(ToString::to_string);

        match result {
            Err(DbtAdapterError::DepsFailed { stdout, .. }) => {
                assert!(
                    stdout.contains("Could not resolve package"),
                    "stdout should surface: {stdout:?}"
                );
                let message = message.expect("should be an error");
                assert!(
                    message.contains("Could not resolve package"),
                    "the error's own Display should include dbt's real error: {message:?}"
                );
            }
            other => panic!("expected DepsFailed, got {other:?}"),
        }
    }

    #[test]
    fn materialization_maps_recognized_dbt_materialized_strings() {
        assert_eq!(materialization(Some("table")), Materialization::Table);
        assert_eq!(materialization(Some("view")), Materialization::View);
        assert_eq!(
            materialization(Some("incremental")),
            Materialization::Incremental
        );
        assert_eq!(
            materialization(Some("ephemeral")),
            Materialization::Ephemeral
        );
    }

    #[test]
    fn materialization_defaults_to_view_when_absent() {
        assert_eq!(materialization(None), Materialization::View);
    }

    #[test]
    fn materialization_preserves_an_unrecognized_string_verbatim() {
        assert_eq!(
            materialization(Some("materialized_view")),
            Materialization::Other("materialized_view".to_string())
        );
    }

    fn write_manifest(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("manifest.json");
        fs::write(&path, contents).expect("should write manifest");
        path
    }

    #[test]
    fn adapter_type_reads_the_manifests_metadata() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = write_manifest(
            dir.path(),
            r#"{"nodes": {}, "sources": {}, "metadata": {"adapter_type": "snowflake"}}"#,
        );

        assert_eq!(
            DbtAdapter.adapter_type(&path).expect("should parse"),
            Some("snowflake".to_string())
        );
    }

    #[test]
    fn adapter_type_is_none_when_metadata_is_absent() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = write_manifest(dir.path(), r#"{"nodes": {}, "sources": {}}"#);

        assert_eq!(DbtAdapter.adapter_type(&path).expect("should parse"), None);
    }

    #[test]
    fn relation_identities_reads_each_models_qualified_name() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = write_manifest(
            dir.path(),
            r#"{
                "nodes": {
                    "model.p.a": {
                        "unique_id": "model.p.a",
                        "resource_type": "model",
                        "name": "a",
                        "database": "analytics",
                        "schema": "public",
                        "alias": "a"
                    },
                    "model.p.b": {
                        "unique_id": "model.p.b",
                        "resource_type": "model",
                        "name": "b"
                    },
                    "seed.p.c": {
                        "unique_id": "seed.p.c",
                        "resource_type": "seed",
                        "name": "c",
                        "database": "analytics",
                        "schema": "public",
                        "alias": "c"
                    }
                },
                "sources": {},
                "metadata": {}
            }"#,
        );

        let identities = DbtAdapter.relation_identities(&path).expect("should parse");

        assert_eq!(
            identities.get("model.p.a"),
            Some(&RelationIdentity {
                database: Some("analytics".to_string()),
                schema: Some("public".to_string()),
                identifier: "a".to_string(),
            })
        );
        assert!(
            !identities.contains_key("model.p.b"),
            "a model missing database/schema/alias should be skipped, not defaulted"
        );
        assert!(
            !identities.contains_key("seed.p.c"),
            "a non-model resource type should never appear, even with a full qualified name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dbt_query_executor_runs_run_operation_and_parses_the_result() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(
            stub_dir.path(),
            r#"echo "$@" > invocation.txt
echo 'ZHAO_RELATION_EXISTS_RESULT:true'
"#,
        );

        let executor = DbtQueryExecutor {
            project_dir: project_dir.path(),
            dbt_command: dbt.to_str().expect("utf8 path"),
            extra_args: &[],
        };
        let mut args = HashMap::new();
        args.insert(
            "relation_identifier".to_string(),
            "dim_customers".to_string(),
        );

        let result = executor
            .run_macro(RELATION_EXISTS_MACRO, &args)
            .expect("should succeed");
        assert_eq!(result, "true");

        let invocation = fs::read_to_string(project_dir.path().join("invocation.txt"))
            .expect("should read invocation.txt");
        assert!(invocation.contains("run-operation"), "{invocation}");
        assert!(invocation.contains(RELATION_EXISTS_MACRO), "{invocation}");
        assert!(invocation.contains("dim_customers"), "{invocation}");
    }

    #[cfg(unix)]
    #[test]
    fn dbt_query_executor_always_removes_the_temporary_macro_file() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'ZHAO_RELATION_EXISTS_RESULT:false'");

        let executor = DbtQueryExecutor {
            project_dir: project_dir.path(),
            dbt_command: dbt.to_str().expect("utf8 path"),
            extra_args: &[],
        };
        executor
            .run_macro(RELATION_EXISTS_MACRO, &HashMap::new())
            .expect("should succeed");

        assert!(
            fs::read_dir(project_dir.path().join("macros"))
                .expect("macros dir should exist")
                .next()
                .is_none(),
            "the temporary macro file must not be left behind in the project directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dbt_query_executor_removes_the_macro_file_even_when_run_operation_fails() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'boom' >&2\nexit 1");

        let executor = DbtQueryExecutor {
            project_dir: project_dir.path(),
            dbt_command: dbt.to_str().expect("utf8 path"),
            extra_args: &[],
        };
        let result = executor.run_macro(RELATION_EXISTS_MACRO, &HashMap::new());

        assert!(result.is_err());
        assert!(
            fs::read_dir(project_dir.path().join("macros"))
                .expect("macros dir should exist")
                .next()
                .is_none(),
            "the temporary macro file must not be left behind even on failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dbt_query_executor_rejects_an_unknown_macro_name() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let executor = DbtQueryExecutor {
            project_dir: project_dir.path(),
            dbt_command: "dbt",
            extra_args: &[],
        };

        let result = executor.run_macro("some_other_macro", &HashMap::new());
        assert!(result.is_err());
    }

    #[test]
    fn node_display_name_extracts_the_bare_name_from_a_unique_id() {
        assert_eq!(
            DbtVocabulary.node_display_name("model.zhao_dbt_test.stg_customers"),
            "stg_customers"
        );
    }

    #[test]
    fn node_display_name_falls_back_to_the_whole_string_if_there_is_no_dot() {
        assert_eq!(
            DbtVocabulary.node_display_name("stg_customers"),
            "stg_customers"
        );
    }

    // -----------------------------------------------------------------
    // STRUCT internal field shape extraction (issue #53).
    // -----------------------------------------------------------------

    fn struct_field(name: &str, data_type: Option<&str>) -> StructField {
        StructField {
            name: ColumnName::new(name),
            data_type: data_type.map(str::to_string),
        }
    }

    /// A `CAST(... AS STRUCT<...>)` on Databricks/Spark-shaped compiled
    /// SQL extracts every field's name and type.
    #[test]
    fn a_cast_to_struct_extracts_its_named_and_typed_fields() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"select cast(x.raw_payload as struct<user_id bigint, name string>) as payload from "db"."s"."t" as x"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "payload");
                assert_eq!(
                    cols[0].struct_fields,
                    Some(vec![
                        struct_field("user_id", Some("BIGINT")),
                        struct_field("name", Some("STRING")),
                    ])
                );
            }
            other => panic!("expected Known([payload with a struct shape]), got {other:?}"),
        }
    }

    /// BigQuery/Databricks' typeless `STRUCT(expr AS name, ...)`
    /// constructor extracts each explicitly-named field, with no type
    /// (a constructor's value only ever states a name, never a type).
    #[test]
    fn a_typeless_struct_constructor_extracts_its_named_fields_with_no_type() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"select struct(x.user_id as user_id, x.name as name) as payload from "db"."s"."t" as x"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "payload");
                assert_eq!(
                    cols[0].struct_fields,
                    Some(vec![
                        struct_field("user_id", None),
                        struct_field("name", None)
                    ])
                );
            }
            other => panic!("expected Known([payload with a struct shape]), got {other:?}"),
        }
    }

    /// BigQuery's typed `STRUCT<field_name field_type, ...>(expr1, ...)`
    /// constructor extracts each field's name and type from the type
    /// definition itself, not the values.
    #[test]
    fn a_typed_struct_constructor_extracts_its_named_and_typed_fields() {
        let query =
            parse_query("select struct<user_id int64, name string>(1, 'a') as payload from t")
                .expect("should parse");
        let known_relations: HashMap<QualifiedName, Upstream> = HashMap::new();
        let resolved_schemas = HashMap::new();
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "payload");
                let fields = cols[0]
                    .struct_fields
                    .as_ref()
                    .expect("typed struct constructor should produce a known shape");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, ColumnName::new("user_id"));
                assert_eq!(fields[1].name, ColumnName::new("name"));
            }
            other => panic!("expected Known([payload with a struct shape]), got {other:?}"),
        }
    }

    /// Databricks'/Spark's `named_struct('field', expr, ...)` constructor
    /// extracts every field name from its literal key arguments.
    #[test]
    fn a_named_struct_call_extracts_its_named_fields() {
        let query = parse_query(
            "select named_struct('user_id', x.id, 'name', x.full_name) as payload from t as x",
        )
        .expect("should parse");
        let known_relations: HashMap<QualifiedName, Upstream> = HashMap::new();
        let resolved_schemas = HashMap::new();
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "payload");
                assert_eq!(
                    cols[0].struct_fields,
                    Some(vec![
                        struct_field("user_id", None),
                        struct_field("name", None)
                    ])
                );
            }
            other => panic!("expected Known([payload with a struct shape]), got {other:?}"),
        }
    }

    /// Acceptance criterion (c): the overwhelmingly common case -- a
    /// struct-typed column simply passed through (here, a plain rename
    /// with no `CAST`/constructor in the immediate SQL) -- must produce
    /// no struct shape at all, not a guessed empty one. `struct_fields`
    /// stays a real `None`, exactly the same "unknown, not empty"
    /// contract `Column::data_type` already has for an undocumented type.
    #[test]
    fn a_plain_passthrough_column_produces_no_struct_shape() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(r#"select x.payload as payload from "db"."s"."t" as x"#)
            .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "payload");
                assert_eq!(
                    cols[0].struct_fields, None,
                    "a plain passthrough/rename must never produce a guessed struct shape"
                );
            }
            other => panic!("expected Known([payload with no struct shape]), got {other:?}"),
        }
    }

    /// Unlike a rename sourced from a base table (above, where the real
    /// shape is genuinely never known), a rename sourced from an earlier
    /// CTE that itself had an explicit struct shape now carries that
    /// shape forward -- the same way a scalar column's `sources` already
    /// carry forward across a CTE hop.
    #[test]
    fn a_struct_shape_propagates_forward_across_a_cte_hop() {
        let mut known_relations = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "t".to_string()),
            origin("origin.s.t"),
        );
        let resolved_schemas = HashMap::new();

        let query = parse_query(
            r#"with base as (
                select cast(x.payload as struct<user_id int64, name string>) as payload
                from "db"."s"."t" as x
            )
            select b.payload as renamed_payload from base as b"#,
        )
        .expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "renamed_payload");
                assert_eq!(
                    cols[0].struct_fields,
                    Some(vec![
                        struct_field("user_id", Some("INT64")),
                        struct_field("name", Some("STRING")),
                    ]),
                    "expected the CTE's own explicit struct shape to survive both the CTE hop \
                     and the rename"
                );
            }
            other => panic!(
                "expected Known([renamed_payload carrying base.payload's struct shape]), got \
                 {other:?}"
            ),
        }
    }

    /// A wildcard expanded alongside another projection item (forcing
    /// `expand_wildcard_of`'s enumeration path, rather than the pure
    /// `SELECT * FROM <one thing>` shortcut that just propagates a
    /// `Passthrough` unchanged) never carries forward an upstream
    /// column's struct shape, even when the upstream column itself had
    /// one -- wildcard expansion only ever has resolved column *names* to
    /// work with (`resolved_schemas: HashMap<NodeId, Vec<ColumnName>>`),
    /// never the upstream `Column`'s own detail (see
    /// `expand_wildcard_of`'s doc comment).
    #[test]
    fn a_wildcard_expansion_never_carries_forward_an_upstream_struct_shape() {
        let mut known_relations: HashMap<QualifiedName, Upstream> = HashMap::new();
        known_relations.insert(
            ("db".to_string(), "s".to_string(), "up".to_string()),
            Upstream::Node(NodeId::new("model.upstream")),
        );
        let mut resolved_schemas = HashMap::new();
        resolved_schemas.insert(
            NodeId::new("model.upstream"),
            vec![ColumnName::new("payload")],
        );

        // The upstream Node itself resolved `payload` with a real struct
        // shape (via an explicit CAST) -- but that detail lives only in
        // *that* build's own `LocalSchema::Known`, never in
        // `resolved_schemas`, so this downstream model's wildcard
        // expansion of it has no way to see it.
        let query =
            parse_query(r#"select *, 1 as extra_col from "db"."s"."up""#).expect("should parse");
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                let payload = cols
                    .iter()
                    .find(|c| c.name == "payload")
                    .expect("payload should be expanded from the wildcard");
                assert_eq!(
                    payload.struct_fields, None,
                    "wildcard expansion must never carry forward an upstream struct shape"
                );
            }
            other => panic!("expected Known([payload, extra_col]), got {other:?}"),
        }
    }

    /// A `named_struct(...)` call with a non-literal (or missing) key
    /// argument -- or any other shape this extraction doesn't recognize
    /// as fully self-describing -- must not produce a partial field list;
    /// it stays `None`, not a shape missing an entry.
    #[test]
    fn a_named_struct_call_with_a_non_literal_key_produces_no_struct_shape() {
        let query = parse_query("select named_struct(x.key_col, x.id) as payload from t as x")
            .expect("should parse");
        let known_relations: HashMap<QualifiedName, Upstream> = HashMap::new();
        let resolved_schemas = HashMap::new();
        let schema = resolve_query(
            &query,
            &known_relations,
            &resolved_schemas,
            &CatalogSchemas::new(),
        );

        match schema {
            LocalSchema::Known(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(
                    cols[0].struct_fields, None,
                    "a non-literal named_struct key must not produce a partial/guessed shape"
                );
            }
            other => panic!("expected Known([payload with no struct shape]), got {other:?}"),
        }
    }
}
