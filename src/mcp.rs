//! MCP server over stdio: JSON-RPC 2.0, newline-delimited.
//! stdout is protocol; anything human goes to stderr.

use crate::{config, index, query};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    eprintln!(
        "raft mcp server on stdio (db: {})",
        config::db_path()?.display()
    );

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_message(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") },
                    }),
                )?;
                continue;
            }
        };

        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = message.get("id").cloned();

        // Notifications (no id) expect no response.
        let Some(id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => {
                let requested = message
                    .pointer("/params/protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(PROTOCOL_VERSION);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": requested,
                        "capabilities": { "tools": {} },
                        "serverInfo": {
                            "name": "raft",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    },
                })
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() },
            }),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call_tool(&name, &arguments) {
                    Ok(text) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": text }] },
                    }),
                    Err(e) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("error: {e}") }],
                            "isError": true,
                        },
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }),
        };

        write_message(&mut stdout, &response)?;
    }

    Ok(())
}

fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stdout.write_all(line.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search",
            "description": "Full-text search across all indexed notes. Returns matching notes with dates and snippets, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "term": { "type": "string", "description": "Text to search for" },
                    "limit": { "type": "integer", "default": 20 },
                },
                "required": ["term"],
            },
        },
        {
            "name": "about",
            "description": "Everything the graph knows about a project or entity: git state (branch, recent commits), notes that mention it, and what it co-occurs with. Case-insensitive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Project or entity name" },
                },
                "required": ["name"],
            },
        },
        {
            "name": "dangling",
            "description": "Open loops (follow-up items, unchecked boxes) across all notes, stalest first. Use to find forgotten work.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "about": { "type": "string", "description": "Only loops mentioning this project or entity" },
                    "limit": { "type": "integer", "default": 50 },
                },
            },
        },
        {
            "name": "connect",
            "description": "Connections nobody wrote down: pairs of projects/entities that keep co-occurring across notes over time (affinity-scored), and projects whose commits land on the same days.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min": { "type": "integer", "default": 3, "description": "Minimum shared notes / shared commit days" },
                    "limit": { "type": "integer", "default": 15 },
                },
            },
        },
        {
            "name": "reindex",
            "description": "Rescan all configured sources and rebuild the graph. Call after creating or editing notes so queries see the changes.",
            "inputSchema": { "type": "object", "properties": {} },
        },
    ])
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
            let connections =
                query::connect(&conn, arg_int("min", 3), arg_int("limit", 15) as usize)?;
            Ok(serde_json::to_string_pretty(&connections)?)
        }
        "reindex" => {
            let cfg = config::Config::load()?;
            let stats = index::rebuild(&cfg)?;
            Ok(format!(
                "indexed {} notes, {} projects, {} entities, {} loops, {} edges",
                stats.notes, stats.projects, stats.entities, stats.loops, stats.edges
            ))
        }
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    }
}
