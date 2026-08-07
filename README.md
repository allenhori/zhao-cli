# zhao (曌)

**A free, offline, deterministic breaking-change gate for dbt.** zhao reads your dbt
project's compiled SQL and tells a reviewer exactly what a pull request changed and which
downstream models it actually reaches — before anyone has to trace the DAG by hand.

```
Changed:
  model model.jaffle_shop.stg_customers:
    - column removed: last_name

Downstream impact:
  model model.jaffle_shop.dim_customers:
    [BREAKING] last_name removed from model model.jaffle_shop.stg_customers breaks reference via last_name (column-removed-with-active-references)

Summary: 1 model(s) changed, 1 column(s) changed, 1 breaking

Recommended: dbt build --select dim_customers
```

## Why

dbt's own `state:modified` comparison is syntactic: any compiled-SQL text change counts as
"modified," and everything downstream is assumed affected. Teams end up either rebuilding
their whole downstream cone on every PR (slow CI), or leaning on a human reviewer to catch a
removed column, a narrowed type, or a loosened join by reading SQL — something nobody
reliably does across a DAG of any real size.

zhao parses the SQL itself and computes *real* column-level lineage between two states of
your project, classifies each change against a fixed Rule catalog (column removed with an
active reference, type narrowed, join loosened, column added), and reports the exact models
each change actually reaches — never the whole DAG, never a guess. The whole thing runs
fully offline: no LLM, no network call, no account, nothing installed in your warehouse
beyond what `dbt run` already needs.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
```

Downloads the right pre-built binary for your platform from the
[releases page](https://github.com/allenhori/zhao-cli/releases) — no Rust toolchain needed.
Windows: grab `zhao-x86_64-pc-windows-msvc.zip` from the same page. Rust users:
`cargo install --git https://github.com/allenhori/zhao-cli`.

Two release channels: a tagged **stable** release (`v0.1.0`, ...) for anything you depend
on, and a rolling **nightly** build off `master`, always available at the
[`nightly` tag](https://github.com/allenhori/zhao-cli/releases/tag/nightly) — set
`ZHAO_VERSION=nightly` before running the install script above to track it instead. See
[RELEASING.md](RELEASING.md) for how the two channels work and how releases are cut.

## Quickstart

```bash
cd your-dbt-project
dbt compile
zhao check --against main
```

`zhao check` finds the merge-base between your branch and `main`, compiles it with `dbt` to
get a Baseline, diffs it against your current state, and exits non-zero if anything breaking
fired. Full walkthrough: **[Getting started](docs/getting-started.md)**.

## Commands

| Command | What it does |
|---|---|
| `zhao check` | The CI gate — diffs against a Baseline, fails on a breaking change. |
| `zhao diff` | Same engine, always exits `0` — for local inspection during development. |
| `zhao lineage` | What's upstream/downstream of a model or column, right now (no diff, no git). |
| `zhao update` | Replaces the current binary with a release from GitHub Releases. The only command that reaches the network at all — and only to download the binary itself, never to send anything from your project. See [What it doesn't do](#what-it-doesnt-do). |

Full flag reference: **[docs/commands.md](docs/commands.md)**.

## Addons

`zhao <name>` falls through to a `zhao-<name>` binary on `PATH` when `<name>` isn't one of the
built-ins above — the same convention `git` uses for `git <custom-command>`. `zhao-cli` has no
compiled-in knowledge of any specific Addon; it only knows the naming convention and forwards
arguments, exit code, and output verbatim.

[`zhao-dbt-plan`](https://github.com/allenhori/zhao-dbt-plan) (a dbt microbatch cascading
time-window planner, AGPLv3, separate repo) is the first real Addon. See
[`examples/hello-zhao-addon/`](examples/hello-zhao-addon/) for a minimal reference
implementation of the Addon contract if you want to build your own — its `README.md` is a
walkthrough of the whole discovery/input/output contract.

## See it, don't just read about it

`zhao lineage` exports an interactive, self-contained lineage graph by default — click a model
or column to trace exactly what it depends on and what depends on it, search, filter, all
offline in one HTML file.

![Clicking a model, expanding columns, then tracing a calculated column's real upstream source](docs/assets/lineage-demo.gif)

**[Open the live demo](docs/assets/lineage-demo.html)** to try it yourself. More in
**[docs/lineage-html.md](docs/lineage-html.md)**.

## Configuration

An optional `zhao.yml` at your project root lets your team set its own severity policy —
versioned in the repo, not hidden in a CI script:

```yaml
preset: strict
rules:
  column-added: pass
defer:
  target: prod
  state: artifacts/prod/manifest.json
```

Full reference, including monorepo cascading: **[docs/configuration.md](docs/configuration.md)**.

## CI integration

```yaml
- uses: actions/checkout@v7
  with: { fetch-depth: 0 }   # zhao's Baseline resolution needs full history
- run: dbt compile
- run: curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
- run: PATH="$HOME/.zhao/bin:$PATH" zhao check --against origin/${{ github.base_ref }}
```

Full example and notes: **[docs/ci-integration.md](docs/ci-integration.md)**.

## Documentation

- [Getting started](docs/getting-started.md)
- [Command reference](docs/commands.md)
- [Configuring `zhao.yml`](docs/configuration.md)
- [CI integration](docs/ci-integration.md)
- [Understanding lineage](docs/lineage-html.md)
- [Architecture](ARCHITECTURE.md) — how the code is organized, for contributors
- [Releasing](RELEASING.md) — how the stable/nightly channels work, for maintainers

## What it doesn't do

zhao never connects to, stores, or holds credentials for your warehouse or database. `zhao
check`/`zhao diff`/`zhao lineage` read only the compiled `manifest.json` dbt itself already
produced, entirely on your own machine or CI runner — no secret or token ever passes through
zhao to get there. The one place a live connection is genuinely useful (`--check-relations`,
fully optional) still doesn't change that: zhao never opens the connection itself, it hands
the check to `dbt run-operation` and borrows whatever connection your own dbt profile already
has.

zhao never reads, collects, or stores your actual data — no row values, nothing about what's
*in* your tables. It only derives structural metadata (schema, lineage, what changed), and
that metadata stays on your own filesystem, under `target/zhao/`, unless you decide otherwise.
Nothing is ever sent anywhere automatically: `zhao-cli` makes no network call of its own
except `zhao update`, which only downloads a release binary — it doesn't send anything from
your project. If you separately choose to use zhao-cloud, metadata only ever leaves your
machine when you explicitly trigger that upload yourself — a command you run manually, or one
you wrote into your own CI pipeline — never silently, and never as a side effect of
`check`/`diff`/`lineage`.

zhao also never generates or applies schema-evolution DDL for you: it detects that a change
needs manual evolution or a backfill; the decision and the mechanism stay entirely yours.

## Status

Early, real-world usable. dbt is the first supported project format, not the definition of
what zhao is — the core engine (`zhao-core`) has no dbt-specific vocabulary baked in, so a
second Transformation Tool Adapter is a matter of implementing a trait, not rewriting the
engine.

## Building from source

```bash
cargo build --workspace
cargo test --workspace
```

Requires a recent stable Rust toolchain (edition 2024). Before opening a PR, also run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for how the code is organized.

## License

[Apache 2.0](LICENSE).
