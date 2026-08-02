//! Integration test for [`zhao_core::diff::diff`] against two real
//! compiled manifests: `diff_baseline_manifest.json` and
//! `diff_current_manifest.json`, both compiled from the same dbt project
//! (a modified copy of `zhao-dbt-test`) before and after deliberate
//! changes:
//!
//! - `stg_customers`: `customer_id`'s documented type narrowed from
//!   `bigint` to `int`; `last_name` removed; `marketing_source` (a
//!   computed literal) added.
//! - `dim_customers`: its join with `customer_orders` changed from
//!   `LEFT JOIN` to a plain (`INNER`) `JOIN`; it also lost its own
//!   `last_name` column, since removing it upstream (from
//!   `stg_customers`) meant `dim_customers`' own reference to it had to
//!   go too -- a second, real, honest column removal, not a test-fixture
//!   mistake.
//! - Every other model (`stg_orders`, `stg_payments`, `fct_orders`,
//!   `fct_orders_incremental`) is untouched between the two states.

use std::path::Path;
use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::DbtAdapter;
use zhao_core::diff::{Change, diff};
use zhao_core::model::{ColumnName, JoinKind, NodeId};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
}

#[test]
fn detects_the_full_set_of_deliberate_changes_between_two_real_manifests() {
    let baseline = DbtAdapter
        .parse(&fixture("diff_baseline_manifest.json"))
        .expect("baseline fixture should parse");
    let current = DbtAdapter
        .parse(&fixture("diff_current_manifest.json"))
        .expect("current fixture should parse");

    let changes = diff(&baseline, &current);
    let stg_customers = NodeId::new("model.zhao_dbt_test.stg_customers");
    let dim_customers = NodeId::new("model.zhao_dbt_test.dim_customers");

    assert!(
        changes.contains(&Change::ColumnAdded {
            node: stg_customers.clone(),
            column: ColumnName::new("marketing_source"),
        }),
        "expected marketing_source to be detected as added"
    );
    assert!(
        changes.contains(&Change::ColumnRemoved {
            node: stg_customers.clone(),
            column: ColumnName::new("last_name"),
        }),
        "expected last_name to be detected as removed"
    );
    assert!(
        changes.contains(&Change::ColumnTypeChanged {
            node: stg_customers.clone(),
            column: ColumnName::new("customer_id"),
            from_type: "bigint".to_string(),
            to_type: "int".to_string(),
        }),
        "expected customer_id's documented type change (bigint -> int) to be detected"
    );
    assert!(
        changes.contains(&Change::JoinChanged {
            node: dim_customers.clone(),
            position: 0,
            from_kind: Some(JoinKind::Left),
            to_kind: Some(JoinKind::Inner),
        }),
        "expected dim_customers' join to be detected as changed from LEFT to INNER"
    );
    assert!(
        changes.contains(&Change::ColumnRemoved {
            node: dim_customers.clone(),
            column: ColumnName::new("last_name"),
        }),
        "expected dim_customers to also lose its own last_name column"
    );

    // Exactly these five changes, and only for the two models actually
    // touched -- no false positives elsewhere (stg_orders, stg_payments,
    // fct_orders, fct_orders_incremental were not modified).
    assert_eq!(
        changes.len(),
        5,
        "expected exactly the five deliberate changes, got: {changes:#?}"
    );
    for change in &changes {
        let node = match change {
            Change::ColumnAdded { node, .. }
            | Change::ColumnRemoved { node, .. }
            | Change::ColumnTypeChanged { node, .. }
            | Change::JoinChanged { node, .. } => node,
        };
        assert!(
            *node == stg_customers || *node == dim_customers,
            "unexpected change on an untouched node: {change:?}"
        );
    }
}
