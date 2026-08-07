# Configuring `zhao.yml`

`zhao.yml`, at your dbt project's root (or your repo's root, for a monorepo — see
[Monorepos](#monorepos-multiple-dbt-projects) below), holds your team's policy: which kind
of change is an error, a warning, or informational-only, and (optionally) where to defer
to for CI. It's entirely optional — a project with none behaves exactly like zhao's
built-in defaults.

Preferred over CLI flags on purpose: a `zhao.yml` is versioned and reviewable alongside the
code it governs, instead of hidden in a CI script only the person who wrote it remembers.

## The Rule catalog

Every change zhao can detect maps to one Rule. Each Rule has a default severity —
`error` (fails `zhao check`), `warn` (shown, doesn't fail), or `pass` (informational only,
not shown in Downstream impact unless something else made the model impactful):

| Rule (`zhao.yml` name) | Default severity | Fires when |
|---|---|---|
| `column-removed-with-active-references` | `error` | A column was removed while a downstream model still actively references it (the Baseline shows a real column-level edge into it). |
| `column-type-narrowed` | `warn` | A column's documented type narrowed (e.g. `bigint` → `int`) — silent truncation risk. |
| `join-cardinality-loosened` | `warn` | A join's cardinality loosened (`INNER` → `LEFT`/`FULL`) — potential row-count/duplication regression. |
| `column-added` | `pass` | A column was added. Informational by default — nothing downstream could already depend on a column that didn't exist. |

## Presets

A named bundle of severities across the whole catalog, applied before any per-Rule override:

```yaml
preset: strict   # every warn -> error; error stays error
```

```yaml
preset: lenient   # every error -> warn, every warn -> pass; pass stays pass
```

```yaml
preset: default   # zhao's built-in defaults, unchanged -- the same as omitting `preset` entirely
```

## Per-Rule overrides

Layered on top of whichever Preset is active — an override always wins for that Rule only,
every other Rule still follows the Preset:

```yaml
preset: strict
rules:
  column-added: warn   # overrides strict's default (still error) for this one Rule
```

Valid severities: `error`, `warn`, `pass`.

## `defer`

Backs the report's ready-to-run `--defer` command (see [`zhao check`'s report
sections](commands.md#report-sections-text-output)):

```yaml
defer:
  target: prod                              # a human-readable label, shown next to the command
  state: artifacts/prod/manifest.json       # the path dbt's --state flag needs
```

Both are optional and independent. `state` alone still produces a full, ready-to-run
`dbt build --select <impacted models> --defer --state <path>` command; `target` alone (no
`state`) labels the plan without a command, since dbt's `--defer` mechanism has nothing to
function without a state path. `--defer-target`/`--defer-state` CLI flags override either
value when given.

## `against`

The branch `zhao check`/`zhao diff`'s git-native Baseline resolution finds a merge-base
against, when `--state` isn't given:

```yaml
against: main
```

Saves passing `--against main` on every invocation just because your default branch isn't
called `master` (zhao's own built-in default). The CLI `--against` flag still overrides this
when given; with neither set, zhao falls back to `master`.

## `dbt-command`/`dbt-args`

The executable (and, if you like, a wrapper's own leading flags) zhao invokes for every `dbt`
subprocess call it makes itself (`dbt deps`/`dbt compile` for git-native Baseline resolution,
`dbt run-operation` for `--check-relations`, `dbt compile` for `zhao lineage --compile`):

```yaml
dbt-command: dbt          # zhao's own default -- resolved via PATH
dbt-command: uv run dbt   # a project that only ever invokes dbt through uv
dbt-command: dw some-flag # a custom in-house wrapper, with its own leading flag
```

Shell-word-split, so a multi-word value works as a genuine prefix — `dw some-flag` runs as
`dw some-flag deps`/`dw some-flag compile`/etc, not as one literal (nonexistent) executable
named `"dw some-flag"`. Assumes you know what your own wrapper does with any flags placed
ahead of the subcommand; zhao never interprets them. The CLI `--dbt-command` flag overrides
this when given; with neither set, zhao falls back to `"dbt"`.

`dbt-args` (or the CLI's `--dbt-arg`/`--dbt-args`) is separate and additive — extra arguments
appended *after* the subcommand (`--target`, `--vars`, ...), not part of the command prefix
itself:

```yaml
dbt-args: "--target ci"
```

## `log`

Settings for the daily-rotating run log (`target/zhao/logs/<date>.log` -- see
[Command reference](commands.md#the-daily-run-log)):

```yaml
log:
  level: mirror       # or: debug
  retention_days: 30  # omit entirely to keep everything, forever
```

`level` -- `mirror` (the default) is a literal mirror of whatever was already printed to
stdout. `debug` is accepted and parsed but not yet wired to anything -- there's no debug-level
content defined yet for it to switch to; reserved so a later ticket adding real debug-level
content doesn't need another config-shape change. A CLI `--log-level` flag exists on
`check`/`diff`/`lineage` for the same reason, also not yet connected to anything.

`retention_days` -- how many days of `target/zhao/logs/` history to keep; log files older than
this are purged on every run. Omitted by default, which means no purging happens at all --
matching the assumption that most environments running zhao are disposable anyway. A CLI
`--purge-logs <days>` flag overrides this for a single run without changing `zhao.yml`.

## Monorepos: multiple dbt projects

A root-level `zhao.yml` (at the nearest ancestor directory containing `.git`) acts as the
org-wide default; each individual dbt project can layer its own `zhao.yml` on top,
overriding only the keys it sets — everything else still inherits from the root. This
applies independently, key by key: a project can override just `defer.state` and still
inherit the root's `preset` and rule overrides untouched.

```
repo-root/
├── .git/
├── zhao.yml              # org-wide default: preset: strict
└── services/
    └── analytics/
        ├── zhao.yml       # this project only: rules: { column-added: warn }
        └── dbt_project.yml
```

Running `zhao check --project-dir services/analytics` here gets `strict` from the root
*and* the `column-added: warn` override from the project-local file — the same override
relationship a Preset already has to individual Rule overrides, one layer higher.

## Full example

```yaml
preset: strict

rules:
  column-added: pass   # deviate from strict's default for this one Rule

defer:
  target: prod
  state: artifacts/prod/manifest.json

against: main
```
