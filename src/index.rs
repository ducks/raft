use crate::config::{Config, SourceKind};
use crate::extract;
use crate::scan;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,             -- note | project | entity
    name TEXT NOT NULL,
    path TEXT,
    meta TEXT,                      -- JSON blob, kind-specific
    UNIQUE(kind, name)
);

CREATE TABLE IF NOT EXISTS notes (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id),
    body TEXT NOT NULL,
    note_date TEXT                  -- YYYY-MM-DD for daily notes
);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY,
    src INTEGER NOT NULL REFERENCES nodes(id),
    dst INTEGER NOT NULL REFERENCES nodes(id),
    kind TEXT NOT NULL,             -- mentions | wikilink
    provenance TEXT NOT NULL,       -- human | indexer | agent
    weight REAL NOT NULL DEFAULT 1.0,
    rationale TEXT,
    UNIQUE(src, dst, kind, provenance)
);

CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst);
"#;

pub fn open_db() -> Result<Connection> {
    let path = crate::config::db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)
        .with_context(|| format!("could not open database at {}", path.display()))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub struct IndexStats {
    pub notes: usize,
    pub projects: usize,
    pub entities: usize,
    pub edges: usize,
}

/// Full rebuild: scan every source, replace the index.
pub fn rebuild(config: &Config) -> Result<IndexStats> {
    let mut conn = open_db()?;

    // Scan projects first so notes can be matched against the dictionary.
    let mut projects = Vec::new();
    let mut all_notes = Vec::new();

    for source in &config.sources {
        let root = crate::config::expand_tilde(&source.path);
        match source.kind {
            SourceKind::Projects => projects.extend(scan::scan_projects(&root)?),
            SourceKind::Notes => all_notes.extend(scan::scan_notes(&root)?),
        }
    }

    let project_names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();

    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM edges; DELETE FROM notes; DELETE FROM nodes;")?;

    for project in &projects {
        let meta = project
            .git_meta
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "{}".to_string());
        // Same directory name can exist under multiple sources (tmp, notes,
        // scratch dirs); first source wins for v0.
        tx.execute(
            "INSERT OR IGNORE INTO nodes (kind, name, path, meta) VALUES ('project', ?1, ?2, ?3)",
            params![project.name, project.path.to_string_lossy(), meta],
        )?;
    }

    let mut entity_count = 0usize;
    let mut edge_count = 0usize;

    for note in &all_notes {
        let name = note.path.to_string_lossy().to_string();
        tx.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('note', ?1, ?1, '{}')",
            params![name],
        )?;
        let note_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO notes (node_id, body, note_date) VALUES (?1, ?2, ?3)",
            params![note_id, note.body, note.note_date],
        )?;

        let extraction = extract::extract(&note.body, &project_names);

        for (project, count) in &extraction.project_mentions {
            let dst: i64 = tx.query_row(
                "SELECT id FROM nodes WHERE kind = 'project' AND name = ?1",
                params![project],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance, weight)
                 VALUES (?1, ?2, 'mentions', 'indexer', ?3)",
                params![note_id, dst, *count as f64],
            )?;
            edge_count += 1;
        }

        for target in &extraction.wiki_links {
            let dst = upsert_entity(&tx, target)?;
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                 VALUES (?1, ?2, 'wikilink', 'human')",
                params![note_id, dst],
            )?;
            edge_count += 1;
        }

        for span in &extraction.code_spans {
            // Only spans that recur across the corpus become entities;
            // for v0 store them all and let queries filter by degree.
            if span.len() < 3 || project_names.contains(span) {
                continue;
            }
            let dst = upsert_entity(&tx, span)?;
            entity_count += 1;
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                 VALUES (?1, ?2, 'mentions', 'indexer')",
                params![note_id, dst],
            )?;
            edge_count += 1;
        }
    }

    tx.commit()?;

    Ok(IndexStats {
        notes: all_notes.len(),
        projects: projects.len(),
        entities: entity_count,
        edges: edge_count,
    })
}

fn upsert_entity(tx: &rusqlite::Transaction, name: &str) -> Result<i64> {
    tx.execute(
        "INSERT OR IGNORE INTO nodes (kind, name, meta) VALUES ('entity', ?1, '{}')",
        params![name],
    )?;
    let id = tx.query_row(
        "SELECT id FROM nodes WHERE kind = 'entity' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(id)
}
