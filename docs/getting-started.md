# Getting started

`zhao` reads your dbt project's compiled state and tells you, in plain language, what a
change actually does downstream — before you merge it. This page walks through installing
it and running it against a real dbt project for the first time.

## What you need

- A dbt project (v1 `dbt-core` or v2/Fusion — both produce the `manifest.json` zhao reads).
- `dbt` itself installed and runnable, **only if** you want zhao to resolve a Baseline for
  you automatically (see [Baselines](#baselines) below). If you already have a compiled
  manifest from another point in history, you don't need `dbt` on your `PATH` at all.
- Nothing else. zhao never connects to your warehouse or holds credentials for it directly —
  the fully optional `--check-relations` flag borrows your own dbt profile's already-configured
  connection via `dbt run-operation`, so no secret ever passes through zhao itself. zhao makes
  no network call of its own (`zhao update` is the one exception, and it only downloads a
  release binary), needs no account, and never uploads anything on your behalf.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
```

This downloads the right pre-built binary for your platform (macOS Intel/Apple Silicon,
Linux x86_64) from the [releases page](https://github.com/allenhori/zhao-cli/releases) and
installs it to `~/.zhao/bin`. No Rust toolchain required.

Windows: download `zhao-x86_64-pc-windows-msvc.zip` directly from the
[releases page](https://github.com/allenhori/zhao-cli/releases) and unzip it somewhere on
your `PATH`.

Have Rust already? `cargo install --git https://github.com/allenhori/zhao-cli` works too, and
tracks `master` if you want the bleeding edge between releases.

Check it worked:

```bash
zhao --version
```

## Your first run

From your dbt project's root:

```bash
dbt compile
zhao check --against main   # or master, or whatever your default branch is called
```

That's it. zhao finds the merge-base between your current branch and `main`, checks it out
into a temporary worktree, compiles *that* with `dbt` to get a Baseline, and diffs it against
your current compiled state. If nothing breaking changed, it exits `0` and prints a short
summary. If something did, you'll see a report like this:

```
Changed:
  model model.jaffle_shop.stg_customers:
    ~ column type changed: customer_id (bigint -> int)
    + column added: marketing_source
    - column removed: last_name
  model model.jaffle_shop.dim_customers:
    - column removed: last_name

Downstream impact:
  model model.jaffle_shop.stg_customers:
    [WARN] customer_id type narrowed from bigint to int (column-type-narrowed)
  model model.jaffle_shop.dim_customers:
    [BREAKING] last_name removed from model model.jaffle_shop.stg_customers breaks reference via last_name (column-removed-with-active-references)

Summary: 2 model(s) changed, 3 column(s) changed, 1 breaking, 1 warning

Recommended: dbt build --select stg_customers dim_customers
```

Three things happened here that a plain text diff of the SQL never gives you:

1. **Changed** — exactly what changed, per model, in plain language (not "this file's hash
   differs").
2. **Downstream impact** — exactly which models are actually reached by each change, each
   labeled `BREAKING` or `WARN`, with the specific reference that makes it so. Nothing
   downstream of the change is silently swept in; nothing unrelated is silently omitted.
3. A ready-to-copy `dbt build --select ...` command scoped to exactly the impacted models —
   so validating the fix doesn't mean rebuilding your whole project.

`zhao check` exits non-zero exactly when something `BREAKING` fired — wire it straight into
CI (see [CI integration](ci-integration.md)).

## `zhao diff`: the same engine, no gate

Want to see what changed without the pass/fail semantics (e.g. while you're still iterating
locally)? `zhao diff` runs the identical pipeline and always exits `0`:

```bash
zhao diff --against main
```

## Baselines

zhao needs two things to compare: your current compiled state, and a Baseline to compare it
against. There are two ways to give it a Baseline:

- **Git-native (the default)** — pass `--against <branch>` (default `master`); zhao finds the
  merge-base commit, checks it out into a temporary git worktree, and compiles it with `dbt`
  itself. Zero setup — this is what most projects should start with.
- **`--state <path>`** — same idea dbt's own `--state` flag uses: point zhao at an already-
  compiled `manifest.json` from somewhere else (an artifact you already publish from your
  main branch's CI runs, for instance). Skips the git/compile step entirely.

## Next steps

- [Command reference](commands.md) — every flag, for `check`, `diff`, and `lineage`.
- [Configuring `zhao.yml`](configuration.md) — set your team's severity policy so it's
  versioned in the repo, not hidden in a CI script.
- [Understanding lineage](lineage-html.md) — explore your project's column-level lineage
  interactively, in the browser.
- [CI integration](ci-integration.md) — wiring `zhao check` into GitHub Actions.
