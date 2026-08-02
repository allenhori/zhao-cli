# Architecture

A living map of how the code is organized, updated as functionality lands.
It's a companion to the crate-level documentation (`cargo doc --open`), not
a replacement for it — read the doc comments for anything this file
doesn't cover.

## Workspace layout

- `crates/zhao-core` — the actual engine. Format-agnostic: it has no
  knowledge of dbt, or of any other specific transformation tool or
  warehouse. Everything here operates on zhao's own vocabulary.
- `crates/zhao-cli` — a thin binary (`zhao`) built on top of `zhao-core`.
  Owns argument parsing, output formatting, and process exit codes; delegates
  all actual analysis to `zhao-core`.

Why split at all, for what's currently a small project: this boundary is
what keeps `zhao-core` reusable outside the CLI (a future consumer that
links it directly, without going through a subprocess) and keeps the CLI
crate itself free of analysis logic that would otherwise be tempting to
inline. The split is cheap to keep now and expensive to introduce later
once the two are tangled together.

## Domain vocabulary (zhao-core)

- **Node** — the atomic buildable thing the engine reasons about (a dbt
  `model` maps to a Node via the dbt adapter).
- **Origin** — an external input a Node reads but doesn't build (a dbt
  `source` maps to an Origin).
- **Lineage Edge** — a reference from one Node/Origin to another, tracked at
  both node level and column level.
- **Change** — a diff between two versions of a Node's schema and
  definition.

## Adapter boundaries

Two independent trait boundaries keep the engine decoupled from any one
tool or warehouse:

- **`TransformationToolAdapter`** — reads a specific project format and
  produces Nodes, Origins, and Lineage Edges. dbt is the first
  implementation. Nothing outside an adapter's own module should depend on
  that tool's specific types — code elsewhere only ever sees the trait's
  output.
- **`WarehouseAdapter`** — warehouse-specific connection and dialect
  behavior (Snowflake, Databricks, BigQuery, ...), used wherever the engine
  needs to talk to a real warehouse.

Adding support for a new transformation tool or warehouse should mean
adding a new implementation of one of these traits, not changing the
engine itself.

## Where new code goes

Before adding a module, check whether it's genuinely a new concern or
belongs inside an existing one. A handful of well-organized files that each
own a coherent piece of the domain beats many thin files split apart for
their own sake — split a module when it's grown enough to be hard to read
in one sitting, not before.

## Contributing

- Every public item needs a doc comment, explaining what it does and its
  inputs/outputs — enforced by `missing_docs = "deny"` in the workspace's
  `[lints]` table, so this fails the build, not just a style review.
- Before opening a PR, `cargo fmt --all -- --check`, `cargo clippy
  --workspace --all-targets -- -D warnings`, and `cargo test --workspace`
  should all pass locally. CI runs all three, plus `cargo doc` with
  warnings denied to catch broken doc links.
