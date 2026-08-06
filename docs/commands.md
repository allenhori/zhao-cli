# Command reference

`zhao` has three subcommands: [`check`](#zhao-check) (the CI gate), [`diff`](#zhao-diff)
(the same engine, no gate), and [`lineage`](#zhao-lineage) (a structural query, no diff at
all). `zhao --help`, `zhao check --help`, etc. print the same information from the binary
itself if this page ever drifts.

## `zhao check`

Diffs the current project against a Baseline, evaluates the Rule catalog, and exits non-zero
if anything fired at `error` severity.

```bash
zhao check [OPTIONS]
```

| Flag | Default | What it does |
|---|---|---|
| `--state <path>` | — | Use an already-compiled `manifest.json` as the Baseline instead of resolving one via git. |
| `--project-dir <dir>` | `.` | The dbt project to check. Its current state is read from `<dir>/target/manifest.json`. |
| `--against <ref>` | `master` | The branch to find a merge-base against, for git-native Baseline resolution. Ignored when `--state` is given. Overrides `zhao.yml`'s `against` when given — see [Configuring `zhao.yml`](configuration.md#against). |
| `--format <text\|json>` | `text` | `json` is the machine-readable shape everything else in this table also applies to — build a PR-comment bot or dashboard on top of it without scraping text. |
| `--no-color` | — | Disables ANSI color in text output (auto-detected otherwise; no effect on `--format json`). |
| `--dbt-arg <arg>` | — | Appends one extra argument to every internal `dbt deps`/`dbt compile` call (git-native Baseline resolution only). Repeatable, e.g. `--dbt-arg --target --dbt-arg ci`. Mutually exclusive with `--dbt-args`. |
| `--dbt-args "<string>"` | — | Same, but as one shell-word-style string to split, e.g. `--dbt-args "--target ci --vars '{\"key\": \"value\"}'"`. Mutually exclusive with `--dbt-arg`. |
| `--check-relations` | — | Opt-in: actually checks whether each flagged incremental model exists in your configured target (same connection `dbt run` already needs), turning the conditional schema-evolution note into a definitive one. |
| `--defer-target <name>` | — | A human-readable label (e.g. `"prod"`) for the target the `--defer` plan defers to. Shown next to the generated command; not passed to dbt itself. Overrides `zhao.yml`'s `defer.target`. |
| `--defer-state <path>` | — | A compiled manifest path to defer to — when set, the report includes a ready-to-run `dbt build --select ... --defer --state <path>` command. Overrides `zhao.yml`'s `defer.state`. |

**Exit codes**: `0` nothing breaking; `1` a `BREAKING` finding fired; `2` zhao itself
couldn't run (bad path, unparsable manifest, `dbt` not invokable, merge-base not found, ...).

### Report sections (text output)

1. **Changed** — every model that actually changed, with the precise change (column
   added/removed/type-changed, join altered).
2. **Downstream impact** — every model actually *reached* by a change (never the whole
   downstream cone), each labeled `[BREAKING]` or `[WARN]` with the specific reference and
   the Rule that fired.
3. **Summary** — a one-line count, plus (if applicable) a ready-to-copy
   `dbt build --select <impacted models>` command and a `Defer plan:` section (see
   [Configuring `defer`](configuration.md#defer)).

A **Schema evolution** section appears whenever a schema-changing change (column
added/removed/type-changed) lands on a model materialized `incremental` — phrased
conditionally ("if this model already exists in your target...") unless you pass
`--check-relations`, in which case it's either upgraded to a definitive statement or dropped
entirely if the model is confirmed not to exist yet.

## `zhao diff`

Identical engine and flags to `zhao check` — same Baseline resolution, same diff, same
report — but **always exits `0`**, regardless of what fired. For inspecting changes locally
during development without CI's pass/fail semantics getting in the way.

```bash
zhao diff [OPTIONS]   # same flags as `zhao check`
```

## `zhao lineage`

A structural query over your project's **current** compiled state: what's upstream or
downstream of a given model (or a specific column on it)? Unlike `check`/`diff`, this reads
no Baseline, touches no git history, and doesn't run `dbt compile` unless you pass
`--compile`.

```bash
zhao lineage [OPTIONS] [TARGET]
```

`TARGET` uses dbt's own selector syntax:

| Form | Meaning |
|---|---|
| `model_name` | Both upstream and downstream. |
| `+model_name` | Upstream only (ancestors). |
| `model_name+` | Downstream only (descendants). |
| `model_name.column_name` | Same, at column grain — traces the specific column's real lineage, not just "this model depends on that one." |

| Flag | Default | What it does |
|---|---|---|
| `--project-dir <dir>` | `.` | The dbt project to query. |
| `--html <path>` | — | Writes a self-contained, interactive HTML lineage graph to `path` instead of printing text — see [Understanding lineage](lineage-html.md). `TARGET` becomes optional with this flag: omit it to embed the whole project. |
| `--compile` | — | Runs `dbt compile` first, for a guaranteed-fresh view. Without it, the existing `target/manifest.json` is read as-is. |
| `--package <name>` | — | Disambiguates `TARGET` when its bare model name matches more than one model across dbt packages (a real but uncommon shape — internal packages, dbt Mesh). The error without this flag names every candidate's full ID, which is exactly what to pass here. |

### Text output

```
Upstream:
  source source.jaffle_shop.raw.raw_customers
  model model.jaffle_shop.stg_customers
Downstream:
  model model.jaffle_shop.fct_orders
```

A column-level target additionally reports any node with real connectivity in that direction
whose specific column mapping couldn't be resolved (a computed expression zhao's static SQL
resolver can't fully trace) as `(unresolved)` — visible, never silently dropped.

### `--html`: the interactive export

```bash
zhao lineage --html lineage.html                    # whole project
zhao lineage --html lineage.html fct_orders          # scoped to one model
zhao lineage --html lineage.html fct_orders.amount   # scoped to one column
open lineage.html
```

See [Understanding lineage](lineage-html.md) for what you get.
