## What changed

## Why

What problem this solves, or what issue it closes (`Closes #N`). If there's a design decision
in here that isn't obvious from the code, say why you went this way and what you didn't do
instead.

## How you tested it

New tests are the expectation for anything but pure docs/config changes -- this project is
built TDD-first (see the existing test files for the house style: `#[test] fn
descriptive_sentence_name()`, real fixtures over mocks where practical). If you added a test,
say what it actually proves; if you couldn't test something (e.g. it needs a live warehouse
connection), say that plainly instead of leaving it implicit.

## Checklist

Run before opening, not after CI tells you:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] New/changed behavior has test coverage (or this PR is docs/config-only)
- [ ] Docs updated if this changes user-facing behavior (`README.md`, `docs/`, `--help` text)
