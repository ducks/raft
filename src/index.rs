use crate::config::{Config, SourceKind};
use crate::extract;
use crate::scan;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;

const SCHEMA_VERSION: i64 = 3;

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

-- Full-text index over note bodies. External-content: FTS5 reads the
-- body from `notes` via node_id rather than storing a second copy.
-- Kept in sync manually during rebuild (a full wipe-and-reinsert), so
-- no sync triggers are needed.
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    body,
    content='notes',
    content_rowid='node_id'
);
"#;

/// Open an in-memory database with the current schema applied. Test-only
/// helper so query tests don't touch the filesystem.
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Open the index for reading. Never mutates the schema: if the database
/// is missing or was built by an older raft (stale schema version), this
/// returns an error telling the user to reindex rather than silently
/// wiping the index and returning empty results. Only `rebuild` is allowed
/// to reset the index (see `open_db_for_rebuild`).
pub fn open_db() -> Result<Connection> {
    open_db_at(&crate::config::db_path()?)
}

/// Open the index for reading at a specific path. Never mutates the schema:
/// if the database is missing or was built by an older raft (stale schema
/// version), returns an error telling the user to reindex rather than
/// silently wiping the index and returning empty results. Only `rebuild` is
/// allowed to reset the index (see `open_db_for_rebuild_at`).
fn open_db_at(path: &std::path::Path) -> Result<Connection> {
    if !path.exists() {
        anyhow::bail!("no index at {} - run `raft index` first", path.display());
    }
    let conn = Connection::open(path)
        .with_context(|| format!("could not open database at {}", path.display()))?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        anyhow::bail!(
            "index at {} was built by a different raft version \
             (schema {version}, expected {SCHEMA_VERSION}) - run `raft index` to rebuild it",
            path.display()
        );
    }
    Ok(conn)
}

fn open_db_for_rebuild() -> Result<Connection> {
    open_db_for_rebuild_at(&crate::config::db_path()?)
}

/// Open (creating if needed) the index for a full rebuild at a specific
/// path. This is the only path allowed to drop tables: on a stale schema
/// version it resets the index, since the index is derived data that
/// `rebuild` is about to repopulate. Read commands use `open_db`, which
/// never does this.
fn open_db_for_rebuild_at(path: &std::path::Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("could not open database at {}", path.display()))?;

    // The index is derived data; on a schema change just start over.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS notes_fts;
             DROP TABLE IF EXISTS edges;
             DROP TABLE IF EXISTS notes;
             DROP TABLE IF EXISTS nodes;",
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
    let mut conn = open_db_for_rebuild()?;

    // Scan projects first so notes can be matched against the dictionary.
    let mut projects = Vec::new();
    let mut all_notes = Vec::new();

    for source in &config.sources {
        let root = crate::config::expand_tilde(&source.path);
        match source.kind {
            SourceKind::Projects => {
                projects.extend(scan::scan_projects(&root).with_context(|| {
                    format!("failed to scan configured source {}", root.display())
                })?)
            }
            SourceKind::Notes => {
                all_notes.extend(scan::scan_notes(&root).with_context(|| {
                    format!("failed to scan configured source {}", root.display())
                })?)
            }
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
    tx.execute_batch(
        "DELETE FROM notes_fts; DELETE FROM edges; DELETE FROM notes; DELETE FROM nodes;",
    )?;

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
            // Confidence scales with how many times the name appears in prose.
            let rationale = format!("matched project name '{project}' {count}x in prose");
            insert_edge(
                &tx,
                note_id,
                dst,
                "mentions",
                "indexer",
                *count as f64,
                &rationale,
            )?;
            edge_count += 1;
        }

        for target in &extraction.wiki_links {
            let Some(dst) = upsert_entity(&tx, target, &ignore)? else {
                continue;
            };
            let rationale = format!("wiki link [[{}]]", target.trim());
            insert_edge(
                &tx,
                note_id,
                dst,
                "wikilink",
                "human",
                WEIGHT_WIKILINK,
                &rationale,
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
            // Weakest signal: a backticked span that looked entity-shaped.
            let rationale = format!("backticked span `{}`", span.trim());
            insert_edge(
                &tx,
                note_id,
                dst,
                "mentions",
                "indexer",
                WEIGHT_CODE_SPAN,
                &rationale,
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
            insert_edge(
                &tx,
                note_id,
                loop_id,
                "contains",
                "indexer",
                WEIGHT_STRUCTURAL,
                "open loop found in note",
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
                let rationale = format!("loop text mentions project '{project}'");
                insert_edge(
                    &tx,
                    loop_id,
                    dst,
                    "mentions",
                    "indexer",
                    WEIGHT_STRUCTURAL,
                    &rationale,
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

    // Rebuild the FTS index from the now-populated `notes` content table
    // in one pass. Cheaper and less error-prone than per-row FTS inserts.
    tx.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])?;

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

/// How much to trust an inferred edge, and why. `weight` is a rough
/// confidence signal (higher = stronger); `rationale` is the human-readable
/// evidence that produced the edge. These let queries separate ground truth
/// (wiki links a human wrote) from heuristic guesses (a backticked span that
/// happened to look like an entity), instead of treating every edge alike.
const WEIGHT_WIKILINK: f64 = 1.0; // human wrote the link
const WEIGHT_CODE_SPAN: f64 = 0.3; // weakest: a backticked span, guessed
const WEIGHT_STRUCTURAL: f64 = 1.0; // derived structure (loop containment)

/// Insert an inferred/derived edge with its provenance filled in. Dedup is
/// `INSERT OR IGNORE` on (src, dst, kind, provenance), so the first edge for
/// a pair wins and its rationale is kept.
fn insert_edge(
    tx: &rusqlite::Transaction,
    src: i64,
    dst: i64,
    kind: &str,
    provenance: &str,
    weight: f64,
    rationale: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO edges (src, dst, kind, provenance, weight, rationale)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![src, dst, kind, provenance, weight, rationale],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed_index(path: &std::path::Path) {
        // Build a current-schema index with one node so we can tell whether
        // a later open wiped it.
        let conn = open_db_for_rebuild_at(path).unwrap();
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('project', 'canary', NULL, '{}')",
            [],
        )
        .unwrap();
    }

    fn node_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn read_on_missing_db_errors_instead_of_creating_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        let err = open_db_at(&path).unwrap_err().to_string();
        assert!(err.contains("no index"), "unexpected error: {err}");
        // Must not have created the file as a side effect.
        assert!(!path.exists());
    }

    #[test]
    fn read_after_rebuild_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        seed_index(&path);
        let conn = open_db_at(&path).unwrap();
        assert_eq!(node_count(&conn), 1);
    }

    #[test]
    fn read_on_stale_schema_errors_and_does_not_wipe() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        seed_index(&path);

        // Simulate an index left by an older raft.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
                .unwrap();
        }

        let err = open_db_at(&path).unwrap_err().to_string();
        assert!(err.contains("different raft version"), "unexpected: {err}");

        // The data must survive - a read must never destroy the index.
        let conn = Connection::open(&path).unwrap();
        assert_eq!(node_count(&conn), 1);
    }

    #[test]
    fn rebuild_resets_stale_schema_then_read_works() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        seed_index(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
                .unwrap();
        }

        // Rebuild is allowed to reset; it upgrades the schema version.
        let _ = open_db_for_rebuild_at(&path).unwrap();
        let conn = open_db_at(&path).unwrap();
        // The stale table was dropped and recreated empty by rebuild.
        assert_eq!(node_count(&conn), 0);
    }
}
