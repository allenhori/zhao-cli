# hello-zhao-addon

A minimal, deliberately trivial reference implementation of the **zhao-cli Addon contract** —
not a real Addon, a teaching example. This *is* the "how to build a zhao-cli Addon" guide;
reading `src/main.rs` alongside this page is the fastest way to understand the whole contract.

For a real Addon built on this same contract, see
[`zhao-dbt-plan`](https://github.com/allenhori/zhao-dbt-plan) — a dbt microbatch cascading
time-window planner, AGPLv3, its own separate repo.

## Try it

```bash
cd examples/hello-zhao-addon
cargo build
cd /path/to/some/dbt/project
zhao lineage --project-dir .          # writes target/zhao/full_lineage.json
/path/to/hello-zhao-addon/target/debug/zhao-hello --project-dir .
```

You should see one line per model/source in your project, and a new
`target/zhao/hello_plan.json` file.

To try it as `zhao hello` (discovered on `PATH`, the way a real installed Addon would be), put
the built `zhao-hello` binary on your `PATH` alongside `zhao` itself, then just run:

```bash
zhao hello --project-dir .
```

## The contract, in three parts

### 1. Discovery: PATH + naming convention, nothing more

`zhao-cli` has **zero compiled-in knowledge** of this Addon, or any Addon. When you run `zhao
<name>` and `<name>` isn't one of `zhao`'s own built-in subcommands (`check`, `diff`, `lineage`,
`update`), it looks for a binary literally named `zhao-<name>` on `PATH` and execs it, forwarding
every argument after `<name>` verbatim. See `crates/zhao-cli/src/addon.rs` in the main `zhao-cli`
crate for the actual dispatch code — it's short.

That's the entire discovery mechanism. No manifest to register, no plugin API to implement, no
trait to satisfy. If `zhao-<name>` exists on `PATH` and is executable, it's discoverable as `zhao
<name>`.

**This Addon has no technical dependency on `zhao-cli` either.** Nothing above requires
`zhao-cli` to invoke this binary — you just ran it directly, with no `zhao` binary involved at
all. The dependency runs one direction only, and it's practical, not technical: `zhao dbt-plan`
is more discoverable than a user having to already know `zhao-dbt-plan` exists.

### 2. Input: read zhao-cli's own artifacts, don't ask it for anything

`zhao lineage` always writes `target/zhao/full_lineage.json`, unconditionally, on every run,
regardless of what else it was asked to do (`--text`, `--html`, a specific target — it doesn't
matter, this file is always written). That's this Addon's entire input: it reads that file
directly off disk. It never runs `zhao` itself, never asks it a question at runtime, never links
against `zhao-core`.

```json
{
  "nodes": [{ "id": "...", "name": "...", "kind": "node", "layer": 0, "columns": [...] }],
  "edges": [{ "upstream": "...", "downstream": "...", "upstream_column": "...", "downstream_column": "..." }]
}
```

(`src/main.rs`'s `FullLineage`/`LineageNode`/`LineageEdge` structs read only the subset this
example actually uses — `id`/`name`/`kind` and `upstream`/`downstream` — not the whole shape.
See `crates/zhao-cli/src/lineage_html.rs`'s `GraphNode`/`GraphEdge` in the main crate for the
authoritative, complete shape if you need more of it.)

If your own Addon needs something this file doesn't carry (e.g. `zhao-dbt-plan` needs
`config.meta.zhao` and `event_time`, which aren't lineage data at all), read it from wherever
*that* data actually lives instead — for `zhao-dbt-plan`, that's dbt's own compiled
`manifest.json`, a file dbt itself already writes, not something `zhao-cli` needs to hand you.
The pattern is the same either way: read an existing, well-known file; never ask `zhao-cli` (or
anything else) a question at runtime that a file can already answer.

### 3. Output: your own fixed path, your own shape

This Addon writes `target/zhao/hello_plan.json` — a fixed, predictable path, so anything
downstream (a human, a script, a CI step) knows exactly where to look without needing to ask
this binary anything either. The *shape* of that file is entirely this Addon's own business —
`zhao-cli` has no opinion about it, validates nothing, and never reads it back. Pick a name and
shape that make sense for what your Addon actually does; `hello_plan.json`'s trivial `{greeting,
node_count, edge_count, downstream_counts}` shape exists purely to prove "write something," not
as a pattern to copy.

## Why this isn't a workspace member

`examples/hello-zhao-addon/`'s `Cargo.toml` has its own `[workspace]` table, deliberately making
it a separate Cargo workspace root rather than a member of the main `zhao-cli`/`zhao-core`
workspace one directory up. This is load-bearing, not incidental: a real third-party Addon
author has no access to `zhao-core`'s internals, only to `zhao-cli`'s public, documented
artifacts (`full_lineage.json`, and whatever else `zhao-cli` writes to `target/zhao/`). If this
example were a workspace member, it would be trivially easy to accidentally `use zhao_core::...`
and end up demonstrating something a real external Addon author couldn't actually do. Being a
fully separate workspace makes that mistake structurally impossible instead of just
discouraged.

## What this deliberately doesn't do

- No real analysis. `zhao-hello` counts nodes and edges; that's it.
- No `clap` dependency, or any CLI-parsing crate at all — one hand-rolled `--project-dir` flag is
  all this needs to demonstrate. Use whatever fits your own Addon.
- No error-hardening beyond "print a clear message and exit 1." A real Addon should be much more
  careful about this than a teaching example needs to be.

The contract is deliberately just a subprocess plus files — no shared library, no compiled-in
knowledge on `zhao-cli`'s side of any specific Addon, discovery purely by the `zhao-<name>`
naming convention on `PATH`. That's what keeps an Addon like this one entirely independent:
any language, any license, any release cadence, as long as it honors the three-part contract
above.
