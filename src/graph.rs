//! Neighborhood graph extraction and rendering.
//!
//! `raft graph <name>` builds the subgraph within N hops of a starting node -
//! the local graph view, like Obsidian's - and renders it as Graphviz DOT or
//! Mermaid so you can actually *see* the shape around a project or entity.
//! The full index is far too dense to render whole; a bounded neighborhood is
//! the useful unit.

use crate::extract;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// A node in an extracted subgraph.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: i64,
    pub kind: String,
    pub name: String,
    /// Hops from the start node (0 = the start node itself).
    pub distance: usize,
}

/// A directed edge in an extracted subgraph.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub src: i64,
    pub dst: i64,
    pub relation: String,
    pub weight: f64,
}

/// A bounded neighborhood around a start node.
pub struct Subgraph {
    pub start: i64,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// The output format for `raft graph`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFormat {
    Dot,
    Mermaid,
}

impl GraphFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "dot" | "graphviz" => Some(GraphFormat::Dot),
            "mermaid" | "mmd" => Some(GraphFormat::Mermaid),
            _ => None,
        }
    }
}

/// Resolve a node by name (case/punctuation-insensitive), across the kinds
/// that make sense as a graph starting point.
fn resolve_start(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let canonical = extract::canonicalize(name);
    let mut stmt = conn.prepare(
        "SELECT id, name FROM nodes
         WHERE kind IN ('project', 'entity', 'note')
         ORDER BY CASE kind
             WHEN 'project' THEN 0 WHEN 'entity' THEN 1 ELSE 2 END, id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let stored: String = row.get(1)?;
        if extract::canonicalize(&stored) == canonical {
            return Ok(Some(row.get(0)?));
        }
    }
    Ok(None)
}

/// Build the subgraph within `depth` hops of `name`, treating edges as
/// undirected for traversal (so we reach both what points at the node and
/// what it points to) while preserving direction on the edges themselves.
/// `min_weight` drops low-confidence edges before traversal, so weak
/// backticked-span guesses don't bloat the picture.
pub fn neighborhood(
    conn: &Connection,
    name: &str,
    depth: usize,
    min_weight: f64,
) -> Result<Option<Subgraph>> {
    let Some(start) = resolve_start(conn, name)? else {
        return Ok(None);
    };

    // BFS outward, undirected, up to `depth`.
    let mut distance: HashMap<i64, usize> = HashMap::new();
    distance.insert(start, 0);
    let mut frontier = vec![start];

    for hop in 1..=depth {
        let mut next = Vec::new();
        for &node in &frontier {
            let mut stmt = conn.prepare(
                "SELECT CASE WHEN src = ?1 THEN dst ELSE src END AS other
                 FROM edges
                 WHERE (src = ?1 OR dst = ?1) AND weight >= ?2",
            )?;
            let neighbors = stmt.query_map(rusqlite::params![node, min_weight], |row| {
                row.get::<_, i64>(0)
            })?;
            for other in neighbors {
                let other = other?;
                if let std::collections::hash_map::Entry::Vacant(e) = distance.entry(other) {
                    e.insert(hop);
                    next.push(other);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    let in_set: HashSet<i64> = distance.keys().copied().collect();

    // Fetch node metadata for everything in the neighborhood.
    let mut nodes = Vec::new();
    for (&id, &dist) in &distance {
        let (kind, name): (String, String) =
            conn.query_row("SELECT kind, name FROM nodes WHERE id = ?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        nodes.push(GraphNode {
            id,
            kind,
            name,
            distance: dist,
        });
    }
    nodes.sort_by_key(|n| (n.distance, n.id));

    // Every edge whose endpoints are both in the neighborhood.
    let mut edges = Vec::new();
    let mut stmt = conn.prepare("SELECT src, dst, kind, weight FROM edges WHERE weight >= ?1")?;
    let rows = stmt.query_map([min_weight], |row| {
        Ok(GraphEdge {
            src: row.get(0)?,
            dst: row.get(1)?,
            relation: row.get(2)?,
            weight: row.get(3)?,
        })
    })?;
    for edge in rows {
        let edge = edge?;
        if in_set.contains(&edge.src) && in_set.contains(&edge.dst) {
            edges.push(edge);
        }
    }

    Ok(Some(Subgraph {
        start,
        nodes,
        edges,
    }))
}

/// A short, render-safe label for a node. Note names are file paths, so show
/// just the file name; other kinds use their name verbatim.
fn label(node: &GraphNode) -> String {
    if node.kind == "note" {
        std::path::Path::new(&node.name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&node.name)
            .to_string()
    } else {
        node.name.clone()
    }
}

/// Render the subgraph as Graphviz DOT.
pub fn to_dot(sub: &Subgraph) -> String {
    let mut out = String::from("digraph raft {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  node [style=filled, fontname=\"monospace\"];\n");

    for node in &sub.nodes {
        let fill = match node.kind.as_str() {
            "project" => "#a6e3a1",
            "entity" => "#89b4fa",
            "note" => "#f9e2af",
            "loop" => "#f38ba8",
            _ => "#cdd6f4",
        };
        let shape = if node.id == sub.start {
            "doublecircle"
        } else {
            "box"
        };
        out.push_str(&format!(
            "  n{} [label=\"{}\", fillcolor=\"{}\", shape={}];\n",
            node.id,
            escape_dot(&label(node)),
            fill,
            shape,
        ));
    }

    for edge in &sub.edges {
        // Heavier (higher-confidence) edges draw thicker.
        let pen = (edge.weight.clamp(0.3, 6.0) / 2.0).max(0.5);
        out.push_str(&format!(
            "  n{} -> n{} [label=\"{}\", penwidth={:.1}];\n",
            edge.src,
            edge.dst,
            escape_dot(&edge.relation),
            pen,
        ));
    }

    out.push_str("}\n");
    out
}

/// Render the subgraph as a Mermaid flowchart.
pub fn to_mermaid(sub: &Subgraph) -> String {
    let mut out = String::from("graph LR\n");
    for node in &sub.nodes {
        // Mermaid node: shape by kind, start node double-bordered.
        let l = escape_mermaid(&label(node));
        if node.id == sub.start {
            out.push_str(&format!("  n{}(((\"{}\")))\n", node.id, l));
        } else if node.kind == "note" {
            out.push_str(&format!("  n{}[\"{}\"]\n", node.id, l));
        } else if node.kind == "loop" {
            out.push_str(&format!("  n{}>\"{}\"]\n", node.id, l));
        } else {
            out.push_str(&format!("  n{}(\"{}\")\n", node.id, l));
        }
    }
    for edge in &sub.edges {
        out.push_str(&format!(
            "  n{} -->|{}| n{}\n",
            edge.src,
            escape_mermaid(&edge.relation),
            edge.dst,
        ));
    }
    out
}

fn escape_dot(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_mermaid(s: &str) -> String {
    // Mermaid labels are quoted; escape quotes and strip characters that
    // break its parser.
    s.replace('"', "&quot;").replace(['[', ']', '{', '}'], "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::open_in_memory;
    use rusqlite::params;

    fn node(conn: &Connection, kind: &str, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES (?1, ?2, NULL, '{}')",
            params![kind, name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn edge(conn: &Connection, src: i64, dst: i64, rel: &str, weight: f64) {
        conn.execute(
            "INSERT INTO edges (src, dst, kind, provenance, weight) VALUES (?1, ?2, ?3, 'indexer', ?4)",
            params![src, dst, rel, weight],
        )
        .unwrap();
    }

    #[test]
    fn format_parse() {
        assert_eq!(GraphFormat::parse("dot"), Some(GraphFormat::Dot));
        assert_eq!(GraphFormat::parse("MERMAID"), Some(GraphFormat::Mermaid));
        assert_eq!(GraphFormat::parse("svg"), None);
    }

    #[test]
    fn neighborhood_respects_depth() {
        let conn = open_in_memory().unwrap();
        let a = node(&conn, "project", "alpha");
        let b = node(&conn, "entity", "beta");
        let c = node(&conn, "entity", "gamma");
        let d = node(&conn, "entity", "delta");
        edge(&conn, a, b, "mentions", 1.0); // depth 1
        edge(&conn, b, c, "mentions", 1.0); // depth 2
        edge(&conn, c, d, "mentions", 1.0); // depth 3

        let sub = neighborhood(&conn, "alpha", 1, 0.0).unwrap().unwrap();
        let ids: HashSet<i64> = sub.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, HashSet::from([a, b]));

        let sub2 = neighborhood(&conn, "alpha", 2, 0.0).unwrap().unwrap();
        assert_eq!(sub2.nodes.len(), 3); // a, b, c
    }

    #[test]
    fn neighborhood_filters_by_weight() {
        let conn = open_in_memory().unwrap();
        let a = node(&conn, "project", "alpha");
        let strong = node(&conn, "entity", "strong");
        let weak = node(&conn, "entity", "weak");
        edge(&conn, a, strong, "mentions", 2.0);
        edge(&conn, a, weak, "mentions", 0.3);

        let sub = neighborhood(&conn, "alpha", 1, 0.5).unwrap().unwrap();
        let names: HashSet<&str> = sub.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains("strong"));
        assert!(!names.contains("weak"));
    }

    #[test]
    fn unknown_start_is_none() {
        let conn = open_in_memory().unwrap();
        assert!(neighborhood(&conn, "nope", 2, 0.0).unwrap().is_none());
    }

    #[test]
    fn dot_and_mermaid_render_start_and_edges() {
        let conn = open_in_memory().unwrap();
        let a = node(&conn, "project", "alpha");
        let b = node(&conn, "entity", "beta");
        edge(&conn, a, b, "mentions", 1.0);
        let sub = neighborhood(&conn, "alpha", 1, 0.0).unwrap().unwrap();

        let dot = to_dot(&sub);
        assert!(dot.starts_with("digraph raft {"));
        assert!(dot.contains("doublecircle")); // start node
        assert!(dot.contains("-> n")); // an edge
        assert!(dot.contains("alpha") && dot.contains("beta"));

        let mmd = to_mermaid(&sub);
        assert!(mmd.starts_with("graph LR"));
        assert!(mmd.contains("-->"));
        assert!(mmd.contains("alpha") && mmd.contains("beta"));
    }
}
