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
| `--project-dir <dir>` | `.` | The dbt project to check. Its current state is read from `<dir>/target/manifest.json` — run `dbt compile` in the project first; zhao refuses to run against a stale one (see `--allow-stale-manifest` below). Which Transformation Tool Adapter to read it with is auto-detected from `<dir>` itself (a `dbt_project.yml` marker means dbt, the only adapter zhao ships today); see [`tool`](configuration.md#tool) for the (rarely needed) fallback. |
| `--against <ref>` | `master` | The branch to find a merge-base against, for git-native Baseline resolution. Ignored when `--state` is given. Overrides `zhao.yml`'s `against` when given — see [Configuring `zhao.yml`](configuration.md#against). |
| `--format <text\|json>` | `text` | `json` is the machine-readable shape everything else in this table also applies to — build a PR-comment bot or dashboard on top of it without scraping text. |
| `--no-color` | — | Disables ANSI color in text output (auto-detected otherwise; no effect on `--format json`). |
| `--dbt-arg <arg>` | — | Appends one extra argument to every internal `dbt deps`/`dbt compile` call (git-native Baseline resolution only). Repeatable, e.g. `--dbt-arg --target --dbt-arg ci`. Mutually exclusive with `--dbt-args`. |
| `--dbt-args "<string>"` | — | Same, but as one shell-word-style string to split, e.g. `--dbt-args "--target ci --vars '{\"key\": \"value\"}'"`. Mutually exclusive with `--dbt-arg`. |
| `--dbt-command "<cmd>"` | `dbt` | The executable/prefix for every internal `dbt` call zhao makes itself. Shell-word-split, so a wrapper's own leading flags work too (e.g. `"uv run dbt"`, or a custom in-house wrapper like `"myshell custom-flag"`). Overrides `zhao.yml`'s `dbt-command` when given — see [Configuring `zhao.yml`](configuration.md#dbt-commanddbt-args). |
| `--check-relations` | — | Opt-in: actually checks whether each flagged incremental model exists in your configured target (same connection `dbt run` already needs), turning the conditional schema-evolution note into a definitive one. |
| `--defer-target <name>` | — | A human-readable label (e.g. `"prod"`) for the target the `--defer` plan defers to. Shown next to the generated command; not passed to dbt itself. Overrides `zhao.yml`'s `defer.target`. |
| `--defer-state <path>` | — | A compiled manifest path to defer to — when set, the report includes a ready-to-run `dbt build --select ... --defer --state <path>` command. Overrides `zhao.yml`'s `defer.state`. |
| `--allow-stale-manifest` | — | Skips the check that `<project-dir>/target/manifest.json` is newer than the project's own dbt source files (`dbt_project.yml`, `packages.yml`/`dependencies.yml`, and everything under `models/`, `macros/`, `seeds/`, `snapshots/`, `analyses/`, `tests/`). Without this flag, a stale manifest — e.g. checked out on a different branch, or pulled without rerunning `dbt compile` — fails fast (exit `2`) instead of silently producing an incorrect diff. Not recommended; exists for cases like a hand-supplied test fixture with no real dbt project alongside it. |

**Exit codes**: `0` nothing breaking; `1` a `BREAKING` finding fired; `2` zhao itself
couldn't run (bad path, unparsable manifest, stale manifest, `dbt` not invokable, merge-base
not found, ...).

### Report sections (text output)

1. **Changed** — every model that actually changed, with the precise change (column
   added/removed/type-changed, join altered).
2. **Downstream impact** — every model actually *reached* by a change (never the whole
   downstream cone), each labeled `[BREAKING]` or `[WARN]` with the specific reference and
   the Rule that fired.
3. **Summary** — a one-line count, plus (if applicable) an `Impacted models:` list and a
   `Defer plan:` section (see [Configuring `defer`](configuration.md#defer)).

`Impacted models` is deliberately just the list of model names, not a constructed command —
zhao has no way to know whether your CI actually invokes `dbt build`, `dbt run`, or a custom
wrapper, so it never assumes one. Build your own command from the list, e.g.
`dbt build --select $(echo "$names" | tr ',' ' ')`, or (more robustly, avoiding any shell
word-splitting concerns) pull the same list straight from `--format json`'s `impacted_models`
array — a real JSON array of strings, ready for a script to consume directly without any
text-parsing at all.

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

`TARGET` is optional: omit it and the export embeds the whole project instead of scoping to one
model.

| Flag | Default | What it does |
|---|---|---|
| `--project-dir <dir>` | `.` | The dbt project to query. Which Transformation Tool Adapter to read it with is auto-detected the same way as `zhao check`'s `--project-dir` — see [`tool`](configuration.md#tool). |
| `--html <path>` | — | Writes the interactive HTML export to this explicit path instead of the computed default (see below). HTML is already the default output mode — this only overrides *where* it's written. |
| `--text` | — | Prints the plain-text report to stdout instead of HTML. `TARGET` is required with this flag. |
| `--compile` | — | Runs `dbt compile` first, for a guaranteed-fresh view. Without it, the existing `target/manifest.json` is read as-is. |
| `--package <name>` | — | Disambiguates `TARGET` when its bare model name matches more than one model across dbt packages (a real but uncommon shape — internal packages, dbt Mesh). The error without this flag names every candidate's full ID, which is exactly what to pass here. |

### The interactive HTML export (default)

With no `--text`/`--html`, `zhao lineage` writes a self-contained, interactive HTML lineage
graph under `target/zhao/lineage_graphs/` at a computed path — see
[Understanding lineage](lineage-html.md) for what you get and the full filename table.

```bash
zhao lineage                              # whole project -> target/zhao/lineage_graphs/full_lineage.html
zhao lineage fct_orders                   # one model -> .../partial_lineage_fct_orders.html
zhao lineage fct_orders.amount            # one column -> .../partial_lineage_fct_orders_amount.html
zhao lineage --html out.html fct_orders   # explicit path, same scoping rules
```

### `--text`: the plain-text report

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

### `target/zhao/full_lineage.json`

Every invocation also unconditionally writes `target/zhao/full_lineage.json` — the whole
project's lineage graph, independent of `TARGET`/`--text`/`--html`, meant for other tooling to
read directly. See [Understanding lineage](lineage-html.md#targetzhaofull_lineagejson).

## The daily run log

Every `check`/`diff`/`lineage` run also appends its own stdout to
`target/zhao/logs/<YYYY-MM-DD>.log`, one file per calendar day, rotated at midnight — always
on, no flag needed, the same "unconditional machine-readable output" precedent as
`target/zhao/run-metadata.json`. This never changes what's actually printed to the real
stdout; it's a mirror, not a redirect.

| Flag | Default | What it does |
|---|---|---|
| `--log-level <mirror\|debug>` | `mirror` | Accepted and parsed (see [Configuring `log`](configuration.md#log)) but not yet wired to anything — the run log has no `debug`-level content defined yet for either this or `zhao.yml`'s `log.level` to switch to. Reserved so a later ticket adding real debug-level content doesn't need another config-shape change. |
| `--purge-logs <days>` | — | One-off override for `zhao.yml`'s `log.retention_days` (see [Configuring `log`](configuration.md#log)): purges `target/zhao/logs/` files older than `<days>` days for this run only, without changing `zhao.yml`. |

With neither `--purge-logs` nor `zhao.yml`'s `log.retention_days` set, no purging happens at
all — `target/zhao/logs/` accumulates indefinitely by default, matching the assumption that
most environments running zhao are disposable anyway. Purging only ever removes files under
`target/zhao/logs/` matching the `<YYYY-MM-DD>.log` pattern this log itself writes and older
than the configured window — never any other `target/zhao/` artifact, and never a log still
within the window.

Any internal `dbt compile`/`dbt deps` subprocess zhao itself runs — `zhao check`/`zhao diff`'s
git-native Baseline resolution, and `zhao lineage --compile` — has its own captured
stdout/stderr routed into the same day's log entry too, both on success (previously discarded
entirely) and on failure (in addition to the terminal error message dbt compile/deps failures
already surface) — so an internal compile/deps run is always inspectable after the fact,
without ever printing dbt's raw output to the terminal directly (which would otherwise corrupt
`--format json` piping).

## `zhao update`

Replaces the currently running `zhao` binary in place with a release fetched from
[GitHub Releases](https://github.com/allenhori/zhao-cli/releases) — the only zhao command that
reaches the network at all, and only to download the binary itself; it never sends anything
from your project (see [What it doesn't do](../README.md#what-it-doesnt-do)).

```bash
zhao update              # the latest stable release
zhao update --nightly    # the current nightly build
zhao update v0.1.1       # pinned to an exact release tag
```

Kept deliberately simple: updates by release tag, not a semver-range resolver. `VERSION` and
`--nightly` are mutually exclusive.

**Exit codes**: `0` the binary was replaced; `2` couldn't run (unsupported platform, the tag/
release doesn't exist, a download or extraction failure, ...). A failure at any point before the
final replace leaves the existing binary completely untouched — never a broken or partial
binary in place. Run `zhao --version` afterward to confirm the update actually took effect.
