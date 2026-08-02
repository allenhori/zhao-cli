//! Entry point for the `zhao` command-line tool.
//!
//! This crate is intentionally a thin shell: argument parsing, output
//! formatting, and process exit codes live here, while all actual analysis
//! is delegated to `zhao-core`. See `ARCHITECTURE.md` at the repository
//! root for the intended command surface as it's implemented.

fn main() {
    println!("zhao {} (pre-implementation)", zhao_core::version());
}
