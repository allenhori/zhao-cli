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
    ColumnLineage, ColumnName, LineageEdge, Node, NodeId, Origin, OriginId, ParsedProject, Upstream,
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
    database: String,
    schema: String,
    alias: String,
    #[serde(default)]
    depends_on: RawDependsOn,
    #[serde(default)]
    compiled_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSource {
    unique_id: String,
    name: String,
    database: String,
    schema: String,
    identifier: String,
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
    let models: Vec<&RawNode> = manifest
        .nodes
        .values()
        .filter(|n| n.resource_type == "model")
        .collect();

    let origins: Vec<Origin> = manifest
        .sources
        .values()
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
    for source in manifest.sources.values() {
        known_relations.insert(
            (
                source.database.clone(),
                source.schema.clone(),
                source.identifier.clone(),
            ),
            Upstream::Origin(OriginId::new(source.unique_id.clone())),
        );
    }
    for model in &models {
        known_relations.insert(
            (
                model.database.clone(),
                model.schema.clone(),
                model.alias.clone(),
            ),
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

        let local_schema = model
            .compiled_code
            .as_deref()
            .and_then(parse_query)
            .map(|query| resolve_query(&query, &known_relations, &resolved_schemas))
            .unwrap_or(LocalSchema::Opaque);

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

        // Baseline node-level edges from dbt's own dependency list -- the
        // authoritative source, since it accounts for macro expansions and
        // references our own SQL resolution might miss (e.g. in a WHERE
        // clause). Column-level edges above are additive detail, not a
        // replacement for these.
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
        nodes.push(Node {
            id: node_id,
            name: model.name.clone(),
            columns,
        });
    }

    ParsedProject {
        nodes,
        origins,
        edges,
    }
}

/// Resolves a `depends_on.nodes` entry (a dbt `unique_id`) to an [`Upstream`].
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
#[derive(Debug, Clone)]
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
    let mut result = HashMap::new();
    for twj in from {
        resolve_table_factor(
            &twj.relation,
            scope,
            known_relations,
            resolved_schemas,
            &mut result,
        );
        for join in &twj.joins {
            resolve_table_factor(
                &join.relation,
                scope,
                known_relations,
                resolved_schemas,
                &mut result,
            );
        }
    }
    result
}

fn resolve_table_factor(
    factor: &TableFactor,
    scope: &HashMap<String, LocalSchema>,
    known_relations: &HashMap<QualifiedName, Upstream>,
    _resolved_schemas: &HashMap<NodeId, Vec<ColumnName>>,
    result: &mut HashMap<String, LocalSchema>,
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

        result.insert(effective_alias, resolved);
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
    // Ambiguous across multiple FROM items: only resolve if exactly one
    // of them can actually provide this column.
    let mut found = None;
    for schema in from_scope.values() {
        if let Some(hit) = source_of(schema, column) {
            if found.is_some() {
                return None; // genuinely ambiguous
            }
            found = Some(hit);
        }
    }
    found
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
