//! `WarehouseAdapter`: the interface boundary for warehouse-specific
//! behavior zhao-core needs. For v1, the only capability is checking
//! whether a relation exists in the configured target (see
//! `crate::adapters::warehouse::WarehouseAdapter::relation_exists`).
//!
//! Zhao never holds or uses warehouse credentials of its own -- it never
//! connects to a warehouse directly. The actual check is always executed
//! through the same connection a transformation tool (e.g. dbt) already
//! has, via the [`QueryExecutor`] a caller supplies. A `WarehouseAdapter`'s
//! job is only to know how to *ask* -- resolving a warehouse identity and
//! phrasing the check -- never how to *connect*.
//!
//! ## Why a shared check, not per-warehouse SQL
//!
//! `adapter.get_relation(database, schema, identifier)` is dbt-core's own
//! cross-adapter API: every dbt adapter plugin (Snowflake, Databricks,
//! BigQuery, DuckDB, ...) implements it with an identical signature, and dbt
//! itself already resolves whatever dialect differences exist underneath
//! (BigQuery calling the first part a "project" rather than a "database,"
//! for instance). So the existence check itself is genuinely
//! warehouse-agnostic for v1 -- there's no dialect-specific SQL zhao needs
//! to generate here, matching the "no SQL generation capability is needed
//! yet" v1 scope. What *does* differ per warehouse is knowing which one is
//! even active for a given project (dbt records this as `adapter_type` in
//! a compiled manifest's metadata) -- that's what
//! [`WarehouseAdapter::adapter_type`] and [`resolve`] are for.
//!
//! ## A caveat for whoever implements the dbt-side `QueryExecutor`
//!
//! `adapter.get_relation` reads from dbt's own relation cache, populated
//! by dbt's schema introspection at the start of a run for schemas it
//! expects to touch -- community reports exist of it returning "not
//! found" for a relation that genuinely exists but falls outside a given
//! invocation's cached scope. The `zhao_relation_exists` macro this
//! module's `WarehouseAdapter`s expect (see `check_relation_exists`)
//! should force a fresh lookup rather than trust that cache, or a
//! `--check-relations` false negative could silently turn a real,
//! existing incrementally-materialized model's schema-evolution warning
//! from conditional back into "doesn't exist" -- worse than not checking
//! at all.

use std::collections::HashMap;

/// A fully-qualified relation identity, already resolved (e.g. from a dbt
/// manifest node's `database`/`schema`/`alias` fields) -- warehouse-
/// agnostic: callers never need to know which warehouse-specific naming
/// convention (BigQuery's "project," Databricks' catalog, ...) produced
/// these parts, only what dbt itself already resolved them to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationIdentity {
    /// The relation's database (or BigQuery project, or Databricks
    /// catalog) -- `None` when a project's target doesn't use this part
    /// (some warehouses/configurations only need schema + identifier).
    pub database: Option<String>,
    /// The relation's schema (or BigQuery dataset).
    pub schema: Option<String>,
    /// The relation's own name (a dbt model's `alias`).
    pub identifier: String,
}

/// Executes a transformation tool's own macro/operation mechanism, using
/// whatever connection that tool already has -- the seam a
/// `WarehouseAdapter` asks through, never connecting to a warehouse
/// directly itself. A dbt implementation shells out to `dbt
/// run-operation`; a different transformation tool would implement this
/// its own way. Kept warehouse-agnostic (like [`WarehouseAdapter`]
/// itself) and dyn-safe so a single registry (see [`resolve`]) can hand
/// back any of the v1 adapters uniformly.
pub trait QueryExecutor {
    /// Runs `macro_name` with `args`, returning its captured output for
    /// the caller to interpret (e.g. a dbt macro's `log(..., info=True)`
    /// output). `Err` carries a human-readable reason -- this seam is
    /// internal plumbing between a `WarehouseAdapter` and whatever tool
    /// implements it, not a user-facing error surface in its own right.
    fn run_macro(&self, macro_name: &str, args: &HashMap<String, String>)
    -> Result<String, String>;
}

/// Everything that can go wrong asking a [`WarehouseAdapter`] to check a
/// relation's existence.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WarehouseAdapterError {
    /// The [`QueryExecutor`] itself failed (e.g. the underlying `dbt
    /// run-operation` invocation errored).
    #[error("could not determine whether {relation} exists: {reason}")]
    ExecutorFailed {
        /// The relation being checked, for a readable error.
        relation: String,
        /// The executor's own failure reason.
        reason: String,
    },
    /// The executor ran successfully but its output wasn't the expected
    /// `"true"`/`"false"` shape -- e.g. the macro itself isn't installed
    /// in the target project, or produced unexpected output.
    #[error("could not interpret the existence check's result for {relation}: {output:?}")]
    UnparseableResult {
        /// The relation being checked, for a readable error.
        relation: String,
        /// The executor's raw (unparseable) output.
        output: String,
    },
}

/// The interface boundary for warehouse-specific behavior -- see the
/// module doc comment. v1's only capability: relation existence.
pub trait WarehouseAdapter {
    /// This warehouse's dbt `adapter_type` name (e.g. `"snowflake"`),
    /// matching the string dbt itself records in a compiled manifest's
    /// `metadata.adapter_type` -- the value [`resolve`] matches against.
    fn adapter_type(&self) -> &'static str;

    /// Whether `relation` exists in the configured target, asked through
    /// `executor` -- never a connection this `WarehouseAdapter` holds
    /// itself.
    fn relation_exists(
        &self,
        relation: &RelationIdentity,
        executor: &dyn QueryExecutor,
    ) -> Result<bool, WarehouseAdapterError>;
}

/// The dbt macro every v1 `WarehouseAdapter` invokes -- see the module
/// doc comment's "why a shared check" section. Not `pub`: an
/// implementation detail of this module's three adapters, not part of
/// the trait's own contract (a future, genuinely dialect-specific
/// capability could use a different macro name without changing the
/// trait itself).
const RELATION_EXISTS_MACRO: &str = "zhao_relation_exists";

/// Shared v1 implementation every [`WarehouseAdapter`] in this module
/// delegates to -- see the module doc comment for why this is safe to
/// share rather than reimplement per warehouse.
fn check_relation_exists(
    relation: &RelationIdentity,
    executor: &dyn QueryExecutor,
) -> Result<bool, WarehouseAdapterError> {
    let mut args = HashMap::new();
    if let Some(database) = &relation.database {
        args.insert("relation_database".to_string(), database.clone());
    }
    if let Some(schema) = &relation.schema {
        args.insert("relation_schema".to_string(), schema.clone());
    }
    args.insert(
        "relation_identifier".to_string(),
        relation.identifier.clone(),
    );

    let output = executor
        .run_macro(RELATION_EXISTS_MACRO, &args)
        .map_err(|reason| WarehouseAdapterError::ExecutorFailed {
            relation: relation.identifier.clone(),
            reason,
        })?;

    match output.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(WarehouseAdapterError::UnparseableResult {
            relation: relation.identifier.clone(),
            output,
        }),
    }
}

/// The Snowflake [`WarehouseAdapter`].
#[derive(Debug, Clone, Copy)]
pub struct SnowflakeAdapter;

impl WarehouseAdapter for SnowflakeAdapter {
    fn adapter_type(&self) -> &'static str {
        "snowflake"
    }

    fn relation_exists(
        &self,
        relation: &RelationIdentity,
        executor: &dyn QueryExecutor,
    ) -> Result<bool, WarehouseAdapterError> {
        check_relation_exists(relation, executor)
    }
}

/// The Databricks [`WarehouseAdapter`].
#[derive(Debug, Clone, Copy)]
pub struct DatabricksAdapter;

impl WarehouseAdapter for DatabricksAdapter {
    fn adapter_type(&self) -> &'static str {
        "databricks"
    }

    fn relation_exists(
        &self,
        relation: &RelationIdentity,
        executor: &dyn QueryExecutor,
    ) -> Result<bool, WarehouseAdapterError> {
        check_relation_exists(relation, executor)
    }
}

/// The BigQuery [`WarehouseAdapter`].
#[derive(Debug, Clone, Copy)]
pub struct BigQueryAdapter;

impl WarehouseAdapter for BigQueryAdapter {
    fn adapter_type(&self) -> &'static str {
        "bigquery"
    }

    fn relation_exists(
        &self,
        relation: &RelationIdentity,
        executor: &dyn QueryExecutor,
    ) -> Result<bool, WarehouseAdapterError> {
        check_relation_exists(relation, executor)
    }
}

/// The DuckDB [`WarehouseAdapter`] -- zhao-dbt-test's own credential-free
/// CI target, and likely the most common local-development target for
/// anyone trying zhao out.
#[derive(Debug, Clone, Copy)]
pub struct DuckDbAdapter;

impl WarehouseAdapter for DuckDbAdapter {
    fn adapter_type(&self) -> &'static str {
        "duckdb"
    }

    fn relation_exists(
        &self,
        relation: &RelationIdentity,
        executor: &dyn QueryExecutor,
    ) -> Result<bool, WarehouseAdapterError> {
        check_relation_exists(relation, executor)
    }
}

/// Resolves the [`WarehouseAdapter`] for a dbt-style `adapter_type` string
/// (e.g. as recorded in a compiled manifest's `metadata.adapter_type`).
/// `None` for any warehouse zhao doesn't support checking against yet --
/// callers should treat that the same as `--check-relations` simply not
/// being available for this project, not as an error.
pub fn resolve(adapter_type: &str) -> Option<Box<dyn WarehouseAdapter>> {
    match adapter_type {
        "snowflake" => Some(Box::new(SnowflakeAdapter)),
        "databricks" => Some(Box::new(DatabricksAdapter)),
        "bigquery" => Some(Box::new(BigQueryAdapter)),
        "duckdb" => Some(Box::new(DuckDbAdapter)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`QueryExecutor`] whose response is fixed in advance -- the
    /// "fixture/mocked connection" every `WarehouseAdapter` test here
    /// uses, standing in for a real `dbt run-operation` invocation
    /// against a real warehouse account (which these tests deliberately
    /// never touch).
    struct StubExecutor {
        response: Result<String, String>,
    }

    impl QueryExecutor for StubExecutor {
        fn run_macro(
            &self,
            _macro_name: &str,
            _args: &HashMap<String, String>,
        ) -> Result<String, String> {
            self.response.clone()
        }
    }

    fn relation(identifier: &str) -> RelationIdentity {
        RelationIdentity {
            database: Some("analytics".to_string()),
            schema: Some("public".to_string()),
            identifier: identifier.to_string(),
        }
    }

    #[test]
    fn resolve_matches_each_supported_dbt_adapter_type() {
        assert_eq!(resolve("snowflake").unwrap().adapter_type(), "snowflake");
        assert_eq!(resolve("databricks").unwrap().adapter_type(), "databricks");
        assert_eq!(resolve("bigquery").unwrap().adapter_type(), "bigquery");
        assert_eq!(resolve("duckdb").unwrap().adapter_type(), "duckdb");
    }

    #[test]
    fn resolve_is_none_for_an_unsupported_adapter_type() {
        assert!(resolve("postgres").is_none());
        assert!(resolve("redshift").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn relation_exists_returns_true_when_the_executor_reports_true() {
        let executor = StubExecutor {
            response: Ok("true".to_string()),
        };
        for adapter_type in ["snowflake", "databricks", "bigquery", "duckdb"] {
            let adapter = resolve(adapter_type).unwrap();
            assert_eq!(
                adapter.relation_exists(&relation("dim_customers"), &executor),
                Ok(true),
                "{adapter_type} should report the relation as existing"
            );
        }
    }

    #[test]
    fn relation_exists_returns_false_when_the_executor_reports_false() {
        let executor = StubExecutor {
            response: Ok("false".to_string()),
        };
        for adapter_type in ["snowflake", "databricks", "bigquery", "duckdb"] {
            let adapter = resolve(adapter_type).unwrap();
            assert_eq!(
                adapter.relation_exists(&relation("dim_customers"), &executor),
                Ok(false),
                "{adapter_type} should report the relation as not existing"
            );
        }
    }

    /// Tolerates incidental leading/trailing whitespace in the executor's
    /// output (e.g. a trailing newline from captured stdout) without
    /// treating it as unparseable.
    #[test]
    fn relation_exists_trims_whitespace_around_the_executors_output() {
        let executor = StubExecutor {
            response: Ok("  true\n".to_string()),
        };
        assert_eq!(
            SnowflakeAdapter.relation_exists(&relation("dim_customers"), &executor),
            Ok(true)
        );
    }

    #[test]
    fn relation_exists_surfaces_an_executor_failure() {
        let executor = StubExecutor {
            response: Err("dbt run-operation exited non-zero".to_string()),
        };
        assert_eq!(
            SnowflakeAdapter.relation_exists(&relation("dim_customers"), &executor),
            Err(WarehouseAdapterError::ExecutorFailed {
                relation: "dim_customers".to_string(),
                reason: "dbt run-operation exited non-zero".to_string(),
            })
        );
    }

    #[test]
    fn relation_exists_reports_an_unparseable_result_clearly() {
        let executor = StubExecutor {
            response: Ok("maybe?".to_string()),
        };
        assert_eq!(
            SnowflakeAdapter.relation_exists(&relation("dim_customers"), &executor),
            Err(WarehouseAdapterError::UnparseableResult {
                relation: "dim_customers".to_string(),
                output: "maybe?".to_string(),
            })
        );
    }

    #[test]
    fn relation_exists_passes_the_resolved_identity_through_as_macro_args() {
        struct CapturingExecutor {
            captured: std::cell::RefCell<Option<HashMap<String, String>>>,
        }
        impl QueryExecutor for CapturingExecutor {
            fn run_macro(
                &self,
                macro_name: &str,
                args: &HashMap<String, String>,
            ) -> Result<String, String> {
                assert_eq!(macro_name, RELATION_EXISTS_MACRO);
                *self.captured.borrow_mut() = Some(args.clone());
                Ok("true".to_string())
            }
        }

        let executor = CapturingExecutor {
            captured: std::cell::RefCell::new(None),
        };
        SnowflakeAdapter
            .relation_exists(&relation("dim_customers"), &executor)
            .expect("should succeed");

        let captured = executor.captured.into_inner().expect("should have run");
        assert_eq!(
            captured.get("relation_database").map(String::as_str),
            Some("analytics")
        );
        assert_eq!(
            captured.get("relation_schema").map(String::as_str),
            Some("public")
        );
        assert_eq!(
            captured.get("relation_identifier").map(String::as_str),
            Some("dim_customers")
        );
    }

    /// When a relation identity has no database (some warehouses/target
    /// configurations don't need one), the arg simply isn't passed --
    /// never a spurious empty string.
    #[test]
    fn relation_exists_omits_database_arg_when_absent() {
        struct CapturingExecutor {
            captured: std::cell::RefCell<Option<HashMap<String, String>>>,
        }
        impl QueryExecutor for CapturingExecutor {
            fn run_macro(
                &self,
                _macro_name: &str,
                args: &HashMap<String, String>,
            ) -> Result<String, String> {
                *self.captured.borrow_mut() = Some(args.clone());
                Ok("false".to_string())
            }
        }

        let executor = CapturingExecutor {
            captured: std::cell::RefCell::new(None),
        };
        let relation = RelationIdentity {
            database: None,
            schema: Some("public".to_string()),
            identifier: "dim_customers".to_string(),
        };
        SnowflakeAdapter
            .relation_exists(&relation, &executor)
            .expect("should succeed");

        let captured = executor.captured.into_inner().expect("should have run");
        assert!(!captured.contains_key("relation_database"));
        assert_eq!(
            captured.get("relation_schema").map(String::as_str),
            Some("public")
        );
    }

    /// The symmetric case: no spurious `relation_schema` arg when `schema`
    /// is absent, mirroring `relation_exists_omits_database_arg_when_absent`
    /// above for the other optional part.
    #[test]
    fn relation_exists_omits_schema_arg_when_absent() {
        struct CapturingExecutor {
            captured: std::cell::RefCell<Option<HashMap<String, String>>>,
        }
        impl QueryExecutor for CapturingExecutor {
            fn run_macro(
                &self,
                _macro_name: &str,
                args: &HashMap<String, String>,
            ) -> Result<String, String> {
                *self.captured.borrow_mut() = Some(args.clone());
                Ok("false".to_string())
            }
        }

        let executor = CapturingExecutor {
            captured: std::cell::RefCell::new(None),
        };
        let relation = RelationIdentity {
            database: Some("analytics".to_string()),
            schema: None,
            identifier: "dim_customers".to_string(),
        };
        SnowflakeAdapter
            .relation_exists(&relation, &executor)
            .expect("should succeed");

        let captured = executor.captured.into_inner().expect("should have run");
        assert!(!captured.contains_key("relation_schema"));
        assert_eq!(
            captured.get("relation_database").map(String::as_str),
            Some("analytics")
        );
    }
}
