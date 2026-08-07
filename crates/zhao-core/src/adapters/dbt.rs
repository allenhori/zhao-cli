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
//! and qualified column references, and simple aliasing. It deliberately
//! does not attempt to resolve columns through `UNION`/`UNION ALL`, inline
//! subqueries in a `FROM` clause (as opposed to CTEs), or window functions --
//! those cases fall back to an unresolved (but still node-level-tracked)
//! dependency rather than a guessed column mapping. Getting a column
//! mapping wrong silently would be worse than not having one. Likewise, an
//! Origin's real columns are never known (dbt's manifest doesn't carry a
//! source's actual schema), so a wildcard that would need to enumerate an
//! Origin's columns can't be expanded -- only identity ("this column,
//! whatever it's called, passes through unchanged") relationships to an
//! Origin are tracked.
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
//! shape, and a map's value-type evolution are all out of scope), and
//! *not* propagated across a CTE hop, a rename, or a wildcard expansion
//! the way lineage `sources` are -- only a column's own immediate SQL in
//! the model actually being resolved ever produces a shape; see
//! `ResolvedColumn::struct_fields`'s doc comment (also private).

use super::warehouse::{QueryExecutor, RELATION_EXISTS_MACRO, RelationIdentity};
use super::{AdapterVocabulary, TransformationToolAdapter};
use crate::model::{
    Column, ColumnLineage, ColumnName, JoinKind, LineageEdge, Materialization, Node, NodeId,
    Origin, OriginId, ParsedProject, StructField, Upstream,
};
use serde::Deserialize;
use sqlparser::ast::{
    AccessExpr, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select,
    SelectItem, SetExpr, Statement, Subscript, TableFactor, TableWithJoins, Value,
};
use sqlparser::dialect::GenericDialect;
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

    fn recommended_validation_command(&self, node_ids: &[String]) -> Option<String> {
        if node_ids.is_empty() {
            return None;
        }
        let names: Vec<String> = node_ids
            .iter()
            .map(|id| self.node_display_name(id))
            .collect();
        Some(format!("dbt build --select {}", names.join(" ")))
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
}

impl TransformationToolAdapter for DbtAdapter {
    type Error = DbtAdapterError;

    fn parse(&self, path: &Path) -> Result<ParsedProject, Self::Error> {
        Ok(build_parsed_project(&read_manifest(path)?))
    }

    fn vocabulary(&self) -> &dyn AdapterVocabulary {
        &DbtVocabulary
    }
}

impl DbtAdapter {
    /// Runs `dbt compile` in `project_dir`, so its `target/manifest.json`
    /// reflects the project's current compiled state.
    ///
    /// `dbt_command` is the executable to invoke -- ordinarily just
    /// `"dbt"`, resolved via `PATH` -- exposed as a parameter (rather than
    /// hardcoded) so tests can point it at a stub script instead of
    /// depending on whether a real `dbt` happens to be installed wherever
    /// the tests run. `extra_args` are appended verbatim after `compile`
    /// (e.g. `--target`, `--vars`) -- zhao never interprets or validates
    /// these, dbt does.
    ///
    /// This is a plain method, not part of [`TransformationToolAdapter`]:
    /// that trait's `parse` deliberately leaves "how the compiled output
    /// got there" to the caller, and shelling out to `dbt compile` is
    /// exactly the kind of dbt-specific concern this adapter's module (not
    /// the trait) should own.
    pub fn compile(
        &self,
        project_dir: &Path,
        dbt_command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        let output = run_dbt_subcommand(dbt_command, "compile", project_dir, extra_args)?;
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
    /// before a subsequent [`DbtAdapter::compile`] -- needed the first
    /// time a project is compiled somewhere its packages have never been
    /// installed (e.g. a freshly checked-out git worktree), since `dbt
    /// compile` fails if a `ref()`/macro from an unresolved package is
    /// used. See [`DbtAdapter::compile`] for `dbt_command`/`extra_args`.
    pub fn deps(
        &self,
        project_dir: &Path,
        dbt_command: &str,
        extra_args: &[String],
    ) -> Result<DbtCommandOutput, DbtAdapterError> {
        let output = run_dbt_subcommand(dbt_command, "deps", project_dir, extra_args)?;
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

/// Splits `dbt_command` into a program plus any leading prefix arguments,
/// shell-word-style -- so a value like `"uv run dbt"`, or a custom wrapper
/// a project's own tooling already uses instead of invoking `dbt`
/// directly (e.g. `"dw some-flag"`), works as a genuine multi-word
/// prefix rather than being mistaken for one literal executable named
/// `"uv run dbt"`. Shared by [`run_dbt_subcommand`] and
/// [`DbtQueryExecutor::run_operation`], the two places that actually spawn
/// a `dbt`-shaped subprocess.
fn split_dbt_command(dbt_command: &str) -> Result<(String, Vec<String>), DbtAdapterError> {
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

fn build_parsed_project(manifest: &RawManifest) -> ParsedProject {
    // Only "model" entries missing a usable qualified name are skipped
    // (rather than failing the whole manifest) -- see the doc comment on
    // `RawNode`'s `database`/`schema`/`alias` fields for why those are
    // optional in the first place.
    let models: Vec<&RawNode> = manifest
        .nodes
        .values()
        .filter(|n| n.resource_type == "model" && n.qualified_name().is_some())
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

    // Upstream models must be processed before downstream ones: a
    // downstream model's `SELECT *` against an upstream needs that
    // upstream's already-resolved column list to expand.
    let ordered = topological_order(&models);

    let mut nodes = Vec::with_capacity(ordered.len());
    let mut edges = Vec::new();
    let mut resolved_schemas: HashMap<NodeId, Vec<ColumnName>> = HashMap::new();

    for model in ordered {
        let node_id = NodeId::new(model.unique_id.clone());

        let parsed_query = model.compiled_code.as_deref().and_then(parse_query);
        let local_schema = parsed_query
            .as_ref()
            .map(|query| resolve_query(query, &known_relations, &resolved_schemas))
            .unwrap_or(LocalSchema::Opaque);
        let joins = parsed_query.as_ref().map(extract_joins).unwrap_or_default();

        let columns: Vec<ColumnName> = match &local_schema {
            LocalSchema::Known(cols) => cols
                .iter()
                .map(|c| ColumnName::new(c.name.clone()))
                .collect(),
            LocalSchema::Passthrough(Upstream::Node(upstream_id)) => resolved_schemas
                .get(upstream_id)
                .cloned()
                .unwrap_or_default(),
            // An Origin's real columns are never known -- see module docs.
            LocalSchema::Passthrough(Upstream::Origin(_)) | LocalSchema::Opaque => Vec::new(),
        };

        // Column-level edges, from whatever was resolved. A calculated
        // column can carry more than one source (see the module-level
        // "Known limitations" doc comment) -- one edge per referenced
        // upstream column.
        match &local_schema {
            LocalSchema::Known(cols) => {
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
            LocalSchema::Passthrough(Upstream::Node(upstream_id)) => {
                // A pure `SELECT * FROM <upstream node>`: every column
                // passes through unchanged, one edge per column.
                for column in &columns {
                    edges.push(LineageEdge {
                        upstream: Upstream::Node(upstream_id.clone()),
                        downstream: node_id.clone(),
                        column: Some(ColumnLineage {
                            upstream_column: column.clone(),
                            downstream_column: column.clone(),
                        }),
                    });
                }
            }
            LocalSchema::Passthrough(Upstream::Origin(_)) | LocalSchema::Opaque => {}
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

    ParsedProject {
        nodes,
        origins,
        edges,
    }
}

/// Resolves a `depends_on.nodes` entry (a dbt `unique_id`) to an
/// [`Upstream`]. Returns `None` for a dependency on anything other than a
/// model or a source (a seed or snapshot, for instance) -- v1 only
/// represents Nodes (models) and Origins (sources), so a dependency on
/// some other resource type currently produces no edge at all, not even a
/// node-level one. This is a deliberate v1 scope limitation, not an
/// oversight: extending Node/Origin to cover other dbt resource types is
/// future work.
fn resolve_dependency_id(unique_id: &str, manifest: &RawManifest) -> Option<Upstream> {
    if let Some(node) = manifest.nodes.get(unique_id) {
        if node.resource_type == "model" {
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
    /// This column's `STRUCT` internal field shape, when its *immediate*
    /// defining expression is a `CAST(... AS STRUCT<...>)` or a
    /// `STRUCT(...)`/`named_struct(...)` constructor that names every
    /// field explicitly -- see [`extract_struct_shape`]. `None` otherwise,
    /// including for a plain passthrough/rename of an upstream struct
    /// column: unlike `sources` (which carries forward through a CTE hop
    /// via [`source_of`]), this is deliberately *not* propagated across
    /// CTE hops or wildcard expansion -- only a column's own immediate
    /// SQL in *this* model ever produces a shape, matching the "knowable
    /// from the compiled SQL" scope this feature was built for (see
    /// [`Column::struct_fields`]'s doc comment). A struct column that's
    /// merely renamed or passed through, even from an upstream CTE that
    /// itself had an explicit shape, stays `None` here.
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
    /// Couldn't resolve (e.g. a `UNION`, a subquery in `FROM`, or an
    /// ambiguous unqualified wildcard) -- opaque past this point.
    Opaque,
}

fn parse_query(sql: &str) -> Option<Query> {
    let dialect = GenericDialect {};
    let statements = SqlParser::parse_sql(&dialect, sql).ok()?;
    statements.into_iter().find_map(|stmt| match stmt {
        Statement::Query(query) => Some(*query),
        _ => None,
    })
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
) -> LocalSchema {
    let mut scope: HashMap<String, LocalSchema> = HashMap::new();

    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let resolved =
                resolve_set_expr(&cte.query.body, &scope, known_relations, resolved_schemas);
            scope.insert(cte.alias.name.value.clone(), resolved);
        }
    }

    resolve_set_expr(&query.body, &scope, known_relations, resolved_schemas)
}

fn resolve_set_expr(
    body: &SetExpr,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
) -> LocalSchema {
    match body {
        SetExpr::Select(select) => resolve_select(select, scope, known_relations, resolved_schemas),
        // UNION/INTERSECT/EXCEPT and anything else: not attempted, see
        // module-level "Known limitations" doc comment.
        _ => LocalSchema::Opaque,
    }
}

fn resolve_select(
    select: &Select,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
) -> LocalSchema {
    let from_scope = resolve_from(&select.from, scope, known_relations, resolved_schemas);

    // `SELECT * FROM <one thing>` and nothing else is a pure passthrough:
    // propagate whatever LocalSchema that one thing already resolved to,
    // unchanged, rather than forcing an enumeration we may not be able to
    // do (e.g. the one thing is itself a passthrough of an Origin, whose
    // real columns we never know).
    if let [SelectItem::Wildcard(_)] = select.projection.as_slice() {
        if from_scope.len() == 1 {
            return from_scope.into_values().next().unwrap();
        }
    }

    let mut columns = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => match expand_wildcard(&from_scope, resolved_schemas) {
                Some(mut expanded) => columns.append(&mut expanded),
                None => return LocalSchema::Opaque,
            },
            SelectItem::QualifiedWildcard(kind, _) => {
                let alias = qualified_wildcard_alias(kind);
                match alias.and_then(|a| from_scope.get(&a)) {
                    Some(schema) => match expand_wildcard_of(schema, resolved_schemas) {
                        Some(mut expanded) => columns.append(&mut expanded),
                        None => return LocalSchema::Opaque,
                    },
                    None => return LocalSchema::Opaque,
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                columns.push(resolve_expr_column(expr, None, &from_scope));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                columns.push(resolve_expr_column(
                    expr,
                    Some(alias.value.clone()),
                    &from_scope,
                ));
            }
            // A Snowflake-specific `expr AS (a, b, c)` tuple-expansion
            // syntax we don't compile against; not attempted (see module
            // docs' "Known limitations").
            SelectItem::ExprWithAliases { .. } => return LocalSchema::Opaque,
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
) -> HashMap<String, LocalSchema> {
    let mut collected: Vec<(String, LocalSchema)> = Vec::new();
    for twj in from {
        collect_table_factor(
            &twj.relation,
            scope,
            known_relations,
            resolved_schemas,
            &mut collected,
        );
        for join in &twj.joins {
            collect_table_factor(
                &join.relation,
                scope,
                known_relations,
                resolved_schemas,
                &mut collected,
            );
        }
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for (alias, _) in &collected {
        *counts.entry(alias.clone()).or_insert(0) += 1;
    }

    // A duplicate alias (two unaliased relations that happen to share a
    // trailing name, or a base table shadowing an earlier CTE) means we
    // can no longer trust which relation a later reference to it meant.
    // Mark it opaque rather than silently keeping whichever one happened
    // to be inserted last -- a wrong column-lineage guess is worse than
    // an admittedly-unresolved one.
    collected
        .into_iter()
        .map(|(alias, schema)| {
            let schema = if counts[alias.as_str()] > 1 {
                LocalSchema::Opaque
            } else {
                schema
            };
            (alias, schema)
        })
        .collect()
}

fn collect_table_factor(
    factor: &TableFactor,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    _resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    collected: &mut Vec<(String, LocalSchema)>,
) {
    if let TableFactor::Table { name, alias, .. } = factor {
        let parts: Vec<String> = name
            .0
            .iter()
            .map(|p| p.to_string().replace('"', ""))
            .collect();
        let effective_alias = alias
            .as_ref()
            .map(|a| a.name.value.clone())
            .unwrap_or_else(|| parts.last().cloned().unwrap_or_default());

        let resolved = if parts.len() == 3 {
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
    // Derived subqueries / table functions in FROM: not attempted (see
    // module-level "Known limitations").
}

/// Expands a bare `SELECT *` mixed with other projections, or where
/// multiple relations are in scope: valid only when there's exactly one
/// relation (otherwise which table's columns come first is ambiguous, and
/// guessing would be worse than admitting we don't know).
fn expand_wildcard(
    from_scope: &HashMap<String, LocalSchema>,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
) -> Option<Vec<ResolvedColumn>> {
    if from_scope.len() != 1 {
        return None;
    }
    let only = from_scope.values().next()?;
    expand_wildcard_of(only, resolved_schemas)
}

/// Enumerates a [`LocalSchema`]'s columns as a concrete list, when
/// possible. A `Passthrough` of a Node can always be enumerated (we've
/// already resolved that Node's real columns); a `Passthrough` of an
/// Origin can't (we never know an Origin's real columns).
fn expand_wildcard_of(
    schema: &LocalSchema,
    resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
) -> Option<Vec<ResolvedColumn>> {
    match schema {
        LocalSchema::Passthrough(upstream @ Upstream::Node(id)) => {
            let cols = resolved_schemas.get(id)?;
            Some(
                cols.iter()
                    .map(|c| ResolvedColumn {
                        name: c.as_str().to_string(),
                        sources: vec![(upstream.clone(), c.as_str().to_string())],
                        expression: None,
                        // A wildcard expansion only ever has an upstream
                        // Node's resolved column *names* to work with
                        // (`resolved_schemas: HashMap<NodeId,
                        // Vec<ColumnName>>` never carried full `Column`
                        // detail) -- there's no shape to carry forward
                        // even if the upstream column had one.
                        struct_fields: None,
                    })
                    .collect(),
            )
        }
        LocalSchema::Passthrough(Upstream::Origin(_)) => None,
        LocalSchema::Known(cols) => Some(cols.clone()),
        LocalSchema::Opaque => None,
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
) -> ResolvedColumn {
    let sources = collect_expr_sources(expr, from_scope);

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

    let struct_fields = extract_struct_shape(expr);

    ResolvedColumn {
        name,
        sources,
        expression,
        struct_fields,
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
) -> Vec<(Upstream, String)> {
    let mut found = Vec::new();
    collect_expr_sources_into(expr, from_scope, &mut found);
    found
}

fn collect_expr_sources_into(
    expr: &Expr,
    from_scope: &HashMap<String, LocalSchema>,
    found: &mut Vec<(Upstream, String)>,
) {
    let push_dedup = |mut new: Vec<(Upstream, String)>, found: &mut Vec<(Upstream, String)>| {
        new.retain(|candidate| !found.contains(candidate));
        found.append(&mut new);
    };

    match expr {
        Expr::Identifier(ident) => {
            push_dedup(
                resolve_unqualified(std::slice::from_ref(&ident.value), from_scope),
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
                    resolve_qualified(&path[0], &path[1..], from_scope),
                    found,
                );
            } else {
                push_dedup(resolve_unqualified(&path, from_scope), found);
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
            collect_expr_sources_into(value, from_scope, found);
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
                    collect_expr_sources_into(other, from_scope, found);
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
                    push_dedup(resolve_qualified(&path[0], &path[1..], from_scope), found);
                } else {
                    push_dedup(resolve_unqualified(path, from_scope), found);
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
                            collect_expr_sources_into(index, from_scope, found);
                        }
                        Subscript::Slice {
                            lower_bound,
                            upper_bound,
                            stride,
                        } => {
                            for bound in [lower_bound, upper_bound, stride].into_iter().flatten() {
                                collect_expr_sources_into(bound, from_scope, found);
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
                        collect_expr_sources_into(arg_expr, from_scope, found);
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
            collect_expr_sources_into(inner, from_scope, found);
        }
        // `POSITION(expr IN expr)` -- same reasoning, two operands
        // instead of one.
        Expr::Position { expr: inner, r#in } => {
            collect_expr_sources_into(inner, from_scope, found);
            collect_expr_sources_into(r#in, from_scope, found);
        }
        // `SUBSTRING(expr [FROM expr] [FOR expr])` (or its comma-arg
        // form) -- another dedicated variant, same reasoning.
        Expr::Substring {
            expr: inner,
            substring_from,
            substring_for,
            ..
        } => {
            collect_expr_sources_into(inner, from_scope, found);
            if let Some(from) = substring_from {
                collect_expr_sources_into(from, from_scope, found);
            }
            if let Some(for_) = substring_for {
                collect_expr_sources_into(for_, from_scope, found);
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
            collect_expr_sources_into(inner, from_scope, found);
            if let Some(what) = trim_what {
                collect_expr_sources_into(what, from_scope, found);
            }
            for c in trim_characters.iter().flatten() {
                collect_expr_sources_into(c, from_scope, found);
            }
        }
        // `OVERLAY(expr PLACING expr FROM expr [FOR expr])`.
        Expr::Overlay {
            expr: inner,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            collect_expr_sources_into(inner, from_scope, found);
            collect_expr_sources_into(overlay_what, from_scope, found);
            collect_expr_sources_into(overlay_from, from_scope, found);
            if let Some(for_) = overlay_for {
                collect_expr_sources_into(for_, from_scope, found);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_sources_into(left, from_scope, found);
            collect_expr_sources_into(right, from_scope, found);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_expr_sources_into(operand, from_scope, found);
            }
            for when in conditions {
                collect_expr_sources_into(&when.condition, from_scope, found);
                collect_expr_sources_into(&when.result, from_scope, found);
            }
            if let Some(else_result) = else_result {
                collect_expr_sources_into(else_result, from_scope, found);
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
) -> Vec<(Upstream, String)> {
    if from_scope.len() == 1 {
        return match from_scope.values().next() {
            Some(only) => resolve_path_on_schema(only, path),
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
            .filter(|schema| matches!(schema, LocalSchema::Known(cols) if cols.iter().any(|c| c.name == candidate)))
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
) -> Vec<(Upstream, String)> {
    match from_scope.get(qualifier) {
        Some(schema) => resolve_path_on_schema(schema, path),
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
fn resolve_path_on_schema(schema: &LocalSchema, path: &[String]) -> Vec<(Upstream, String)> {
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
    }
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
            .find(|c| c.name == column)
            .map(|c| c.sources.clone())
            .unwrap_or_default(),
        LocalSchema::Opaque => Vec::new(),
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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
            let schema = resolve_query(&query, &known_relations, &resolved_schemas);
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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
    fn recommended_validation_command_derives_bare_names_from_unique_ids() {
        let command = DbtVocabulary.recommended_validation_command(&[
            "model.zhao_dbt_test.stg_customers".to_string(),
            "model.zhao_dbt_test.dim_customers".to_string(),
        ]);

        assert_eq!(
            command.as_deref(),
            Some("dbt build --select stg_customers dim_customers")
        );
    }

    /// The Node's own bare name is derivable straight from its `NodeId`
    /// string -- no lookup against a real `Node` needed -- so even an ID
    /// with no corresponding `Node` anywhere (e.g. one that only ever
    /// existed in a Baseline that's since been deleted) still produces a
    /// sensible, selectable name.
    #[test]
    fn recommended_validation_command_works_without_a_real_node_to_look_up() {
        let command = DbtVocabulary
            .recommended_validation_command(&["model.zhao_dbt_test.long_gone".to_string()]);

        assert_eq!(command.as_deref(), Some("dbt build --select long_gone"));
    }

    #[test]
    fn recommended_validation_command_is_none_for_an_empty_list() {
        assert_eq!(DbtVocabulary.recommended_validation_command(&[]), None);
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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
        let schema = resolve_query(&query, &known_relations, &resolved_schemas);

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
