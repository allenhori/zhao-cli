//! `zhao-hello`: the minimal possible zhao-cli Addon. See this crate's
//! `README.md` for the full walkthrough -- this file is deliberately
//! small and does nothing "real": it reads zhao's own
//! `target/zhao/full_lineage.json`, counts what's in it, and writes a
//! trivial result to its own fixed output path. That's the whole
//! contract a real Addon (like `zhao-dbt-plan`,
//! <https://github.com/allenhori/zhao-dbt-plan>) builds on top of.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The subset of `target/zhao/full_lineage.json`'s shape this example
/// actually reads -- just enough to prove the input contract, not a
/// complete model of the file. See `lineage_html.rs`'s `GraphNode`/
/// `GraphEdge` in the main `zhao-cli` crate for the authoritative shape.
#[derive(Debug, Deserialize)]
struct FullLineage {
    nodes: Vec<LineageNode>,
    edges: Vec<LineageEdge>,
}

#[derive(Debug, Deserialize)]
struct LineageNode {
    id: String,
    name: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct LineageEdge {
    upstream: String,
    downstream: String,
}

/// `zhao-hello`'s own trivial output -- deliberately not shaped like a
/// real Addon's output (e.g. `zhao-dbt-plan`'s plan JSON has a real
/// schema for real consumers). This exists purely to demonstrate "write
/// your own output to your own fixed path," not to be useful itself.
#[derive(Debug, Serialize)]
struct HelloPlan {
    greeting: String,
    node_count: usize,
    edge_count: usize,
    /// How many models/sources each node has downstream of it --
    /// exactly the kind of thing "walk the edges" makes easy, and
    /// exactly the kind of thing a real Addon (again, see
    /// `zhao-dbt-plan`) does far more of.
    downstream_counts: HashMap<String, usize>,
}

fn main() {
    // No clap dependency -- deliberately minimal. A real Addon should
    // use whatever CLI parsing fits its own needs; this one flag is all
    // this example needs to demonstrate.
    let project_dir = parse_project_dir(std::env::args().skip(1));

    let input_path = project_dir
        .join("target")
        .join("zhao")
        .join("full_lineage.json");
    let lineage = match read_full_lineage(&input_path) {
        Ok(lineage) => lineage,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    };

    let mut downstream_counts: HashMap<String, usize> = HashMap::new();
    for edge in &lineage.edges {
        *downstream_counts.entry(edge.upstream.clone()).or_insert(0) += 1;
    }
    // `edge.downstream` also participates in the graph even with zero
    // of its own downstream edges -- recorded as 0 rather than omitted,
    // so every node that appears anywhere in an edge shows up in the
    // output, not just the ones with outgoing edges.
    for edge in &lineage.edges {
        downstream_counts
            .entry(edge.downstream.clone())
            .or_insert(0);
    }

    let plan = HelloPlan {
        greeting: "hello from zhao-hello -- a real Addon would do something useful here"
            .to_string(),
        node_count: lineage.nodes.len(),
        edge_count: lineage.edges.len(),
        downstream_counts,
    };

    let output_path = project_dir
        .join("target")
        .join("zhao")
        .join("hello_plan.json");
    if let Err(message) = write_output(&output_path, &plan) {
        eprintln!("error: {message}");
        std::process::exit(1);
    }

    println!(
        "zhao-hello: read {} node(s), {} edge(s) from {} -- wrote {}",
        lineage.nodes.len(),
        lineage.edges.len(),
        input_path.display(),
        output_path.display()
    );

    // One line per node, naming what kind it is -- proves the input was
    // actually walked, not just counted.
    for node in &lineage.nodes {
        println!("  {} ({}): {}", node.name, node.kind, node.id);
    }
}

/// `--project-dir <path>`, defaulting to `.` -- the one flag this
/// example needs. Any other argument is ignored; a real Addon should
/// reject unrecognized arguments instead (this one doesn't, to stay
/// minimal).
fn parse_project_dir(mut args: impl Iterator<Item = String>) -> PathBuf {
    while let Some(arg) = args.next() {
        if arg == "--project-dir"
            && let Some(value) = args.next()
        {
            return PathBuf::from(value);
        }
    }
    PathBuf::from(".")
}

/// Reads and parses `target/zhao/full_lineage.json` -- zhao-cli's own
/// input contract for an Addon (see README.md): `zhao lineage` always
/// writes this file, unconditionally, to this exact path, regardless of
/// what else it was asked to do. An Addon never needs to run `zhao`
/// itself to get it -- just read the file, same as this function does.
fn read_full_lineage(path: &Path) -> Result<FullLineage, String> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        format!(
            "{}: {err} -- run `zhao lineage` in this project first (it always writes this file)",
            path.display()
        )
    })?;
    serde_json::from_str(&contents).map_err(|err| format!("{}: {err}", path.display()))
}

/// Writes `plan` to `path` -- zhao-cli's own output contract for an
/// Addon: write to your own fixed, predictable path under
/// `target/zhao/`, so anything downstream (a human, a script, `zhao-cli`
/// itself once dispatch finds you) knows exactly where to look without
/// needing to ask you.
fn write_output(path: &Path, plan: &HelloPlan) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    let json =
        serde_json::to_string_pretty(plan).map_err(|err| format!("could not serialize: {err}"))?;
    std::fs::write(path, json).map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flag_defaults_to_current_directory() {
        let args: Vec<String> = vec![];
        assert_eq!(parse_project_dir(args.into_iter()), PathBuf::from("."));
    }

    #[test]
    fn project_dir_flag_is_used_when_given() {
        let args = vec!["--project-dir".to_string(), "/some/path".to_string()];
        assert_eq!(
            parse_project_dir(args.into_iter()),
            PathBuf::from("/some/path")
        );
    }
}
