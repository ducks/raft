use crate::config::{Config, SourceKind};
use crate::extract;
use crate::scan;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,             -- note | project | entity | loop
    name TEXT NOT NULL,
    path TEXT,
    meta TEXT,                      -- JSON blob, kind-specific
    UNIQUE(kind, name)
);

CREATE TABLE IF NOT EXISTS notes (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id),
    body TEXT NOT NULL,
    note_date TEXT,                 -- YYYY-MM-DD for daily notes
    mtime TEXT                      -- YYYY-MM-DD file mtime fallback
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

    // The index is derived data; on schema changes just start over.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS edges; DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS nodes;",
        )?;
    }
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(conn)
}

pub struct IndexStats {
    pub notes: usize,
    pub projects: usize,
    pub entities: usize,
    pub edges: usize,
    pub loops: usize,
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

    let ignore: HashSet<String> = config
        .ignore
        .iter()
        .map(|s| extract::canonicalize(s))
        .collect();

    // Ignored names never enter the matching dictionary; the project
    // node itself still exists, it just never gets auto-linked.
    let project_names: HashSet<String> = projects
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| !ignore.contains(&extract::canonicalize(n)))
        .collect();
    let project_canon: HashSet<String> = project_names
        .iter()
        .map(|n| extract::canonicalize(n))
        .collect();

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

    let mut edge_count = 0usize;
    let mut loop_count = 0usize;

    for note in &all_notes {
        let name = note.path.to_string_lossy().to_string();
        tx.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('note', ?1, ?1, '{}')",
            params![name],
        )?;
        let note_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO notes (node_id, body, note_date, mtime) VALUES (?1, ?2, ?3, ?4)",
            params![note_id, note.body, note.note_date, note.mtime_date],
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
            let Some(dst) = upsert_entity(&tx, target, &ignore)? else {
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                 VALUES (?1, ?2, 'wikilink', 'human')",
                params![note_id, dst],
            )?;
            edge_count += 1;
        }

        for span in &extraction.code_spans {
            // Spans matching a project name are already project mentions;
            // don't duplicate them as entities.
            if project_canon.contains(&extract::canonicalize(span)) {
                continue;
            }
            let Some(dst) = upsert_entity(&tx, span, &ignore)? else {
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                 VALUES (?1, ?2, 'mentions', 'indexer')",
                params![note_id, dst],
            )?;
            edge_count += 1;
        }

        for open_loop in extract::extract_loops(&note.body) {
            let meta = serde_json::json!({ "section": open_loop.section }).to_string();
            // Identical text across notes intentionally merges into one
            // loop node; multiple 'contains' edges record every sighting.
            tx.execute(
                "INSERT OR IGNORE INTO nodes (kind, name, meta) VALUES ('loop', ?1, ?2)",
                params![open_loop.text, meta],
            )?;
            let loop_id: i64 = tx.query_row(
                "SELECT id FROM nodes WHERE kind = 'loop' AND name = ?1",
                params![open_loop.text],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                 VALUES (?1, ?2, 'contains', 'indexer')",
                params![note_id, loop_id],
            )?;
            loop_count += 1;
            edge_count += 1;

            // Link the loop to the projects its text mentions so
            // `raft dangling --about <project>` can filter.
            let loop_extraction = extract::extract(&open_loop.text, &project_names);
            for project in loop_extraction.project_mentions.keys() {
                let dst: i64 = tx.query_row(
                    "SELECT id FROM nodes WHERE kind = 'project' AND name = ?1",
                    params![project],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO edges (src, dst, kind, provenance)
                     VALUES (?1, ?2, 'mentions', 'indexer')",
                    params![loop_id, dst],
                )?;
                edge_count += 1;
            }
        }
    }

    let entity_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM nodes WHERE kind = 'entity'",
        [],
        |row| row.get(0),
    )?;
    tx.commit()?;

    Ok(IndexStats {
        notes: all_notes.len(),
        projects: projects.len(),
        entities: entity_count as usize,
        edges: edge_count,
        loops: loop_count,
    })
}

/// Common words that show up backticked (table headers, YAML keys,
/// prose emphasis) but carry no identity.
const STOPWORDS: &[&str] = &[
    "what", "how", "why", "when", "where", "who", "true", "false", "nil", "null", "none", "yes",
    "no", "n/a", "tbd", "todo", "done", "new", "old", "the", "and", "for", "not",
];

/// Insert (or find) an entity under its canonical name. The first-seen
/// spelling is kept as the display form. Returns None for names that
/// canonicalize away to nothing, are too short, are stopwords, or are
/// ignored.
fn upsert_entity(
    tx: &rusqlite::Transaction,
    raw: &str,
    ignore: &HashSet<String>,
) -> Result<Option<i64>> {
    let canonical = extract::canonicalize(raw);
    if canonical.len() < 3 || ignore.contains(&canonical) || STOPWORDS.contains(&canonical.as_str())
    {
        return Ok(None);
    }
    let meta = serde_json::json!({ "display": raw.trim() }).to_string();
    tx.execute(
        "INSERT OR IGNORE INTO nodes (kind, name, meta) VALUES ('entity', ?1, ?2)",
        params![canonical, meta],
    )?;
    let id = tx.query_row(
        "SELECT id FROM nodes WHERE kind = 'entity' AND name = ?1",
        params![canonical],
        |row| row.get(0),
    )?;
    Ok(Some(id))
}
