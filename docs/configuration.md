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
