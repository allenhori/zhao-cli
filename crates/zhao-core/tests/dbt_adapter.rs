//! Integration tests for the dbt [`TransformationToolAdapter`] against a
//! real compiled manifest (trimmed from `zhao-dbt-test`, a genuine dbt
//! project, not hand-written JSON) -- see
//! `tests/fixtures/zhao_dbt_test_manifest.json`.

use std::path::Path;
use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
use zhao_core::model::{JoinKind, NodeId, OriginId, Upstream};

fn fixture_path() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/zhao_dbt_test_manifest.json"
    ))
}

#[test]
fn produces_the_expected_nodes_and_origins() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let mut node_names: Vec<&str> = project.nodes.iter().map(|n| n.name.as_str()).collect();
    node_names.sort_unstable();
    assert_eq!(
        node_names,
        vec![
            "dim_customers",
            "fct_orders",
            "fct_orders_incremental",
            "stg_customers",
            "stg_orders",
            "stg_payments",
        ]
    );

    let mut origin_names: Vec<&str> = project.origins.iter().map(|o| o.name.as_str()).collect();
    origin_names.sort_unstable();
    assert_eq!(
        origin_names,
        vec!["raw_customers", "raw_orders", "raw_payments"]
    );
}

#[test]
fn resolves_a_staging_model_schema_and_source_lineage() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let stg_customers = project
        .nodes
        .iter()
        .find(|n| n.name == "stg_customers")
        .expect("stg_customers should exist");

    let column_names: Vec<&str> = stg_customers
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(column_names, vec!["customer_id", "first_name", "last_name"]);

    let origin_id = OriginId::new("source.zhao_dbt_test.raw.raw_customers");
    let node_id = NodeId::new("model.zhao_dbt_test.stg_customers");

    // `id as customer_id` -- a real rename, traced through the "source"
    // and "renamed" CTEs back to the Origin's `id` column.
    let renamed = project.edges.iter().any(|e| {
        e.upstream == Upstream::Origin(origin_id.clone())
            && e.downstream == node_id
            && e.column.as_ref().is_some_and(|c| {
                c.upstream_column.as_str() == "id" && c.downstream_column.as_str() == "customer_id"
            })
    });
    assert!(
        renamed,
        "expected an edge tracing customer_id back to the source's id column"
    );

    // `first_name` -- passed straight through with no rename.
    let passthrough = project.edges.iter().any(|e| {
        e.upstream == Upstream::Origin(origin_id.clone())
            && e.downstream == node_id
            && e.column.as_ref().is_some_and(|c| {
                c.upstream_column.as_str() == "first_name"
                    && c.downstream_column.as_str() == "first_name"
            })
    });
    assert!(passthrough, "expected an identity edge for first_name");
}

#[test]
fn resolves_column_lineage_through_a_join_across_two_upstream_models() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let dim_customers = project
        .nodes
        .iter()
        .find(|n| n.name == "dim_customers")
        .expect("dim_customers should exist");

    let column_names: Vec<&str> = dim_customers
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        column_names,
        vec![
            "customer_id",
            "first_name",
            "last_name",
            "first_order_date",
            "most_recent_order_date",
            "number_of_orders",
        ]
    );

    let stg_customers_id = NodeId::new("model.zhao_dbt_test.stg_customers");
    let dim_customers_id = NodeId::new("model.zhao_dbt_test.dim_customers");

    // `customers.customer_id` -- qualified reference through the
    // "customers" CTE (a pure `select *` passthrough of stg_customers).
    let customer_id_traced = project.edges.iter().any(|e| {
        e.upstream == Upstream::Node(stg_customers_id.clone())
            && e.downstream == dim_customers_id
            && e.column.as_ref().is_some_and(|c| {
                c.upstream_column.as_str() == "customer_id"
                    && c.downstream_column.as_str() == "customer_id"
            })
    });
    assert!(
        customer_id_traced,
        "expected customer_id to trace back to stg_customers"
    );

    // `coalesce(customer_orders.number_of_orders, 0) as number_of_orders`
    // is computed, not a plain column reference -- it should exist in the
    // schema but carry no resolved column-level source.
    let number_of_orders_has_no_column_edge = !project.edges.iter().any(|e| {
        e.downstream == dim_customers_id
            && e.column
                .as_ref()
                .is_some_and(|c| c.downstream_column.as_str() == "number_of_orders")
    });
    assert!(
        number_of_orders_has_no_column_edge,
        "a computed (coalesce) column should not have a resolved column-level source"
    );
}

#[test]
fn baseline_node_level_edges_exist_even_without_column_resolution() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let stg_orders_id = NodeId::new("model.zhao_dbt_test.stg_orders");
    let dim_customers_id = NodeId::new("model.zhao_dbt_test.dim_customers");

    // dbt's own dependency list says dim_customers depends on stg_orders
    // (via the customer_orders CTE's aggregates) -- this must be tracked
    // at the node level even though the aggregate columns themselves
    // (first_order_date, etc.) have no resolvable single source column.
    let baseline_edge_exists = project.edges.iter().any(|e| {
        e.upstream == Upstream::Node(stg_orders_id.clone()) && e.downstream == dim_customers_id
    });
    assert!(
        baseline_edge_exists,
        "expected a node-level edge from stg_orders to dim_customers"
    );
}

#[test]
fn adapter_vocabulary_matches_dbt_terms() {
    let adapter = DbtAdapter;
    let vocab = adapter.vocabulary();
    assert_eq!(vocab.node_term(), "model");
    assert_eq!(vocab.origin_term(), "source");
}

#[test]
fn missing_manifest_file_produces_an_io_error() {
    let result = DbtAdapter.parse(Path::new("/nonexistent/path/manifest.json"));
    assert!(matches!(
        result,
        Err(zhao_core::adapters::dbt::DbtAdapterError::Io { .. })
    ));
}

#[test]
fn malformed_manifest_produces_an_invalid_manifest_error() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "zhao-test-malformed-manifest-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, "{ not valid json").expect("should be able to write temp file");

    let result = DbtAdapter.parse(&path);
    let _ = std::fs::remove_file(&path);

    assert!(matches!(
        result,
        Err(zhao_core::adapters::dbt::DbtAdapterError::InvalidManifest { .. })
    ));
}

#[test]
fn resolves_a_join_and_aggregation_across_two_upstream_models() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let fct_orders = project
        .nodes
        .iter()
        .find(|n| n.name == "fct_orders")
        .expect("fct_orders should exist");

    let column_names: Vec<&str> = fct_orders.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        column_names,
        vec!["order_id", "customer_id", "order_date", "status", "amount"]
    );

    let stg_orders_id = NodeId::new("model.zhao_dbt_test.stg_orders");
    let fct_orders_id = NodeId::new("model.zhao_dbt_test.fct_orders");

    // `orders.order_id` -- qualified reference through the "orders" CTE
    // (a pure passthrough of stg_orders).
    let order_id_traced = project.edges.iter().any(|e| {
        e.upstream == Upstream::Node(stg_orders_id.clone())
            && e.downstream == fct_orders_id
            && e.column.as_ref().is_some_and(|c| {
                c.upstream_column.as_str() == "order_id"
                    && c.downstream_column.as_str() == "order_id"
            })
    });
    assert!(
        order_id_traced,
        "expected order_id to trace back to stg_orders"
    );

    // `coalesce(order_payments.total_amount, 0) as amount` is computed --
    // no resolved column-level source.
    let amount_has_no_column_edge = !project.edges.iter().any(|e| {
        e.downstream == fct_orders_id
            && e.column
                .as_ref()
                .is_some_and(|c| c.downstream_column.as_str() == "amount")
    });
    assert!(
        amount_has_no_column_edge,
        "a computed (coalesce) column should not have a resolved source"
    );
}

#[test]
fn resolves_an_incremental_model_the_same_as_an_equivalent_non_incremental_one() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    // fct_orders_incremental has the same shape as fct_orders (it's the
    // same query, just wrapped in dbt's `is_incremental()` macro, which
    // compiles to nothing on a from-scratch build) -- resolution must not
    // regress for the incremental materialization specifically.
    let incremental = project
        .nodes
        .iter()
        .find(|n| n.name == "fct_orders_incremental")
        .expect("fct_orders_incremental should exist");

    let column_names: Vec<&str> = incremental
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        column_names,
        vec!["order_id", "customer_id", "order_date", "status", "amount"]
    );

    let stg_orders_id = NodeId::new("model.zhao_dbt_test.stg_orders");
    let incremental_id = NodeId::new("model.zhao_dbt_test.fct_orders_incremental");

    let order_id_traced = project.edges.iter().any(|e| {
        e.upstream == Upstream::Node(stg_orders_id.clone())
            && e.downstream == incremental_id
            && e.column.as_ref().is_some_and(|c| {
                c.upstream_column.as_str() == "order_id"
                    && c.downstream_column.as_str() == "order_id"
            })
    });
    assert!(
        order_id_traced,
        "expected order_id to trace back to stg_orders on the incremental model too"
    );

    let amount_has_no_column_edge = !project.edges.iter().any(|e| {
        e.downstream == incremental_id
            && e.column
                .as_ref()
                .is_some_and(|c| c.downstream_column.as_str() == "amount")
    });
    assert!(
        amount_has_no_column_edge,
        "the computed amount column should not have a resolved source on the incremental model either"
    );
}

#[test]
fn extracts_the_join_kind_from_a_models_final_select() {
    let project = DbtAdapter
        .parse(fixture_path())
        .expect("fixture should parse");

    let dim_customers = project
        .nodes
        .iter()
        .find(|n| n.name == "dim_customers")
        .expect("dim_customers should exist");

    // `from customers left join customer_orders using (customer_id)`.
    assert_eq!(dim_customers.joins, vec![JoinKind::Left]);

    let stg_customers = project
        .nodes
        .iter()
        .find(|n| n.name == "stg_customers")
        .expect("stg_customers should exist");

    // No joins anywhere in stg_customers' definition.
    assert_eq!(stg_customers.joins, Vec::<JoinKind>::new());
}
