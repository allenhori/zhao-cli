# Set up zhao with an AI coding agent

This page is written **for a coding agent** (Claude Code, Cursor, Copilot, Codex, or anything
similar) that has been asked to wire `zhao` into a dbt project — locally, in CI, or both. It is
also readable by a human.

**To use it,** tell your agent:

> Read https://raw.githubusercontent.com/allenhori/zhao-cli/master/docs/ai-setup.md and set up
> zhao for this repository.

The agent should follow the procedure below in order, ask you when something can't be
determined from the repo, and show you every change before making it.

---

## Instructions for the agent

You are helping a user adopt `zhao`, a change-review and CI gate for dbt projects. Your job is to
produce a working setup, not to explain zhao. Work through the steps in order.

### Ground rules

- **Do not invent flags or `zhao.yml` keys.** The source of truth is
  [commands.md](commands.md) and [configuration.md](configuration.md), and `zhao --help` /
  `zhao check --help` print the flags from the installed binary. If something you want isn't
  documented there, it doesn't exist; say so and pick another route.
- **Never write credentials, tokens, or profile secrets into any file.** Reference the CI's own
  secret store by name and let the user populate it.
- **Show a diff and get a yes before writing or committing anything.** Do not push, open PRs, or
  change CI settings outside the repository unless the user asks.
- **Tell the user before running an install command** (`curl ... | sh`, `pip install`, `cargo
  install`) and what it does.
- zhao never connects to the warehouse or holds credentials. It reads dbt's compiled
  `manifest.json` and shells out to the user's own `dbt`. Anything that needs a warehouse
  connection is dbt's job, using whatever profile the user already has.

### Step 1 — Establish the goal

Ask the user, unless they already said:

1. **Where** should zhao run: local development only, CI only, or both? (Recommend local first
   even if they want CI; it is the fastest way to catch a wrong `dbt-command`.)
2. **What should happen in CI** when zhao finds something: fail the PR on a breaking change
   (`zhao check`, the default), or only report (`zhao diff`, always exits 0)?
3. **Do they also want `zhao-dbt-plan`** (a dbt microbatch window planner)? Most projects don't.
   If yes, finish this guide first, then follow
   [zhao-dbt-plan's setup guide](https://raw.githubusercontent.com/allenhori/zhao-dbt-plan/master/docs/ai-setup.md).

### Step 2 — Discover how dbt is invoked

Read the repo; do not ask what you can find. Look for:

| Look at | What it tells you |
|---|---|
| `pyproject.toml`, `uv.lock`, `poetry.lock`, `Pipfile`, `requirements*.txt` | dbt is a Python dependency. `uv.lock` → `uv run dbt`; `poetry.lock` → `poetry run dbt`; `Pipfile` → `pipenv run dbt`. |
| `.venv/`, `venv/`, `.python-version`, `.tool-versions`, `.mise.toml` | A virtualenv or version manager. A bare `dbt` on `PATH` may not be the right one. |
| `Makefile`, `justfile`, `Taskfile.yml`, `scripts/` | A wrapper the team already uses. If `make dbt-build` calls `dbt build --target ci`, that target/vars set matters below. |
| `.github/workflows/`, `.buildkite/`, `Jenkinsfile`, `.gitlab-ci.yml`, `.circleci/`, `azure-pipelines.yml` | The CI system, how it installs dbt, and which `--target` / `--vars` / `--profiles-dir` it uses. **Copy these exactly.** |
| `dbt_project.yml`, `profiles.yml`, `packages.yml` / `dependencies.yml` | Project root, whether profiles live in the repo, whether `dbt deps` is needed. |
| `dbt --version` (run it) | dbt Core (1.x) versus dbt Fusion (2.x). zhao reads either. Also confirms which `dbt` actually resolves. |
| A `zhao.yml` at the repo root or in the dbt project directory | An existing config. Extend it; don't replace it. |

Also determine:

- **Where the dbt project lives.** If `dbt_project.yml` is not at the repo root, this is a
  subdirectory or a monorepo; zhao is pointed at it with `--project-dir`.
- **The default branch name** (`main`, `master`, `develop`, ...).
- **Whether dbt Cloud is the only way the team runs dbt** (no local `dbt` available). If so, zhao's
  git-native Baseline can't compile; use `--state` with a published manifest instead (see
  [Step 5](#step-5--decide-how-the-baseline-is-obtained)).

Then **ask the user only for what remains ambiguous**, one question at a time, with your best
guess as the default. Typical open questions: which `--target` should CI use for compilation, and
whether a wrapper is required.

### Step 3 — Write `zhao.yml`

Put as much as possible in `zhao.yml` and as little as possible in CI YAML. The CI file then stays
short and nearly identical across CI systems, and local runs behave the same as CI. Keys are
documented in [configuration.md](configuration.md); the ones that matter for wiring:

```yaml
# Which dbt to run for every dbt call zhao makes itself (deps/compile for the Baseline).
# Shell-word-split, so a multi-word prefix works. Match what you found in Step 2:
dbt-command: dbt              # or: uv run dbt | poetry run dbt | ./.venv/bin/dbt | <team wrapper>

# Extra arguments appended AFTER the dbt subcommand on those same calls.
dbt-args: "--target ci"       # only if CI compiles with a non-default target/vars

# Default branch used to find the merge-base for the Baseline.
against: main

# Severity of the gate. default | strict | lenient. Omit for default.
preset: default

# Optional: makes zhao print a ready-to-run "rebuild only what's impacted" command.
# Set to whichever dbt subcommand the team actually uses: build | run | test.
recommended-command:
  subcommand: build
```

Rules of thumb:

- **`dbt-command` / `dbt-args` must reproduce what the project's own CI does.** zhao compiles the
  *merge-base* commit with these, and diffs it against the *current* compiled manifest. If the
  two sides are compiled with different targets or vars, the diff reports changes nobody made.
  Whatever `--target` / `--vars` the pipeline's own `dbt compile` step uses, `dbt-args` must
  match.
- **`dbt-args` only affects zhao's own internal dbt calls.** It does not change the separate
  `dbt compile` step that produces the *current* manifest (see Step 6); that step is written in
  the CI file, so keep the two in agreement by hand.
- **If local and CI invoke dbt differently** (say `uv run dbt` locally and a plain `dbt` on the
  CI image), put the local value in `zhao.yml` and override in CI with the `--dbt-command` flag,
  which takes precedence over the file. Do not commit a CI-only value that breaks local runs.
- **Monorepo / several dbt projects.** A `zhao.yml` at the repo root (the nearest ancestor with
  `.git`) is the shared default; each dbt project's own `zhao.yml` overrides only the keys it
  sets. Put `dbt-command`, `against`, and `preset` at the root; put anything project-specific in
  the project's file. Run zhao once per project with `--project-dir`.
- **Leave `defer:` and `tool:` out** unless the user asks. `tool:` is only a fallback; zhao
  auto-detects dbt from `dbt_project.yml`. `defer:` only adds a "defer plan" section to the report.
- **Do not add keys "just in case."** An unneeded key is one more thing for the user to maintain.

Show the user the proposed `zhao.yml` and explain each line in one sentence before writing it.

### Step 4 — Install zhao and verify locally

Tell the user what you're about to run, then:

```bash
curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
zhao --version
```

Notes for the agent:

- It installs to `~/.zhao/bin`. If that isn't on `PATH`, the installer prints the line to add;
  tell the user rather than editing their shell profile without asking.
- Prebuilt binaries: macOS (Intel and Apple Silicon), Linux **x86_64 only**, and Windows (a zip on
  the [releases page](https://github.com/allenhori/zhao-cli/releases)). On Linux **arm64**
  (including Graviton/ARM CI runners) the installer stops with an error; use
  `cargo install --git https://github.com/allenhori/zhao-cli` instead.
- Homebrew (`brew install allenhori/zhao/zhao-cli`) and Scoop are also available for local machines.

Then verify with the **non-gating** command first, from the dbt project directory, on a branch
that has at least one change relative to the default branch:

```bash
<dbt-command> compile            # zhao reads target/manifest.json and refuses a stale one
zhao diff --against <default-branch>
```

Interpret the result for the user:

| Outcome | Meaning | What to do |
|---|---|---|
| Report with a `Changed:` section | Working. | Continue. |
| "Nothing changed" on a branch you *know* has model edits | Wrong branch, or `--against` doesn't resolve to the ref you think. | Check `git merge-base HEAD <ref>`. |
| Exit code `2` mentioning `dbt` | zhao couldn't run dbt. | Fix `dbt-command` (Step 3); run that exact command by hand. |
| Exit code `2`, stale manifest | `target/manifest.json` older than the project's dbt source files. | Re-run `dbt compile`. Do **not** reach for `--allow-stale-manifest`. |
| Exit code `2`, merge-base not found | The base ref isn't available locally. | `git fetch origin <default-branch>`; use `origin/<branch>`. |
| Changes reported that the user didn't make | Baseline and current compiled with different targets/vars. | Align `dbt-args` with the compile step (Step 3). |

Exit codes: `0` nothing breaking, `1` a breaking finding fired (`check` only), `2` zhao couldn't
run. **Do not proceed to CI until `zhao diff` works locally.** Also ask the user to try the
**gate**, `zhao check`, once to see the exit code.

If the user wanted local-only, stop here after confirming `recommended-command` (if set) prints a
command that actually runs.

### Step 5 — Decide how the Baseline is obtained

zhao needs the *prior* state to diff against. Two ways; pick with the user:

- **Git-native (default, zero setup).** `zhao check --against origin/<default-branch>` finds the
  merge-base, checks it out into a temporary worktree, and compiles it with `dbt`. Requires that,
  in the CI job: (a) full git history, (b) `dbt` invokable with working packages and profile at
  that older commit, and usually (c) the same warehouse credentials `dbt compile` already needs.
  Best for most projects.
- **`--state <manifest.json>`.** Skip git and compile entirely by pointing at a compiled manifest
  the default branch's own CI publishes (artifact store, bucket, release asset). Best when
  compiling the merge-base is slow, or when dbt can't run in the PR job. See
  [ci-integration.md](ci-integration.md#publishing-a---state-artifact-instead).

Default to git-native unless the user says compiling is slow or the job can't run dbt.

### Step 6 — Wire CI

Every CI system needs the same six things, in this order. Write them in the CI's own syntax by
reading the existing pipeline files from Step 2 and matching their style (image, caching, secrets,
step naming). Do not paste a template that ignores what's already there.

1. **Checkout with full history.** Shallow clones break the merge-base. GitHub Actions:
   `fetch-depth: 0`. Buildkite/Jenkins/GitLab/CircleCI: disable shallow clone or set the fetch
   depth to full, and make sure the *base* branch is fetched (`git fetch origin <base>`); on
   Jenkins and others the checkout is often a detached HEAD with no `origin/<base>` ref.
2. **Install dbt** the way the project already does (reuse the existing step and its caching).
3. **`dbt deps`, then `dbt compile`** for the *current* commit, using the same target/vars as
   `dbt-args` in `zhao.yml`. zhao does not compile the current side itself and refuses a stale
   manifest. Warehouse credentials come from the CI's secret store, as they already do for dbt.
4. **Install zhao.** Prefer pinning for reproducible builds (`ZHAO_VERSION=v0.1.x` before the
   installer; see [releases](https://github.com/allenhori/zhao-cli/releases)). Add `~/.zhao/bin`
   to `PATH`, or set `ZHAO_INSTALL_DIR` to somewhere already on it.
5. **Run the gate:** `zhao check --against origin/<base-branch>`. Nothing else is required to fail
   the job; the non-zero exit code is the gate. Add `--project-dir <dir>` if the dbt project isn't
   at the repo root, and `--dbt-command "<cmd>"` only if CI's dbt differs from `zhao.yml`.
6. **Optional: act on the result.** `zhao check --format json` (or `zhao diff --format json`)
   gives structured output; `impacted_models` is the exact set of models to rebuild. If
   `recommended-command` is set, the text report already prints the command. Only add this once
   the gate itself works.

**The base-branch name differs per CI system.** Use the variable the CI provides rather than
hard-coding `main`. Check each against the CI's own documentation, since they can change:

| CI | Pull/merge request base branch |
|---|---|
| GitHub Actions | `${{ github.base_ref }}` (only set on `pull_request` events) |
| GitLab CI | `$CI_MERGE_REQUEST_TARGET_BRANCH_NAME` (merge request pipelines) |
| Buildkite | `$BUILDKITE_PULL_REQUEST_BASE_BRANCH` (empty on non-PR builds) |
| Jenkins (multibranch) | `$CHANGE_TARGET` (only set on change-request builds) |
| CircleCI | No built-in base-branch variable; use the default branch name, or an API/parameter |
| Azure Pipelines | `$(System.PullRequest.TargetBranch)` (may include `refs/heads/`) |

Also make the job run **only on pull requests**: on a push to the default branch the merge-base
is the commit itself and the diff is empty.

The minimal GitHub Actions shape, for reference only. Adapt it to the pipeline that's actually
there rather than pasting it in:

```yaml
- uses: actions/checkout@v7
  with: { fetch-depth: 0 }
- run: pip install dbt-core dbt-duckdb        # ← reuse the project's own install step
- run: dbt deps && dbt compile                # ← same --target/--vars as zhao.yml's dbt-args
- run: curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
- run: PATH="$HOME/.zhao/bin:$PATH" zhao check --against origin/${{ github.base_ref }}
```

For the other systems the six steps are identical; only the syntax and the base-branch variable
change.

### Step 7 — Verify in CI

Have the user open a small, throwaway PR that makes one visible model change. Confirm:

- the job ran on the PR, and the report's `Changed:` section names that change;
- the job's exit code matches expectations (`0` for a harmless change);
- for a gating setup, make a deliberately breaking change (drop a column something still reads) on
  a scratch branch and confirm the job fails with exit code `1`.

Failure triage:

| Symptom in CI | Likely cause |
|---|---|
| merge-base not found / unknown revision `origin/<base>` | Shallow clone, or base branch not fetched (step 1). |
| `dbt: command not found` / wrong version | `dbt-command` doesn't match how CI installs dbt; set `--dbt-command` in CI. |
| Baseline compile fails at the old commit | Missing packages/profile/credentials at that commit; or use `--state` (Step 5). |
| Stale manifest error | The `dbt compile` step is missing, ran before checkout, or ran in a different directory. |
| Phantom changes across the whole project | Baseline and current compiled with different `--target`/`--vars`. |
| Passes on every PR, even breaking ones | Wrong `--against` (comparing a branch to itself), or the job runs on pushes to the default branch. |

### Step 8 — Hand off

Summarise for the user, in a few lines: what files you changed, the exact CI command that now
runs, how to run the same thing locally, and how to loosen or tighten the gate (`preset:` and
per-rule `rules:` overrides in [configuration.md](configuration.md#the-rule-catalog)). Do not
commit or push unless asked.

---

## Reference

- [Getting started](getting-started.md), [Command reference](commands.md),
  [Configuring `zhao.yml`](configuration.md), [CI integration](ci-integration.md)
- Exit codes: `0` nothing breaking · `1` breaking finding (`check`) · `2` zhao couldn't run
- Files zhao writes: everything under `target/zhao/` (run metadata, logs, lineage). Nothing is
  written to the warehouse or sent anywhere.
