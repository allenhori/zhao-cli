# Releasing

How `zhao` gets built and published. For using a release, see the
[README](README.md#install); this page is for maintainers.

## Two channels

| | Stable | Nightly |
|---|---|---|
| Trigger | Pushing a `v*` tag (e.g. `v0.1.1`) | Daily schedule (07:00 UTC) + manual dispatch |
| Built from | The exact tagged commit | `master`'s current tip at run time |
| Tag | Permanent, one per version | `nightly` -- a single moving tag, force-updated every run |
| GitHub Release | Permanent, marked "Latest" | Marked **prerelease**, updated in place |
| Who gets it by default | `curl .../install.sh \| sh` | Only with `ZHAO_VERSION=nightly` set explicitly |

Both are produced by the identical workflow (`.github/workflows/release.yml`) and the identical
cross-compiled binary matrix (macOS Intel/Apple Silicon, Linux x86_64, Windows x86_64) -- the
only thing that differs is which tag/release the result attaches to.

**Nothing publishes automatically on a merged PR.** Merging to `master` only changes what the
*next* nightly run happens to pick up; it never triggers a build by itself.

## Why this design (not a `nightly` branch)

A dedicated `nightly-xxxxx` branch is not the standard pattern for this and was deliberately not
used. Branches are for *development* lines (feature branches; for a mature, multi-version
project, maintenance branches like `release/1.x` for backporting fixes to old stable
versions -- not a concern `zhao` has pre-1.0). What projects with a nightly *channel*
(rustup, Deno, Bun, Neovim, ...) actually use is a moving tag plus a GitHub Release explicitly
flagged `prerelease: true` -- exactly what's built here.

That flag is what actually keeps the two channels separated safely: GitHub excludes prereleases
from a repo's `/releases/latest` API endpoint, which is exactly what
`github.com/allenhori/zhao-cli/releases/latest/download/...` (what the install script hits by
default) resolves against. Nobody can land on nightly by accident -- they have to explicitly set
`ZHAO_VERSION=nightly` first. A branch wouldn't add any protection beyond what this already
guarantees, and would add upkeep (something has to keep it synced to `master`) for no extra
safety.

## Cutting a stable release

1. Make sure `master` is green (`ci.yml`'s Test/Clippy/Format/Docs checks).
2. Bump `version` in the workspace root `Cargo.toml`'s `[workspace.package]` if it hasn't already
   been bumped to the version you're about to tag.
3. Tag and push:
   ```bash
   git tag -a v0.1.1 -m "zhao v0.1.1"
   git push origin v0.1.1
   ```
4. `release.yml` builds all four targets and publishes a GitHub Release at that tag, with
   auto-generated release notes (every merged PR since the last tag). Check
   [the Actions run](https://github.com/allenhori/zhao-cli/actions/workflows/release.yml) and
   the [resulting release](https://github.com/allenhori/zhao-cli/releases) once it finishes.

There's currently no automated gate stopping a `v*` tag from being pushed on a commit that
hasn't actually passed CI -- see [Planned: branch protection](#planned-branch-protection-not-yet-configured)
below. Until that's in place, cutting a release is a manual judgment call: only tag a commit
you've confirmed is green.

## Nightly

Runs unattended (schedule) or can be forced manually:

```bash
gh workflow run release.yml --ref master
```

No action needed otherwise -- every day's build simply overwrites the previous nightly release
and moves the `nightly` tag to `master`'s current tip.

## Planned: branch protection (not yet configured)

Tracked as a low-priority roadmap item, not yet acted on: GitHub branch protection on `master`
(required `ci.yml` status checks before merge, required PR review, no direct pushes). Since a
`v*` tag is normally cut from `master`'s tip, gating entry into `master` this way means every
commit that could ever become a tagged release was already vetted before it landed -- without
needing any check inside `release.yml` itself. Deliberately not enabled yet while the repo isn't
open to outside contributions.
