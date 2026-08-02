# zhao-cli

`zhao-cli` (曌) is the open source (Apache 2.0) CLI for Zhao — a change-review and CI engine
for data transformation projects. dbt is the first supported project format, not the definition
of what it is.

Its core job: tell a reviewer what a pull request actually changed (a column added, a
calculation rewritten, a join loosened) and which downstream models that change genuinely
reaches — so CI runs only what matters, deterministically, offline, with no external calls.

## Status

Early development. See [ARCHITECTURE.md](ARCHITECTURE.md) for how the code is organized.

## Building

```bash
cargo build --workspace
cargo test --workspace
```

Requires a recent stable Rust toolchain (edition 2024). Before opening a PR, also run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```
