# Understanding lineage

`zhao lineage` turns your project's column-level lineage into a single, self-contained HTML
file you open in a browser — no server, no build step, no account, works fully offline (there's
nothing in the file that fetches anything: no CDN script, no font, no external stylesheet).
It's meant for exploring a project, not for CI. HTML is the default output; pass `--text` for
the old plain-text report instead.

```bash
zhao lineage                                # the whole project, written to a computed path
open target/zhao/lineage_graphs/full_lineage.html   # macOS; xdg-open on Linux, or just double-click it
```

## Where it's written

With no `--html <path>`, the export is written under `<project-dir>/target/zhao/lineage_graphs/`
at a computed path, overwritten in place on repeat runs (no timestamping, no accumulation):

| Invocation | Default path |
|---|---|
| no target | `full_lineage.html` |
| `<model>` | `partial_lineage_<model>.html` |
| `+<model>` | `partial_lineage_<model>_upstream_only.html` |
| `<model>+` | `partial_lineage_<model>_downstream_only.html` |
| `<model>.<column>` | `partial_lineage_<model>_<column>.html` |
| `+<model>.<column>` | `partial_lineage_<model>_<column>_upstream_only.html` |
| `<model> --package <pkg>` | `partial_lineage_<pkg>_<model>.html` |

Pass `--html <path>` to write to an explicit path instead:

```bash
zhao lineage --html lineage.html            # the whole project, at an explicit path
```

**[Live demo](assets/lineage-demo.html)** — an actual export from a small fixture project,
committed in this repo. Open it in your browser to try the real thing rather than take our
word for what it does.

![Clicking a model, expanding columns, then tracing a calculated column's real upstream source](assets/lineage-demo.gif)

*(A real capture of the live demo above — click a model, turn on "Columns", click a
calculated column to trace it back to its actual source, then search for `model.column`.)*

## What you get

- **The whole DAG**, laid out left-to-right by dependency depth — sources on the left,
  everything they feed into further right.
- **Click a model** to highlight everything upstream and downstream of it; everything
  unrelated dims out.
- **Toggle "Columns"** to expand every model into its actual output columns — not just
  whatever's documented in `schema.yml` (dbt's manifest only ever has that), but zhao's own
  static resolution of each model's real output schema.
- **Click a column** to trace its specific lineage: which upstream columns it's actually
  derived from (including a calculated column like `coalesce(orders.amount, 0)`, traced back
  through however many CTEs it took), and which downstream columns actually consume it. A
  column zhao's static SQL resolver couldn't fully trace (a `UNION`, a window function, a
  subquery in `FROM`) is shown as `(unresolved)` — visibly, never silently dropped or shown
  as if fully traced.
- **Search** the top bar by model name, or by `model.column` to jump straight to a specific
  column in a large project without opening its model first.
- **Sort and filter columns** within a model's panel — useful once a model has more columns
  than fit comfortably on screen. Defaults to the model's actual `SELECT` order; toggle to
  A–Z/Z–A.

## Scoping the initial view

Pass a target the same way you would for `--text` output, and the export opens already focused
on it:

```bash
zhao lineage fct_orders            # opens with fct_orders selected
zhao lineage fct_orders.amount     # opens at column grain
```

For a large project, a targeted export also renders *only* the target's related subgraph by
default (its full upstream + downstream transitive closure) rather than the whole DAG — a
banner at the top says so, with a **Show whole project** button. The whole project's data is
still fully embedded either way; expanding doesn't regenerate the file, and search still finds
(and automatically expands to) anything outside the current view, so nothing is ever
unreachable, just not rendered until you ask for it. A whole-project export (no target) has
nothing to scope down from, so it always opens fully expanded.

## Getting a fresh export

By default, `zhao lineage` reads whatever's already in `<project-dir>/target/manifest.json` —
the same "compile it yourself first" contract every other zhao command has. Pass `--compile` to
have zhao run `dbt compile` first:

```bash
zhao lineage --compile
```

## What "static resolution" can and can't trace

Every column-to-column edge shown is a *real*, resolved reference — zhao never guesses. A
calculated column (a function call, `CAST`, arithmetic, `CASE`) traces to every column it
structurally references, however many CTEs deep, and its rendered SQL is shown alongside it.
What it deliberately doesn't attempt: `UNION`/`UNION ALL`, a subquery inline in `FROM` (as
opposed to a CTE), and window functions — these fall back to a real, node-level-tracked
dependency with an unresolved column mapping, rather than a wrong guess. That's a deliberate
trade: an admittedly-unresolved edge is safer than a confidently-wrong one.
