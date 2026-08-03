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

use super::{AdapterVocabulary, TransformationToolAdapter};
use crate::model::{
    Column, ColumnLineage, ColumnName, JoinKind, LineageEdge, Node, NodeId, Origin, OriginId,
    ParsedProject, Upstream,
};
use serde::Deserialize;
use sqlparser::ast::{
    Expr, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
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
        // A dbt `unique_id` is always shaped `<resource_type>.<package>.<name>`
        // (dbt itself constructs it that way; model names can't contain
        // `.`), so the bare, selectable name is just the last segment --
        // no need to look up a full `Node`, which may not even exist
        // (e.g. a Node reached only via the Baseline that no longer
        // exists in the current state).
        let names: Vec<&str> = node_ids
            .iter()
            .map(|id| id.rsplit('.').next().unwrap_or(id))
            .collect();
        Some(format!("dbt build --select {}", names.join(" ")))
    }
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
    #[error("dbt compile failed in {project_dir}:\n{stderr}")]
    CompileFailed {
        /// The project directory `dbt compile` was run in.
        project_dir: String,
        /// `dbt compile`'s captured stderr.
        stderr: String,
    },
}

impl TransformationToolAdapter for DbtAdapter {
    type Error = DbtAdapterError;

    fn parse(&self, path: &Path) -> Result<ParsedProject, Self::Error> {
        let raw = fs::read_to_string(path).map_err(|source| DbtAdapterError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let manifest: RawManifest =
            serde_json::from_str(&raw).map_err(|source| DbtAdapterError::InvalidManifest {
                path: path.display().to_string(),
                source,
            })?;

        Ok(build_parsed_project(&manifest))
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
    /// the tests run.
    ///
    /// This is a plain method, not part of [`TransformationToolAdapter`]:
    /// that trait's `parse` deliberately leaves "how the compiled output
    /// got there" to the caller, and shelling out to `dbt compile` is
    /// exactly the kind of dbt-specific concern this adapter's module (not
    /// the trait) should own.
    pub fn compile(&self, project_dir: &Path, dbt_command: &str) -> Result<(), DbtAdapterError> {
        let output = std::process::Command::new(dbt_command)
            .arg("compile")
            .current_dir(project_dir)
            .output()
            .map_err(|source| DbtAdapterError::CommandNotFound {
                command: dbt_command.to_string(),
                source,
            })?;

        if !output.status.success() {
            return Err(DbtAdapterError::CompileFailed {
                project_dir: project_dir.display().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(())
    }
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

        // Column-level edges, from whatever was resolved.
        match &local_schema {
            LocalSchema::Known(cols) => {
                for col in cols {
                    if let Some((upstream, upstream_col)) = &col.source {
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

        let documented_columns: Vec<Column> = columns
            .iter()
            .map(|name| Column {
                name: name.clone(),
                data_type: model
                    .columns
                    .get(name.as_str())
                    .and_then(|doc| doc.data_type.clone()),
            })
            .collect();

        nodes.push(Node {
            id: node_id,
            name: model.name.clone(),
            columns: documented_columns,
            joins,
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

/// A resolved column: its output name, and, if traceable to a single
/// upstream Node/Origin column, where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedColumn {
    name: String,
    source: Option<(Upstream, String)>,
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
                        source: Some((upstream.clone(), c.as_str().to_string())),
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

/// Resolves a single projection expression to a named, possibly-sourced
/// column. Only plain (optionally qualified) identifiers are traced to an
/// upstream column; anything else (function calls, arithmetic, literals,
/// `CASE`, ...) is recorded by its output name with no resolved source.
fn resolve_expr_column(
    expr: &Expr,
    alias: Option<String>,
    from_scope: &HashMap<String, LocalSchema>,
) -> ResolvedColumn {
    let source = match expr {
        Expr::Identifier(ident) => resolve_unqualified(&ident.value, from_scope),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            resolve_qualified(&parts[0].value, &parts[1].value, from_scope)
        }
        _ => None,
    };

    let name = alias.unwrap_or_else(|| match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => {
            parts.last().map(|p| p.value.clone()).unwrap_or_default()
        }
        other => other.to_string(),
    });

    ResolvedColumn { name, source }
}

fn resolve_unqualified(
    column: &str,
    from_scope: &HashMap<String, LocalSchema>,
) -> Option<(Upstream, String)> {
    if from_scope.len() == 1 {
        let only = from_scope.values().next()?;
        return source_of(only, column);
    }

    // A `Known` relation's columns are enumerated -- it either definitely
    // has this column or definitely doesn't, so a `Known` hit is
    // authoritative and wins over merely-possible matches.
    let known_hits: Vec<(Upstream, String)> = from_scope
        .values()
        .filter_map(|schema| match schema {
            LocalSchema::Known(_) => source_of(schema, column),
            _ => None,
        })
        .collect();
    match known_hits.len() {
        1 => return known_hits.into_iter().next(),
        n if n > 1 => return None, // genuinely ambiguous among Known relations
        _ => {}
    }

    // No `Known` relation claims this column -- fall back to
    // `Passthrough` relations, whose real columns we can't enumerate to
    // either confirm or rule out, so only resolve if exactly one is in
    // scope (more than one is genuinely ambiguous).
    let passthrough_hits: Vec<(Upstream, String)> = from_scope
        .values()
        .filter_map(|schema| match schema {
            LocalSchema::Passthrough(upstream) => Some((upstream.clone(), column.to_string())),
            _ => None,
        })
        .collect();
    match passthrough_hits.len() {
        1 => passthrough_hits.into_iter().next(),
        _ => None,
    }
}

fn resolve_qualified(
    qualifier: &str,
    column: &str,
    from_scope: &HashMap<String, LocalSchema>,
) -> Option<(Upstream, String)> {
    let schema = from_scope.get(qualifier)?;
    source_of(schema, column)
}

fn source_of(schema: &LocalSchema, column: &str) -> Option<(Upstream, String)> {
    match schema {
        LocalSchema::Passthrough(upstream) => Some((upstream.clone(), column.to_string())),
        LocalSchema::Known(cols) => cols
            .iter()
            .find(|c| c.name == column)
            .and_then(|c| c.source.clone()),
        LocalSchema::Opaque => None,
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
                    cols[0].source.is_none(),
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
                    cols[0].source,
                    Some((origin("origin.s.tbl"), "name".to_string()))
                );
            }
            other => panic!("expected Known([name sourced from tbl via CTE a]), got {other:?}"),
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
            .compile(project_dir.path(), dbt.to_str().expect("utf8 path"))
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

    #[cfg(unix)]
    #[test]
    fn compile_reports_a_clear_error_when_dbt_compile_fails() {
        let project_dir = tempfile::tempdir().expect("should create temp dir");
        let stub_dir = tempfile::tempdir().expect("should create temp dir");
        let dbt = stub_dbt_command(stub_dir.path(), "echo 'boom' >&2\nexit 1");

        let result = DbtAdapter.compile(project_dir.path(), dbt.to_str().expect("utf8 path"));

        match result {
            Err(DbtAdapterError::CompileFailed { stderr, .. }) => {
                assert!(stderr.contains("boom"), "stderr should surface: {stderr:?}");
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
        );

        assert!(matches!(
            result,
            Err(DbtAdapterError::CommandNotFound { .. })
        ));
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
}
