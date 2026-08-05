# CI integration

`zhao check` is designed to be a drop-in CI gate: it exits `0` when nothing breaking was
found, `1` when something was, and needs nothing beyond `dbt` itself being invokable (for
git-native Baseline resolution) or a `--state` path you already publish.

## GitHub Actions

```yaml
name: zhao

on:
  pull_request:

jobs:
  zhao-check:
    runs-on: ubuntu-latest
    steps:
      # Full history, not the default shallow clone -- zhao's git-native
      # Baseline resolution needs origin/<default-branch>'s full history
      # to compute a real merge-base against this PR's HEAD.
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0

      - uses: actions/setup-python@v7
        with:
          python-version: "3.12"

      - name: Install dbt
        run: pip install dbt-core dbt-duckdb   # swap for your own adapter

      - name: dbt compile (current state)
        run: dbt compile

      - name: Install zhao
        run: curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh

      - name: zhao check
        run: |
          export PATH="$HOME/.zhao/bin:$PATH"
          zhao check --against origin/${{ github.base_ref }}
```

That's the whole gate. A few things worth knowing:

- **`fetch-depth: 0` is required.** zhao's git-native Baseline resolution needs the target
  branch's full history to find a real merge-base; the default shallow checkout only has
  the PR's own commits.
- **`dbt compile` first.** zhao reads your *current* state from
  `<project-dir>/target/manifest.json` as-is — it doesn't run `dbt compile` itself unless
  you're using `zhao lineage --compile` (a different command).
- **The Baseline is resolved separately, inside `zhao check` itself** — it checks out the
  merge-base commit into a temporary worktree and compiles *that* with `dbt`, using
  whatever `dbt`/adapter is on `PATH` in the job. No `--state` artifact pipeline required to
  get started.
- **Exit code is the whole gate.** No extra step needed to fail the job — `zhao check`
  returning non-zero already fails this step, and GitHub Actions fails the job.

## Publishing a `--state` artifact instead

If you'd rather not have every PR job compile the merge-base commit itself (e.g. a large
project where that's slow), publish a compiled manifest from your default branch's own CI
runs somewhere fetchable (a release asset, a cloud storage bucket, an internal artifact
store), then:

```yaml
- name: Fetch baseline manifest
  run: curl -fsSL https://your-artifact-store/latest/manifest.json -o baseline.json

- name: zhao check
  run: zhao check --state baseline.json
```

This skips git/compile entirely for the Baseline side — same semantics as dbt's own
`--state` flag.

## A note on `--format json`

For a custom PR comment bot or dashboard instead of (or alongside) the terminal report,
`--format json` gives you the identical data — changes, findings, the defer plan, schema
evolution notes — as structured JSON. See [`zhao check`'s flag reference](commands.md#zhao-check)
and pipe it into whatever renders your comment.
