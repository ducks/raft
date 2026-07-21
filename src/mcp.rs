//! MCP server over the graph, built on mcp-stdio.
//!
//! This module only describes raft's tools and runs them; the mcp-stdio
//! crate owns the stdio JSON-RPC transport and dispatch. stdout is
//! protocol; the startup line goes to stderr.

use anyhow::Result;
use serde_json::{json, Value};

use mcp_stdio::{serve as serve_stdio, Server, Tool};

use crate::{config, index, query};

pub fn serve() -> Result<()> {
    eprintln!(
        "raft mcp server on stdio (db: {})",
        config::db_path()?.display()
    );
    serve_stdio(&RaftServer);
    Ok(())
}

struct RaftServer;

impl Server for RaftServer {
    fn name(&self) -> &str {
        "raft"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn tools(&self) -> Vec<Tool> {
        vec![
            tool("search", "Full-text search across all indexed notes. Returns matching notes with dates and snippets, newest first.", json!({
                "type": "object",
                "properties": {
                    "term": { "type": "string", "description": "Text to search for" },
                    "limit": { "type": "integer", "default": 20 }
                },
                "required": ["term"]
            })),
            tool("about", "Everything the graph knows about a project or entity: git state (branch, recent commits), notes that mention it, its definition if it's a code symbol, and what it co-occurs with. Case-insensitive.", json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Project or entity name" }
                },
                "required": ["name"]
            })),
            tool("why", "Why a project or entity is in the graph: every edge pointing at it with provenance (human wiki link vs indexer heuristic), a confidence weight, and the rationale that created it. Use to audit whether a connection is trustworthy. Backticked-span mentions score 0.3; set min_weight to filter weak edges.", json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Project or entity name" },
                    "min_weight": { "type": "number", "default": 0.0, "description": "Hide edges below this confidence weight" }
                },
                "required": ["name"]
            })),
            tool("dangling", "Open loops (follow-up items, unchecked boxes) across all notes, stalest first. Use to find forgotten work.", json!({
                "type": "object",
                "properties": {
                    "about": { "type": "string", "description": "Only loops mentioning this project or entity" },
                    "limit": { "type": "integer", "default": 50 }
                }
            })),
            tool("connect", "Connections nobody wrote down: pairs of projects/entities that keep co-occurring across notes over time (affinity-scored), and projects whose commits land on the same days.", json!({
                "type": "object",
                "properties": {
                    "min": { "type": "integer", "default": 3, "description": "Minimum shared notes / shared commit days" },
                    "min_weight": { "type": "number", "default": 0.5, "description": "Hide edges below this confidence weight before pairing; the default excludes weak backticked-span guesses (weight 0.3). Use 0 to include everything." },
                    "limit": { "type": "integer", "default": 15 }
                }
            })),
            tool("reindex", "Rescan all configured sources and rebuild the graph. Call after creating or editing notes so queries see the changes.", json!({ "type": "object", "properties": {} })),
        ]
    }

    fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        call_tool(name, args).map_err(|e| e.to_string())
    }
}

fn tool(name: &str, description: &str, input_schema: Value) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

fn call_tool(name: &str, arguments: &Value) -> Result<String> {
    let arg_str = |key: &str| {
        arguments
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let arg_int = |key: &str, default: i64| {
        arguments
            .get(key)
            .and_then(|v| v.as_i64())
            .unwrap_or(default)
    };

    match name {
        "search" => {
            let term = arg_str("term").ok_or_else(|| anyhow::anyhow!("missing 'term'"))?;
            let conn = index::open_db()?;
            let hits = query::search(&conn, &term, arg_int("limit", 20) as usize)?;
            Ok(serde_json::to_string_pretty(&hits)?)
        }
        "about" => {
            let name = arg_str("name").ok_or_else(|| anyhow::anyhow!("missing 'name'"))?;
            let conn = index::open_db()?;
            match query::about(&conn, &name)? {
                Some(about) => Ok(serde_json::to_string_pretty(&about)?),
                None => Ok(format!("nothing known about '{name}'")),
            }
        }
        "why" => {
            let name = arg_str("name").ok_or_else(|| anyhow::anyhow!("missing 'name'"))?;
            let min_weight = arguments
                .get("min_weight")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let conn = index::open_db()?;
            match query::why(&conn, &name, min_weight)? {
                Some(facts) => Ok(serde_json::to_string_pretty(&facts)?),
                None => Ok(format!("nothing known about '{name}'")),
            }
        }
        "dangling" => {
            let conn = index::open_db()?;
            let loops = query::dangling(
                &conn,
                arg_str("about").as_deref(),
                arg_int("limit", 50) as usize,
            )?;
            Ok(serde_json::to_string_pretty(&loops)?)
        }
        "connect" => {
            let conn = index::open_db()?;
            let min_weight = arguments
                .get("min_weight")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5);
            let connections = query::connect(
                &conn,
                arg_int("min", 3),
                min_weight,
                arg_int("limit", 15) as usize,
            )?;
            Ok(serde_json::to_string_pretty(&connections)?)
        }
        "reindex" => {
            let cfg = config::Config::load()?;
            let stats = index::rebuild(&cfg)?;
            Ok(format!(
                "indexed {} notes, {} projects, {} entities, {} loops, {} edges \
                 (git: {} refreshed, {} reused from cache)",
                stats.notes,
                stats.projects,
                stats.entities,
                stats.loops,
                stats.edges,
                stats.git_refreshed,
                stats.git_cached
            ))
        }
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    }
}
