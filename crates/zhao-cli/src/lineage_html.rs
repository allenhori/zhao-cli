//! `zhao lineage`'s HTML export (`generate`) and standalone graph JSON
//! (`graph_data_json`, written unconditionally to
//! `target/zhao/full_lineage.json` -- see issue #39, meant for other
//! tooling to consume directly). The HTML export itself is a local
//! development convenience -- not intended to run in CI.
//!
//! The whole graph (Nodes, Origins, model-level *and* column-level
//! edges) is embedded as a single JSON blob inside the page; all
//! interactivity (selecting a model/column, toggling column-level
//! detail, highlighting the upstream/downstream chain, searching) runs
//! client-side in plain JavaScript against that blob -- no server, no
//! network access, no CDN script/font/stylesheet reference anywhere in
//! the output, so the file is fully self-contained and works offline.
//!
//! Rust only computes each Node/Origin's *layer* (a longest-path
//! layering: Origins and Nodes with no upstream at layer 0, every other
//! Node one layer past its furthest upstream) -- actual pixel layout
//! (including the taller per-column row layout when column detail is
//! shown) is computed client-side in JS, since it depends on view-mode
//! state a static export can't know ahead of time. This layering still
//! applies to `graph_data_json`'s output too, even though it has no
//! page to lay out -- it's the same `GraphData` shape either way.

use std::collections::HashMap;

use serde::Serialize;
use zhao_core::adapters::AdapterVocabulary;
use zhao_core::model::{NodeId, ParsedProject, Upstream};

/// An Origin or Node not yet assigned its final within-layer order --
/// `generate`'s intermediate grouping step before it's sorted by name
/// and turned into a [`GraphNode`] with a settled position in `nodes`.
struct UngroupedEntry {
    id: String,
    name: String,
    kind: &'static str,
    columns: Vec<GraphColumn>,
}

/// A single column, carried into the export with whatever [`generate`]'s
/// caller already resolved about it -- its documented data type, and, for
/// a calculated/derived column, its rendered defining SQL (see
/// `zhao_core::model::Column::expression`). Both are `None` for a plain,
/// undocumented, or passthrough column; the client-side panel only shows
/// what's actually present.
#[derive(Debug, Serialize)]
struct GraphColumn {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expression: Option<String>,
}

/// A single Node or Origin -- position is computed client-side (see the
/// module doc comment), so this only ever carries the DAG-structural
/// `layer`, not pixel coordinates.
#[derive(Debug, Serialize)]
struct GraphNode {
    id: String,
    name: String,
    /// `"node"` or `"origin"` -- the CSS/JS-facing discriminant; the
    /// human-facing label still goes through `vocabulary`.
    kind: &'static str,
    layer: u32,
    columns: Vec<GraphColumn>,
}

/// A single Lineage Edge, reshaped for the embedded JSON -- both
/// model-level (`column` fields absent) and column-level (present) in
/// the same list, so the client-side JS can do both kinds of
/// highlighting from one dataset.
#[derive(Debug, Serialize)]
struct GraphEdge {
    upstream: String,
    downstream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_column: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downstream_column: Option<String>,
}

/// Everything embedded into the page as `window.ZHAO_LINEAGE_DATA`.
#[derive(Debug, Serialize)]
struct GraphData {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    node_term: String,
    origin_term: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_column: Option<String>,
}

/// Builds the whole project's [`GraphData`] -- every Node/Origin/edge,
/// laid out by [`compute_layers`] -- with no target scoping at all
/// (`initial_target`/`initial_column` both `None`). Shared by both
/// [`generate`] (which overlays a target on top, if one was given) and
/// [`graph_data_json`] (the standalone `full_lineage.json` export, which
/// is always the whole, unscoped graph -- see issue #39).
fn build_graph_data(project: &ParsedProject, vocabulary: &dyn AdapterVocabulary) -> GraphData {
    let layers = compute_layers(project);

    // Grouped by layer purely to get a stable, deterministic ordering
    // within each layer (sorted by display name) -- actual row/column
    // assignment happens client-side.
    let mut by_layer: HashMap<u32, Vec<UngroupedEntry>> = HashMap::new();
    for origin in &project.origins {
        by_layer.entry(0).or_default().push(UngroupedEntry {
            id: origin.id.to_string(),
            name: origin.name.clone(),
            kind: "origin",
            columns: Vec::new(),
        });
    }
    for node in &project.nodes {
        let layer = layers.get(&node.id).copied().unwrap_or(0);
        by_layer.entry(layer).or_default().push(UngroupedEntry {
            id: node.id.to_string(),
            name: node.name.clone(),
            kind: "node",
            columns: node
                .columns
                .iter()
                .map(|c| GraphColumn {
                    name: c.name.to_string(),
                    data_type: c.data_type.clone(),
                    expression: c.expression.clone(),
                })
                .collect(),
        });
    }

    let mut nodes = Vec::new();
    let mut layer_keys: Vec<u32> = by_layer.keys().copied().collect();
    layer_keys.sort_unstable();
    for layer in layer_keys {
        let mut entries = by_layer.remove(&layer).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            nodes.push(GraphNode {
                id: entry.id,
                name: entry.name,
                kind: entry.kind,
                layer,
                columns: entry.columns,
            });
        }
    }

    let edges = project
        .edges
        .iter()
        .map(|edge| GraphEdge {
            upstream: match &edge.upstream {
                Upstream::Node(id) => id.to_string(),
                Upstream::Origin(id) => id.to_string(),
            },
            downstream: edge.downstream.to_string(),
            upstream_column: edge.column.as_ref().map(|c| c.upstream_column.to_string()),
            downstream_column: edge
                .column
                .as_ref()
                .map(|c| c.downstream_column.to_string()),
        })
        .collect();

    GraphData {
        nodes,
        edges,
        node_term: vocabulary.node_term().to_string(),
        origin_term: vocabulary.origin_term().to_string(),
        initial_target: None,
        initial_column: None,
    }
}

/// Generates the full HTML document. `initial_target`/`initial_column`
/// (already-validated, full `NodeId` and bare column name strings) scope
/// the page's initial view -- both `None` embeds the whole project with
/// nothing pre-selected.
pub fn generate(
    project: &ParsedProject,
    vocabulary: &dyn AdapterVocabulary,
    initial_target: Option<NodeId>,
    initial_column: Option<String>,
) -> String {
    let mut data = build_graph_data(project, vocabulary);
    data.initial_target = initial_target.map(|id| id.to_string());
    data.initial_column = initial_column;

    let json = serde_json::to_string(&data).expect("graph data should always serialize");

    render_html(&json, vocabulary.node_term(), vocabulary.origin_term())
}

/// Serializes the whole project's lineage graph -- every Node, Origin,
/// and model-/column-level edge, with no target scoping -- to a JSON
/// string, for `zhao lineage`'s standalone `target/zhao/full_lineage.json`
/// (issue #39). Not the same JSON embedded in an HTML export (that one's
/// shaped for the page's own JS, and can carry `initial_target`/
/// `initial_column`); this is a genuinely separate, always-whole-project
/// artifact meant to be read directly.
pub fn graph_data_json(project: &ParsedProject, vocabulary: &dyn AdapterVocabulary) -> String {
    let data = build_graph_data(project, vocabulary);
    serde_json::to_string(&data).expect("graph data should always serialize")
}

/// Longest-path layering: an Origin is always layer 0 (has no upstream
/// of its own); a Node with no upstream edges at all is also layer 0
/// (it's a root, same visual column as the Origins); every other Node is
/// one layer past the *furthest* of its direct upstreams (Origins count
/// as layer 0 for this purpose) -- a simple, iterative fixed-point
/// relaxation over the edge list (bounded by the Node count, safe on any
/// DAG) rather than a real topological sort, since this only needs to be
/// "good enough to read," not optimal.
fn compute_layers(project: &ParsedProject) -> HashMap<NodeId, u32> {
    let mut layers: HashMap<NodeId, u32> =
        project.nodes.iter().map(|n| (n.id.clone(), 0)).collect();

    for _ in 0..project.nodes.len().max(1) {
        let mut changed = false;
        for edge in &project.edges {
            let upstream_layer = match &edge.upstream {
                Upstream::Node(id) => layers.get(id).copied().unwrap_or(0),
                Upstream::Origin(_) => 0,
            };
            let candidate = upstream_layer + 1;
            if let Some(current) = layers.get_mut(&edge.downstream) {
                if candidate > *current {
                    *current = candidate;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    layers
}

/// Wraps `graph_data_json` (already-serialized `GraphData`) in the full
/// HTML/CSS/JS document. The only other variable content is the
/// toolbar's hint text, which names the adapter's own vocabulary terms
/// (`node_term`/`origin_term`) -- every other byte of markup/style/script
/// is a plain string literal, easy to audit for "no external references
/// anywhere."
fn render_html(graph_data_json: &str, node_term: &str, origin_term: &str) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>zhao lineage</title>
<style>
{CSS}
</style>
</head>
<body>
<div class="viz-root">
  <header id="toolbar">
    <div class="brand">zhao <span class="brand-sub">lineage</span></div>
    <div class="search-wrap">
      <svg class="search-icon" viewBox="0 0 16 16" width="14" height="14"><path d="M11.2 9.8h-.6l-.2-.2c.8-.9 1.3-2.1 1.3-3.4C11.7 3.3 9.4 1 6.6 1S1.5 3.3 1.5 6.1s2.3 5.1 5.1 5.1c1.3 0 2.5-.5 3.4-1.3l.2.2v.6l3.4 3.4 1-1-3.4-3.3zM6.6 9.8c-2 0-3.7-1.7-3.7-3.7S4.6 2.4 6.6 2.4s3.7 1.7 3.7 3.7-1.7 3.7-3.7 3.7z" fill="currentColor"/></svg>
      <input id="search" type="text" placeholder="Search {node_term}s, {origin_term}s, or model.column…" autocomplete="off">
    </div>
    <label class="toggle">
      <input id="show-columns" type="checkbox">
      <span class="toggle-track"><span class="toggle-thumb"></span></span>
      <span class="toggle-label">Columns</span>
    </label>
    <div class="legend">
      <span class="legend-item"><span class="legend-dot origin"></span>{origin_term}</span>
      <span class="legend-item"><span class="legend-dot node"></span>{node_term}</span>
    </div>
  </header>
  <div id="scope-banner">
    <span id="scope-banner-text"></span>
    <button id="scope-expand-btn" class="mini-btn" type="button">Show whole project</button>
  </div>
  <div id="main">
    <div id="graph-scroll"><svg id="graph" xmlns="http://www.w3.org/2000/svg"></svg></div>
    <aside id="panel">
      <div id="panel-empty">Select a {node_term} or {origin_term} to inspect its lineage.</div>
      <div id="panel-content">
        <div class="panel-kind" id="panel-kind"></div>
        <h2 id="panel-title"></h2>
        <div class="panel-section-head">
          <div class="panel-section-label">Columns</div>
          <button id="panel-columns-sort" class="mini-btn" type="button" title="Toggle column order">Order</button>
        </div>
        <input id="panel-columns-search" type="text" placeholder="Filter columns…" autocomplete="off">
        <ul id="panel-columns"></ul>
        <div id="panel-summary"></div>
      </div>
    </aside>
  </div>
</div>
<script>
window.ZHAO_LINEAGE_DATA = {graph_data_json};
{JS}
</script>
</body>
</html>
"##,
        CSS = CSS,
        JS = JS,
        graph_data_json = graph_data_json,
        node_term = node_term,
        origin_term = origin_term,
    )
}

/// Validated categorical/chrome roles from the palette this tool follows
/// for every generated visualization -- slot 1 (blue) for models, slot 2
/// (orange) for sources/origins, plus the standard light/dark chrome and
/// ink roles. See the `dataviz` design skill this file was built
/// against for the full rationale.
const CSS: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }

.viz-root {
  --surface-1:      #fcfcfb;
  --surface-2:      #f9f9f7;
  --text-primary:   #0b0b0b;
  --text-secondary: #52514e;
  --text-muted:     #898781;
  --gridline:       #e1e0d9;
  --border:         rgba(11,11,11,0.10);
  --series-node:    #2a78d6;
  --series-node-soft: #2a78d61a;
  --series-origin:  #eb6834;
  --series-origin-soft: #eb68341a;
  --column-highlight: #e87ba4;
  color-scheme: light;
}
@media (prefers-color-scheme: dark) {
  .viz-root {
    --surface-1:      #1a1a19;
    --surface-2:      #0d0d0d;
    --text-primary:   #ffffff;
    --text-secondary: #c3c2b7;
    --text-muted:     #898781;
    --gridline:       #2c2c2a;
    --border:         rgba(255,255,255,0.10);
    --series-node:    #3987e5;
    --series-node-soft: #3987e526;
    --series-origin:  #d95926;
    --series-origin-soft: #d9592626;
    --column-highlight: #d55181;
    color-scheme: dark;
  }
}

body {
  margin: 0; font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
  background: radial-gradient(120% 100% at 0% 0%, var(--series-node-soft), transparent 55%),
              radial-gradient(120% 100% at 100% 100%, var(--series-origin-soft), transparent 55%),
              var(--surface-2);
}
.viz-root { display: flex; flex-direction: column; height: 100vh; color: var(--text-primary); }

#toolbar {
  display: flex; align-items: center; gap: 20px;
  padding: 12px 20px; background: var(--surface-1); border-bottom: 1px solid var(--border);
}
.brand { font-weight: 600; font-size: 15px; letter-spacing: -0.01em; white-space: nowrap; }
.brand-sub { font-weight: 400; color: var(--text-muted); }

.search-wrap { position: relative; flex: 1; max-width: 360px; }
.search-icon { position: absolute; left: 10px; top: 50%; transform: translateY(-50%); color: var(--text-muted); pointer-events: none; }
#search {
  width: 100%; padding: 7px 12px 7px 30px; font-size: 13px; border-radius: 8px;
  border: 1px solid var(--border); background: var(--surface-2); color: var(--text-primary);
  outline: none; transition: border-color 0.15s ease, box-shadow 0.15s ease;
}
#search:focus { border-color: var(--series-node); box-shadow: 0 0 0 3px var(--series-node-soft); }

.toggle { display: flex; align-items: center; gap: 8px; cursor: pointer; user-select: none; font-size: 13px; color: var(--text-secondary); }
.toggle input { display: none; }
.toggle-track { width: 32px; height: 18px; border-radius: 999px; background: var(--gridline); position: relative; transition: background 0.15s ease; flex-shrink: 0; }
.toggle-thumb { position: absolute; top: 2px; left: 2px; width: 14px; height: 14px; border-radius: 50%; background: var(--surface-1); box-shadow: 0 1px 2px var(--border); transition: transform 0.15s ease; }
.toggle input:checked + .toggle-track { background: var(--series-node); }
.toggle input:checked + .toggle-track .toggle-thumb { transform: translateX(14px); }

.legend { display: flex; gap: 14px; margin-left: auto; }
.legend-item { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--text-secondary); white-space: nowrap; }
.legend-dot { width: 8px; height: 8px; border-radius: 50%; }
.legend-dot.node { background: var(--series-node); }
.legend-dot.origin { background: var(--series-origin); }

/* Hidden by default -- shown only when the initial view is scoped to a
   target's related subgraph (issue #40); JS toggles `display` directly,
   same convention `#panel-empty`/`#panel-content` already use. */
#scope-banner {
  display: none; align-items: center; gap: 12px;
  padding: 8px 20px; background: var(--series-node-soft); border-bottom: 1px solid var(--border);
  font-size: 13px; color: var(--text-secondary);
}
#scope-banner strong { color: var(--text-primary); }

#main { flex: 1; display: flex; overflow: hidden; }
#graph-scroll { flex: 1; overflow: auto; }
#graph { display: block; }

#panel {
  width: 300px; flex-shrink: 0; border-left: 1px solid var(--border); background: var(--surface-1);
  padding: 20px; overflow: auto;
}
#panel-empty { color: var(--text-muted); font-size: 13px; line-height: 1.5; }
#panel-content { display: none; }
.panel-kind { font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text-muted); margin-bottom: 2px; }
#panel-title { font-size: 15px; font-weight: 600; margin: 0 0 16px; word-break: break-word; letter-spacing: -0.01em; }
.panel-section-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px; }
.panel-section-label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text-muted); }
.mini-btn {
  font-size: 10.5px; font-weight: 600; letter-spacing: 0.02em; color: var(--text-secondary);
  background: var(--surface-2); border: 1px solid var(--border); border-radius: 5px;
  padding: 2px 7px; cursor: pointer; transition: color 0.12s ease, border-color 0.12s ease;
}
.mini-btn:hover { color: var(--series-node); border-color: var(--series-node); }
#panel-columns-search {
  width: 100%; padding: 5px 9px; margin-bottom: 6px; font-size: 12px; border-radius: 6px;
  border: 1px solid var(--border); background: var(--surface-2); color: var(--text-primary); outline: none;
}
#panel-columns-search:focus { border-color: var(--series-node); }
#panel-columns {
  list-style: none; margin: 0 0 16px; padding: 0; max-height: 220px; min-height: 48px;
  overflow-y: auto; resize: vertical; border: 1px solid transparent; border-radius: 6px;
}
#panel-columns li {
  padding: 6px 10px; margin: 2px 0; border-radius: 6px; cursor: pointer; font-size: 12.5px;
  transition: background 0.12s ease;
}
#panel-columns li:hover { background: var(--surface-2); }
#panel-columns li.column-selected { background: var(--series-node-soft); color: var(--series-node); font-weight: 600; }
#panel-columns .col-name { font-family: ui-monospace, "SF Mono", Menlo, monospace; }
#panel-columns .col-meta { display: block; font-size: 10.5px; color: var(--text-muted); margin-top: 1px; }
#panel-columns .col-expr { display: block; font-size: 10.5px; color: var(--text-muted); margin-top: 1px; font-family: ui-monospace, "SF Mono", Menlo, monospace; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
#panel-summary { font-size: 12.5px; line-height: 1.7; color: var(--text-secondary); }
#panel-summary .summary-head { display: flex; align-items: center; justify-content: space-between; margin-top: 10px; }
#panel-summary .summary-head:first-child { margin-top: 0; }
#panel-summary strong { color: var(--text-primary); font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; font-weight: 600; }
#panel-summary .unresolved { color: var(--text-muted); font-style: italic; }
#panel-summary .col-entry { font-family: ui-monospace, "SF Mono", Menlo, monospace; }

.node-box { cursor: pointer; transition: opacity 0.15s ease; }
.node-box .card {
  fill: var(--surface-1); stroke: var(--border); stroke-width: 1;
  filter: drop-shadow(0 1px 2px rgba(11,11,11,0.06));
  transition: stroke 0.15s ease, stroke-width 0.15s ease;
}
.node-box.origin .card-accent { fill: var(--series-origin); }
.node-box.node .card-accent { fill: var(--series-node); }
.node-box.selected .card { stroke: var(--series-node); stroke-width: 2; filter: drop-shadow(0 2px 10px var(--series-node-soft)); }
.node-box.selected.origin .card { stroke: var(--series-origin); filter: drop-shadow(0 2px 10px var(--series-origin-soft)); }
.node-box.highlighted .card { stroke: var(--series-node); stroke-width: 1.5; }
.node-box.highlighted.origin .card { stroke: var(--series-origin); }
.node-box.search-match .card { stroke: var(--series-node); stroke-width: 1.5; filter: drop-shadow(0 2px 8px var(--series-node-soft)); }
.node-box.dimmed { opacity: 0.28; }
.node-box .title { font-size: 12.5px; font-weight: 600; fill: var(--text-primary); pointer-events: none; letter-spacing: -0.005em; }
.node-box .col-label { font-size: 11.5px; fill: var(--text-secondary); pointer-events: none; font-family: ui-monospace, "SF Mono", Menlo, monospace; }
.node-box .col-row { fill: transparent; }
.node-box .col-row:hover { fill: var(--surface-2); }
.node-box .col-row.col-active { fill: var(--series-node-soft); }
.node-box .col-row.col-active .col-label { fill: var(--series-node); font-weight: 600; }
.node-box .col-divider { stroke: var(--gridline); stroke-width: 1; }

.edge { stroke: var(--gridline); stroke-width: 1.5; fill: none; transition: stroke 0.15s ease, stroke-width 0.15s ease, opacity 0.15s ease; }
.edge.highlighted { stroke: var(--series-node); stroke-width: 2.25; stroke-dasharray: 7 5; animation: flow 0.85s linear infinite; }
.edge.column-highlighted { stroke: var(--column-highlight); stroke-width: 2.75; stroke-dasharray: 7 5; animation: flow 0.6s linear infinite; }
.edge.dimmed { opacity: 0.18; }
@keyframes flow { to { stroke-dashoffset: -24; } }
@media (prefers-reduced-motion: reduce) {
  .edge.highlighted, .edge.column-highlighted { animation: none; }
}
"#;

const JS: &str = r#"
(function () {
  const data = window.ZHAO_LINEAGE_DATA;
  const svg = document.getElementById("graph");
  const byId = new Map(data.nodes.map((n) => [n.id, n]));

  const upstreamOf = new Map();
  const downstreamOf = new Map();
  for (const e of data.edges) {
    if (!upstreamOf.has(e.downstream)) upstreamOf.set(e.downstream, []);
    upstreamOf.get(e.downstream).push(e);
    if (!downstreamOf.has(e.upstream)) downstreamOf.set(e.upstream, []);
    downstreamOf.get(e.upstream).push(e);
  }

  const NODE_W = 208;
  const HEADER_H = 40;
  const COL_ROW_H = 24;
  const LAYER_SPACING = 280;
  const ROW_GAP = 24;
  const MARGIN = 32;
  const SVG_NS = "http://www.w3.org/2000/svg";

  let showColumns = false;
  let selectedId = null;
  let selectedColumn = null;
  let layout = null; // Map<id, {x, y, w, h}>
  // `null` means "whole project"; otherwise a Set of node/origin ids
  // that `computeLayout`/`render` should treat as the only visible
  // graph -- the initial-view scoping from issue #40. Set once at
  // startup (to a target's related subgraph, if one was given) and
  // cleared for good by `expandToWholeProject`; nothing else narrows it
  // again for the rest of the page's life.
  let visibleIds = null;
  // Column list order in the side panel: "source" (the model's final
  // `SELECT` order, the default), "az", or "za" -- cycled by the sort
  // button. The graph itself always renders columns in source order
  // regardless of this, since row position there also drives edge
  // endpoints (see `columnRowY`).
  let panelColumnOrder = "source";
  // Upstream/downstream lists in the summary sort alphabetically by
  // name; this flips between ascending and descending.
  let summaryDescending = false;
  // The column-lineage result last passed to `renderPanel`, kept around
  // so the columns-search/sort and summary-sort controls can re-render
  // the panel without re-running the BFS.
  let currentColumnResult = null;

  function el(tag, attrs, ns) {
    const e = document.createElementNS(ns || SVG_NS, tag);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    return e;
  }

  function nodeHeight(n) {
    if (!showColumns || n.columns.length === 0) return HEADER_H;
    return HEADER_H + n.columns.length * COL_ROW_H + 8;
  }

  function computeLayout() {
    const byLayer = new Map();
    for (const n of data.nodes) {
      if (visibleIds !== null && !visibleIds.has(n.id)) continue;
      if (!byLayer.has(n.layer)) byLayer.set(n.layer, []);
      byLayer.get(n.layer).push(n);
    }
    const pos = new Map();
    let maxX = 0, maxY = 0;
    for (const [layer, members] of byLayer) {
      let y = MARGIN;
      const x = MARGIN + layer * LAYER_SPACING;
      for (const n of members) {
        const h = nodeHeight(n);
        pos.set(n.id, { x, y, w: NODE_W, h });
        y += h + ROW_GAP;
        maxY = Math.max(maxY, y);
      }
      maxX = Math.max(maxX, x + NODE_W);
    }
    return { pos, maxX: maxX + MARGIN, maxY: maxY + MARGIN };
  }

  function columnRowY(id, column) {
    const p = layout.pos.get(id);
    const n = byId.get(id);
    if (!p || !n) return null;
    const idx = n.columns.findIndex((c) => c.name === column);
    if (idx === -1) return null;
    return p.y + HEADER_H + idx * COL_ROW_H + COL_ROW_H / 2 + 4;
  }

  function edgeEndpoints(e) {
    const from = layout.pos.get(e.upstream);
    const to = layout.pos.get(e.downstream);
    if (!from || !to) return null;
    let y1 = from.y + HEADER_H / 2;
    let y2 = to.y + HEADER_H / 2;
    if (showColumns && e.upstream_column) {
      const cy = columnRowY(e.upstream, e.upstream_column);
      if (cy !== null) y1 = cy;
    }
    if (showColumns && e.downstream_column) {
      const cy = columnRowY(e.downstream, e.downstream_column);
      if (cy !== null) y2 = cy;
    }
    return { x1: from.x + from.w, y1, x2: to.x, y2 };
  }

  function edgePath(pts) {
    const dx = Math.max(40, (pts.x2 - pts.x1) * 0.5);
    return `M ${pts.x1} ${pts.y1} C ${pts.x1 + dx} ${pts.y1}, ${pts.x2 - dx} ${pts.y2}, ${pts.x2} ${pts.y2}`;
  }

  const edgeEls = [];
  const nodeEls = new Map();

  function render() {
    layout = computeLayout();
    svg.innerHTML = "";
    svg.setAttribute("width", layout.maxX);
    svg.setAttribute("height", layout.maxY);
    svg.setAttribute("viewBox", `0 0 ${layout.maxX} ${layout.maxY}`);

    edgeEls.length = 0;
    for (const e of data.edges) {
      const pts = edgeEndpoints(e);
      if (!pts) continue;
      const path = el("path", { class: "edge", d: edgePath(pts) });
      path.dataset.upstream = e.upstream;
      path.dataset.downstream = e.downstream;
      if (e.upstream_column) path.dataset.upstreamColumn = e.upstream_column;
      if (e.downstream_column) path.dataset.downstreamColumn = e.downstream_column;
      svg.appendChild(path);
      edgeEls.push(path);
    }

    nodeEls.clear();
    for (const n of data.nodes) {
      const p = layout.pos.get(n.id);
      if (!p) continue; // scoped out of the current view -- see `visibleIds`
      const g = el("g", { class: `node-box ${n.kind}` });
      g.dataset.id = n.id;

      g.appendChild(el("rect", { class: "card-accent", x: p.x, y: p.y, width: 3, height: p.h, rx: 1.5 }));
      g.appendChild(el("rect", {
        class: "card", x: p.x, y: p.y, width: p.w, height: p.h, rx: 10,
      }));

      const title = el("text", { class: "title", x: p.x + 14, y: p.y + HEADER_H / 2 + 4 });
      title.textContent = n.name.length > 26 ? n.name.slice(0, 25) + "…" : n.name;
      g.appendChild(title);

      const tooltip = el("title", {});
      tooltip.textContent = `${n.kind === "origin" ? data.origin_term : data.node_term} ${n.id}`;
      g.appendChild(tooltip);

      if (showColumns && n.columns.length > 0) {
        g.appendChild(el("line", {
          class: "col-divider", x1: p.x, y1: p.y + HEADER_H, x2: p.x + p.w, y2: p.y + HEADER_H,
        }));
        n.columns.forEach((col, i) => {
          const rowY = p.y + HEADER_H + i * COL_ROW_H;
          const row = el("g", { class: "col-row-group" });
          const active = selectedId === n.id && selectedColumn === col.name;
          const rowRect = el("rect", {
            class: "col-row" + (active ? " col-active" : ""), x: p.x, y: rowY, width: p.w, height: COL_ROW_H,
          });
          rowRect.addEventListener("click", (ev) => { ev.stopPropagation(); selectNode(n.id); selectColumn(col.name); });
          row.appendChild(rowRect);
          const label = el("text", {
            class: "col-label" + (active ? " col-active" : ""), x: p.x + 14, y: rowY + COL_ROW_H / 2 + 4,
          });
          const suffix = col.expression ? " ƒ" : "";
          const budget = 24 - suffix.length;
          label.textContent = (col.name.length > budget ? col.name.slice(0, budget - 1) + "…" : col.name) + suffix;
          row.appendChild(label);
          if (col.expression || col.data_type) {
            const tip = el("title", {});
            tip.textContent = [col.data_type, col.expression].filter(Boolean).join(" — ");
            row.appendChild(tip);
          }
          g.appendChild(row);
        });
      }

      g.addEventListener("click", () => selectNode(n.id));
      svg.appendChild(g);
      nodeEls.set(n.id, g);
    }

    applyHighlight();
  }

  function bfsNodeLevel(startId) {
    const ancestors = new Set();
    const descendants = new Set();
    let frontier = [startId];
    let seen = new Set([startId]);
    while (frontier.length) {
      const next = [];
      for (const id of frontier) {
        for (const e of upstreamOf.get(id) || []) {
          if (!seen.has(e.upstream)) { seen.add(e.upstream); ancestors.add(e.upstream); next.push(e.upstream); }
        }
      }
      frontier = next;
    }
    frontier = [startId];
    seen = new Set([startId]);
    while (frontier.length) {
      const next = [];
      for (const id of frontier) {
        for (const e of downstreamOf.get(id) || []) {
          if (!seen.has(e.downstream)) { seen.add(e.downstream); descendants.add(e.downstream); next.push(e.downstream); }
        }
      }
      frontier = next;
    }
    return { ancestors, descendants };
  }

  // `id`'s full transitive closure (itself plus every ancestor and
  // descendant) -- the same set `bfsNodeLevel` computes for highlighting,
  // reused as the *visible* set for the initial scoped view (issue #40).
  function relatedIds(id) {
    const { ancestors, descendants } = bfsNodeLevel(id);
    return new Set([id, ...ancestors, ...descendants]);
  }

  function updateScopeBanner() {
    const banner = document.getElementById("scope-banner");
    if (visibleIds === null) {
      banner.style.display = "none";
      return;
    }
    const n = selectedId && byId.get(selectedId);
    const label = n ? `${n.kind === "origin" ? data.origin_term : data.node_term} ${n.name}` : "the selected target";
    document.getElementById("scope-banner-text").textContent =
      `Showing ${label} and its lineage only.`;
    banner.style.display = "flex";
  }

  // Clears the scope for the rest of the page's life -- there's no path
  // back to a narrower view once expanded (matches the acceptance
  // criterion: a one-way escape hatch, not a re-toggleable filter).
  function expandToWholeProject() {
    if (visibleIds === null) return;
    visibleIds = null;
    updateScopeBanner();
    render();
  }

  // Column-level BFS, mirroring zhao-core::lineage's walk_upstream_column/
  // walk_downstream_column: a node is "unresolved" for the traced column
  // when it has real connectivity (any edge, resolved-for-another-column
  // or node-level-only) but none of it names this specific column.
  function bfsColumn(startId, startColumn) {
    function walk(getEdges, otherIdOf, otherColOf, matchColOf) {
      const resolved = [];
      const unresolvedAt = new Set();
      const visited = new Set([startId + " " + startColumn]);
      let frontier = [[startId, startColumn]];
      const matchedEdges = [];
      while (frontier.length) {
        const next = [];
        for (const [id, col] of frontier) {
          let foundResolved = false, hasConnectivity = false;
          for (const e of getEdges(id) || []) {
            hasConnectivity = true;
            const matchCol = matchColOf(e);
            if (matchCol === undefined || matchCol !== col) continue;
            foundResolved = true;
            matchedEdges.push(e);
            const otherId = otherIdOf(e);
            const otherCol = otherColOf(e);
            const key = otherId + " " + otherCol;
            if (otherCol && !visited.has(key)) {
              visited.add(key);
              resolved.push({ id: otherId, column: otherCol });
              next.push([otherId, otherCol]);
            }
          }
          if (!foundResolved && hasConnectivity && !unresolvedAt.has(id)) unresolvedAt.add(id);
        }
        frontier = next;
      }
      return { resolved, unresolvedAt: [...unresolvedAt], matchedEdges };
    }
    const up = walk(
      (id) => upstreamOf.get(id),
      (e) => e.upstream, (e) => e.upstream_column, (e) => e.downstream_column
    );
    const down = walk(
      (id) => downstreamOf.get(id),
      (e) => e.downstream, (e) => e.downstream_column, (e) => e.upstream_column
    );
    return { up, down };
  }

  function applyHighlight() {
    for (const g of nodeEls.values()) g.classList.remove("selected", "highlighted", "dimmed", "search-match");
    for (const l of edgeEls) l.classList.remove("highlighted", "dimmed", "column-highlighted");

    const term = document.getElementById("search").value.trim().toLowerCase();
    if (term) {
      // A dotted term ("model.column" or a partial prefix of it) matches
      // on the model-name part against the node name and the
      // column-name part against any of its columns -- so a user with a
      // large project can jump straight to a specific column without
      // first opening its model.
      const dot = term.indexOf(".");
      const namePart = dot === -1 ? term : term.slice(0, dot);
      const colPart = dot === -1 ? null : term.slice(dot + 1);
      for (const [id, g] of nodeEls) {
        const n = byId.get(id);
        const nameHit = n.name.toLowerCase().includes(namePart);
        const colHit = colPart === null || n.columns.some((c) => c.name.toLowerCase().includes(colPart));
        if (nameHit && colHit) g.classList.add("search-match");
        else g.classList.add("dimmed");
      }
      for (const l of edgeEls) l.classList.add("dimmed");
      return;
    }

    if (!selectedId) return;
    const { ancestors, descendants } = bfsNodeLevel(selectedId);
    const related = new Set([selectedId, ...ancestors, ...descendants]);
    for (const [id, g] of nodeEls) {
      if (id === selectedId) g.classList.add("selected");
      else if (related.has(id)) g.classList.add("highlighted");
      else g.classList.add("dimmed");
    }
    for (const l of edgeEls) {
      if (related.has(l.dataset.upstream) && related.has(l.dataset.downstream)) l.classList.add("highlighted");
      else l.classList.add("dimmed");
    }

    if (selectedColumn) {
      const { up, down } = bfsColumn(selectedId, selectedColumn);
      for (const e of [...up.matchedEdges, ...down.matchedEdges]) {
        const found = edgeEls.find((x) =>
          x.dataset.upstream === e.upstream && x.dataset.downstream === e.downstream &&
          x.dataset.upstreamColumn === e.upstream_column && x.dataset.downstreamColumn === e.downstream_column);
        if (found) found.classList.add("column-highlighted");
      }
    }
  }

  function selectNode(id) {
    selectedId = id;
    selectedColumn = null;
    document.getElementById("search").value = "";
    render();
    renderPanel(id);
  }

  function selectColumn(column) {
    selectedColumn = column;
    render();
    const { up, down } = bfsColumn(selectedId, column);
    renderPanel(selectedId, { up, down, column });
  }

  function orderedColumns(n) {
    const cols = n.columns.slice();
    if (panelColumnOrder === "az") cols.sort((a, b) => a.name.localeCompare(b.name));
    else if (panelColumnOrder === "za") cols.sort((a, b) => b.name.localeCompare(a.name));
    // "source": already in the model's final SELECT order -- leave as-is.
    return cols;
  }

  function renderPanelColumns(n, columnResult) {
    const list = document.getElementById("panel-columns");
    const filter = document.getElementById("panel-columns-search").value.trim().toLowerCase();
    list.innerHTML = "";
    for (const c of orderedColumns(n)) {
      if (filter && !c.name.toLowerCase().includes(filter)) continue;
      const li = document.createElement("li");
      const nameEl = document.createElement("span");
      nameEl.className = "col-name";
      nameEl.textContent = c.name;
      li.appendChild(nameEl);
      if (c.data_type) {
        const meta = document.createElement("span");
        meta.className = "col-meta";
        meta.textContent = c.data_type;
        li.appendChild(meta);
      }
      if (c.expression) {
        const expr = document.createElement("span");
        expr.className = "col-expr";
        expr.title = c.expression;
        expr.textContent = c.expression;
        li.appendChild(expr);
      }
      if (columnResult && columnResult.column === c.name) li.classList.add("column-selected");
      li.addEventListener("click", () => selectColumn(c.name));
      list.appendChild(li);
    }
    if (!list.children.length) {
      const li = document.createElement("li");
      li.textContent = filter ? "No columns match." : "(no columns)";
      li.style.cursor = "default";
      li.style.color = "var(--text-muted)";
      list.appendChild(li);
    }
  }

  // Builds a summary section's DOM directly (rather than an HTML string)
  // and appends it to `container` -- model/column names come from the
  // parsed project (manifest/SQL), not from a trusted template, so they
  // go through `textContent`/element creation, never string-interpolated
  // into `innerHTML`.
  function renderSummarySide(container, label, side) {
    const nameOf = (id) => (byId.get(id) ? byId.get(id).name : id);
    const resolved = side.resolved.slice().sort((a, b) => {
      const cmp = (nameOf(a.id) + "." + a.column).localeCompare(nameOf(b.id) + "." + b.column);
      return summaryDescending ? -cmp : cmp;
    });
    const unresolved = side.unresolvedAt.slice().sort((a, b) => {
      const cmp = nameOf(a).localeCompare(nameOf(b));
      return summaryDescending ? -cmp : cmp;
    });

    const head = document.createElement("div");
    head.className = "summary-head";
    const strong = document.createElement("strong");
    strong.textContent = label;
    head.appendChild(strong);
    const sortBtn = document.createElement("button");
    sortBtn.className = "mini-btn summary-sort";
    sortBtn.type = "button";
    sortBtn.title = "Toggle sort order";
    sortBtn.textContent = summaryDescending ? "Z–A" : "A–Z";
    head.appendChild(sortBtn);
    container.appendChild(head);

    if (!resolved.length && !unresolved.length) {
      container.appendChild(document.createTextNode("(none)"));
      return;
    }
    for (const r of resolved) {
      const span = document.createElement("span");
      span.className = "col-entry";
      span.textContent = `${nameOf(r.id)}.${r.column}`;
      container.appendChild(span);
      container.appendChild(document.createElement("br"));
    }
    for (const u of unresolved) {
      const span = document.createElement("span");
      span.className = "unresolved";
      span.textContent = `${nameOf(u)} (unresolved)`;
      container.appendChild(span);
      container.appendChild(document.createElement("br"));
    }
  }

  function renderPanel(id, columnResult) {
    currentColumnResult = columnResult || null;
    const n = byId.get(id);
    document.getElementById("panel-empty").style.display = "none";
    document.getElementById("panel-content").style.display = "block";
    document.getElementById("panel-kind").textContent = n.kind === "origin" ? data.origin_term : data.node_term;
    document.getElementById("panel-title").textContent = n.name;
    renderPanelColumns(n, columnResult);

    const summary = document.getElementById("panel-summary");
    summary.innerHTML = "";
    if (!columnResult) return;
    const { up, down } = columnResult;
    renderSummarySide(summary, "Upstream", up);
    summary.appendChild(document.createElement("br"));
    renderSummarySide(summary, "Downstream", down);
  }

  document.getElementById("search").addEventListener("input", () => {
    // A scoped initial view only has the related subgraph rendered at
    // all -- searching for something outside it would otherwise just
    // silently find nothing, which reads as broken rather than
    // "out of scope." Typing any search term implicitly expands to the
    // whole project first, same as the explicit banner button.
    if (document.getElementById("search").value.trim()) expandToWholeProject();
    applyHighlight();
  });
  document.getElementById("scope-expand-btn").addEventListener("click", expandToWholeProject);
  document.getElementById("show-columns").addEventListener("change", (ev) => {
    showColumns = ev.target.checked;
    render();
  });
  document.getElementById("graph").addEventListener("click", (ev) => {
    if (ev.target.id === "graph") { selectedId = null; selectedColumn = null; render(); document.getElementById("panel-empty").style.display = "block"; document.getElementById("panel-content").style.display = "none"; }
  });

  document.getElementById("panel-columns-search").addEventListener("input", () => {
    if (selectedId) renderPanelColumns(byId.get(selectedId), currentColumnResult);
  });
  document.getElementById("panel-columns-sort").addEventListener("click", () => {
    panelColumnOrder = panelColumnOrder === "source" ? "az" : panelColumnOrder === "az" ? "za" : "source";
    document.getElementById("panel-columns-sort").textContent =
      panelColumnOrder === "source" ? "Order" : panelColumnOrder === "az" ? "A–Z" : "Z–A";
    if (selectedId) renderPanelColumns(byId.get(selectedId), currentColumnResult);
  });
  document.getElementById("panel-summary").addEventListener("click", (ev) => {
    if (!ev.target.classList.contains("summary-sort")) return;
    summaryDescending = !summaryDescending;
    if (selectedId && currentColumnResult) renderPanel(selectedId, currentColumnResult);
  });

  // A targeted export's initial render scopes down to just the target's
  // related subgraph (its full upstream/downstream transitive closure) --
  // the whole project's data is still embedded and `expandToWholeProject`
  // (the banner button, or typing a search term) un-scopes it without
  // regenerating the file. No target at all (a whole-project export) has
  // nothing to scope down from -- render everything, no banner. See
  // issue #40. (`selectNode` below already calls `render()` itself, so
  // there's no separate render() needed on the scoped branch.)
  if (data.initial_target) {
    visibleIds = relatedIds(data.initial_target);
    if (data.initial_column) showColumns = true;
    document.getElementById("show-columns").checked = showColumns;
    selectNode(data.initial_target);
    if (data.initial_column) selectColumn(data.initial_column);
    updateScopeBanner();
  } else {
    render();
  }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use zhao_core::adapters::dbt::DbtVocabulary;
    use zhao_core::model::{
        Column, ColumnLineage, LineageEdge, Materialization, Node, NodeId, Origin, OriginId,
        Upstream,
    };

    fn node(id: &str, columns: &[&str]) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.rsplit('.').next().unwrap_or(id).to_string(),
            columns: columns
                .iter()
                .map(|c| Column {
                    name: zhao_core::model::ColumnName::new(*c),
                    data_type: None,
                    expression: None,
                })
                .collect(),
            joins: Vec::new(),
            materialization: Materialization::Table,
        }
    }

    fn origin(id: &str) -> Origin {
        Origin {
            id: OriginId::new(id),
            name: id.rsplit('.').next().unwrap_or(id).to_string(),
        }
    }

    fn column_edge(upstream: &str, uc: &str, downstream: &str, dc: &str) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Node(NodeId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: Some(ColumnLineage {
                upstream_column: zhao_core::model::ColumnName::new(uc),
                downstream_column: zhao_core::model::ColumnName::new(dc),
            }),
        }
    }

    fn origin_edge(upstream: &str, uc: &str, downstream: &str, dc: &str) -> LineageEdge {
        LineageEdge {
            upstream: Upstream::Origin(OriginId::new(upstream)),
            downstream: NodeId::new(downstream),
            column: Some(ColumnLineage {
                upstream_column: zhao_core::model::ColumnName::new(uc),
                downstream_column: zhao_core::model::ColumnName::new(dc),
            }),
        }
    }

    fn sample_project() -> ParsedProject {
        ParsedProject {
            nodes: vec![node("model.p.a", &["x"]), node("model.p.b", &["x"])],
            origins: vec![origin("source.p.raw")],
            edges: vec![
                origin_edge("source.p.raw", "x", "model.p.a", "x"),
                column_edge("model.p.a", "x", "model.p.b", "x"),
            ],
        }
    }

    /// Acceptance criterion: the generated file is fully self-contained
    /// -- no *fetchable* `http://`/`https://` reference (a script/style/
    /// font/CDN source, or a `fetch`/`XMLHttpRequest` call) anywhere in
    /// its output. The one expected exception is the SVG element's own
    /// `xmlns="http://www.w3.org/2000/svg"` -- a fixed XML namespace
    /// identifier, not a URL ever fetched over the network, so it's
    /// explicitly allowed rather than tripping this check.
    #[test]
    fn generated_html_contains_no_fetchable_external_references() {
        let html = generate(&sample_project(), &DbtVocabulary, None, None);
        let without_svg_namespace = html.replace("http://www.w3.org/2000/svg", "");
        assert!(
            !without_svg_namespace.contains("http://")
                && !without_svg_namespace.contains("https://"),
            "generated HTML must be fully self-contained: {html}"
        );
    }

    #[test]
    fn generated_html_is_a_well_formed_document_with_embedded_data() {
        let html = generate(&sample_project(), &DbtVocabulary, None, None);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<script>"));
        assert!(html.contains("window.ZHAO_LINEAGE_DATA"));
        assert!(html.contains("\"model.p.a\""));
        assert!(html.contains("\"model.p.b\""));
        assert!(html.contains("\"source.p.raw\""));
    }

    /// Acceptance criterion: `--html out.html <model>` scopes the
    /// initial view to that target.
    #[test]
    fn initial_target_is_embedded_when_given() {
        let html = generate(
            &sample_project(),
            &DbtVocabulary,
            Some(NodeId::new("model.p.a")),
            None,
        );
        assert!(html.contains("\"initial_target\":\"model.p.a\""));
        // The embedded JS always *references* `data.initial_column`
        // (it's a fixed property name in the script, present regardless
        // of what's in the JSON), so check the JSON key specifically
        // rather than the bare substring appearing anywhere at all.
        assert!(!html.contains("\"initial_column\":"));
    }

    /// Acceptance criterion: `--html out.html <model>.<column>` scopes
    /// the initial view at the column grain too.
    #[test]
    fn initial_target_and_column_are_both_embedded_when_given() {
        let html = generate(
            &sample_project(),
            &DbtVocabulary,
            Some(NodeId::new("model.p.a")),
            Some("x".to_string()),
        );
        assert!(html.contains("\"initial_target\":\"model.p.a\""));
        assert!(html.contains("\"initial_column\":\"x\""));
    }

    /// No target given (whole-project graph): neither key should be
    /// present in the embedded JSON at all, not even as a null -- keeps
    /// "no target" and "a target that resolved to nothing" from ever
    /// looking alike in the payload. (The embedded JS's own source
    /// always references both property names regardless -- see
    /// `initial_target_is_embedded_when_given`'s comment -- so this
    /// checks the JSON key shape specifically, not a bare substring.)
    #[test]
    fn no_initial_target_omits_both_keys_entirely() {
        let html = generate(&sample_project(), &DbtVocabulary, None, None);
        assert!(!html.contains("\"initial_target\":"));
        assert!(!html.contains("\"initial_column\":"));
    }

    /// Acceptance criterion: a targeted export's markup carries the
    /// scope-banner element and its expand control, since the initial
    /// render now scopes down to the target's related subgraph. The
    /// actual scoping/expand *behavior* lives in the embedded JS and is
    /// verified in a real browser (see issue #40), not here -- this only
    /// checks the control exists in the emitted document at all.
    #[test]
    fn a_targeted_export_carries_the_scope_banner_and_expand_control() {
        let html = generate(
            &sample_project(),
            &DbtVocabulary,
            Some(NodeId::new("model.p.a")),
            None,
        );
        assert!(html.contains(r#"id="scope-banner""#));
        assert!(html.contains(r#"id="scope-expand-btn""#));
        assert!(html.contains("relatedIds"));
        assert!(html.contains("expandToWholeProject"));
    }

    /// The scope banner/expand machinery is present in every export's
    /// markup regardless of whether a target was actually given -- it's
    /// the embedded JS deciding at load time (via `data.initial_target`)
    /// whether to scope down at all, not something Rust conditionally
    /// emits into the page.
    #[test]
    fn the_whole_project_export_still_carries_the_scope_banner_markup() {
        let html = generate(&sample_project(), &DbtVocabulary, None, None);
        assert!(html.contains(r#"id="scope-banner""#));
        assert!(html.contains(r#"id="scope-expand-btn""#));
    }

    #[test]
    fn adapter_vocabulary_terms_are_embedded() {
        let html = generate(&sample_project(), &DbtVocabulary, None, None);
        assert!(html.contains("\"node_term\":\"model\""));
        assert!(html.contains("\"origin_term\":\"source\""));
        assert!(html.contains("Search models, sources, or model.column"));
    }

    /// `sample_project()`'s shape: `source.p.raw -> model.p.a -> model.p.b`.
    /// `model.p.a`'s only upstream is an Origin (which is always treated
    /// as layer 0 for this purpose), so `a` is layer 1, one past it; `b`
    /// is layer 2, one past `a`. A genuinely upstream-free root Node
    /// (added here, not part of the shared fixture) is layer 0, the same
    /// column Origins themselves occupy.
    #[test]
    fn compute_layers_places_origins_and_roots_at_zero_and_steps_downstream_by_one() {
        let mut project = sample_project();
        project.nodes.push(node("model.p.root", &[]));

        let layers = compute_layers(&project);

        assert_eq!(layers.get(&NodeId::new("model.p.root")).copied(), Some(0));
        assert_eq!(layers.get(&NodeId::new("model.p.a")).copied(), Some(1));
        assert_eq!(layers.get(&NodeId::new("model.p.b")).copied(), Some(2));
    }
}
