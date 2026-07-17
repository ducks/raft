use crate::config::{Config, SourceKind};
use crate::extract;
use crate::scan;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i64 = 5;

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

CREATE TABLE IF NOT EXISTS index_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

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
/// to replace the index (see `rebuild`).
pub fn open_db() -> Result<Connection> {
    open_db_at(&crate::config::db_path()?)
}

/// Open the index for reading at a specific path. Never mutates the schema:
/// if the database is missing or was built by an older raft (stale schema
/// version), returns an error telling the user to reindex rather than
/// silently wiping the index and returning empty results. Only `rebuild` is
/// allowed to replace the index (see `rebuild_at`).
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

/// Create a brand-new database with the current schema. Rebuilds only call
/// this for a temporary path; the live index is never modified in place.
fn create_db_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("could not remove stale index at {}", path.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("could not open database at {}", path.display()))?;
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(conn)
}

struct TemporaryIndex {
    path: PathBuf,
    keep: bool,
}

impl TemporaryIndex {
    fn beside(live_path: &Path) -> Result<Self> {
        let file_name = live_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("database path has no UTF-8 filename")?;
        let path = live_path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
        Ok(Self { path, keep: false })
    }

    fn install(mut self, live_path: &Path) -> Result<()> {
        std::fs::rename(&self.path, live_path).with_context(|| {
            format!(
                "could not replace index {} with completed rebuild {}",
                live_path.display(),
                self.path.display()
            )
        })?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct IndexStats {
    pub notes: usize,
    pub projects: usize,
    pub entities: usize,
    pub edges: usize,
    pub loops: usize,
    /// Repos whose git metadata was reused from the previous index (fingerprint
    /// unchanged), skipping the git subprocesses.
    pub git_cached: usize,
    /// Repos whose git metadata was refreshed by shelling out to git (new,
    /// changed, or fingerprint unavailable).
    pub git_refreshed: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub database: String,
    pub indexed: bool,
    pub healthy: bool,
    pub schema_version: Option<i64>,
    pub expected_schema_version: i64,
    pub last_rebuilt: Option<String>,
    pub counts: Option<IndexCounts>,
    pub error: Option<String>,
    pub sources: Vec<SourceStatus>,
}

#[derive(Debug, Serialize)]
pub struct IndexCounts {
    pub notes: i64,
    pub projects: i64,
    pub entities: i64,
    pub loops: i64,
    pub edges: i64,
}

#[derive(Debug, Serialize)]
pub struct SourceStatus {
    pub kind: String,
    pub configured_path: String,
    pub path: String,
    pub healthy: bool,
    pub error: Option<String>,
}

pub fn status(config: &Config) -> Result<IndexStatus> {
    status_at(config, &crate::config::db_path()?)
}

fn status_at(config: &Config, path: &Path) -> Result<IndexStatus> {
    let sources = config.sources.iter().map(source_status).collect();
    let mut status = IndexStatus {
        database: path.to_string_lossy().into_owned(),
        indexed: path.exists(),
        healthy: false,
        schema_version: None,
        expected_schema_version: SCHEMA_VERSION,
        last_rebuilt: None,
        counts: None,
        error: None,
        sources,
    };
    if !status.indexed {
        status.error = Some("index does not exist; run `raft index`".to_string());
        return Ok(status);
    }

    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(err) => {
            status.error = Some(format!("could not open index: {err}"));
            return Ok(status);
        }
    };
    let version = match conn.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(err) => {
            status.error = Some(format!("could not read schema version: {err}"));
            return Ok(status);
        }
    };
    status.schema_version = Some(version);
    if version != SCHEMA_VERSION {
        status.error = Some(format!(
            "schema {version} is stale; expected {SCHEMA_VERSION}; run `raft index`"
        ));
        return Ok(status);
    }

    let integrity: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        status.error = Some(format!("index failed integrity check: {integrity}"));
        return Ok(status);
    }
    status.last_rebuilt = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'last_rebuilt'",
            [],
            |row| row.get(0),
        )
        .ok();
    let (notes, projects, entities, loops): (i64, i64, i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(kind = 'note'), 0),
                COALESCE(SUM(kind = 'project'), 0),
                COALESCE(SUM(kind = 'entity'), 0),
                COALESCE(SUM(kind = 'loop'), 0)
         FROM nodes",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let edges = conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
    status.counts = Some(IndexCounts {
        notes,
        projects,
        entities,
        loops,
        edges,
    });
    status.healthy = true;
    Ok(status)
}

fn source_status(source: &crate::config::Source) -> SourceStatus {
    let path = crate::config::expand_tilde(&source.path);
    let result = std::fs::read_dir(&path);
    let error = result.err().map(|err| err.to_string());
    SourceStatus {
        kind: format!("{:?}", source.kind).to_lowercase(),
        configured_path: source.path.clone(),
        path: path.to_string_lossy().into_owned(),
        healthy: error.is_none(),
        error,
    }
}

/// Full rebuild: scan every source, replace the index.
pub fn rebuild(config: &Config) -> Result<IndexStats> {
    rebuild_at(config, &crate::config::db_path()?)
}

fn rebuild_at(config: &Config, live_path: &Path) -> Result<IndexStats> {
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

    // Load cached git metadata from the live index (if any) so unchanged
    // repos can skip the git subprocesses. Keyed by project path; each entry
    // is (stored fingerprint, full meta JSON). Missing/unreadable index just
    // yields an empty cache and every repo is refreshed.
    let git_cache = load_git_cache(live_path).unwrap_or_default();

    let temporary = TemporaryIndex::beside(live_path)?;
    let mut conn = create_db_at(&temporary.path)?;

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
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM notes_fts; DELETE FROM edges; DELETE FROM notes; DELETE FROM nodes;",
    )?;

    let mut project_ids = HashMap::new();
    let mut git_refreshed = 0usize;
    let mut git_cached = 0usize;
    for project in &projects {
        let path_key = project.path.to_string_lossy().into_owned();
        let meta = project_meta(
            project,
            &git_cache,
            &path_key,
            &mut git_refreshed,
            &mut git_cached,
        );
        // Same directory name can exist under multiple sources (tmp, notes,
        // scratch dirs); first source wins for v0.
        tx.execute(
            "INSERT OR IGNORE INTO nodes (kind, name, path, meta) VALUES ('project', ?1, ?2, ?3)",
            params![project.name, &path_key, meta],
        )?;
        let project_id: i64 = tx.query_row(
            "SELECT id FROM nodes WHERE kind = 'project' AND name = ?1",
            params![project.name],
            |row| row.get(0),
        )?;
        project_ids
            .entry(extract::canonicalize(&project.name))
            .or_insert(project_id);
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
            let dst = project_ids[&extract::canonicalize(project)];
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
            let Some(target_node) = resolve_target(&tx, target, &ignore, &project_ids)? else {
                continue;
            };
            let rationale = format!("wiki link [[{}]]", target.trim());
            insert_edge(
                &tx,
                note_id,
                target_node.id,
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
            let Some(target_node) = resolve_target(&tx, span, &ignore, &project_ids)? else {
                continue;
            };
            if target_node.is_project {
                continue;
            }
            // Weakest signal: a backticked span that looked entity-shaped.
            let rationale = format!("backticked span `{}`", span.trim());
            insert_edge(
                &tx,
                note_id,
                target_node.id,
                "mentions",
                "indexer",
                WEIGHT_CODE_SPAN,
                &rationale,
            )?;
            edge_count += 1;
        }

        for (ordinal, open_loop) in extract::extract_loops(&note.body).into_iter().enumerate() {
            let identity = format!("{name}#loop-{ordinal}");
            let meta = serde_json::json!({
                "text": &open_loop.text,
                "section": &open_loop.section,
            })
            .to_string();
            // A loop is an occurrence in one note, not a globally canonical
            // task. Repeated text remains independently attributable to its
            // own note, date, section, and project edges.
            tx.execute(
                "INSERT INTO nodes (kind, name, path, meta) VALUES ('loop', ?1, ?2, ?3)",
                params![identity, name, meta],
            )?;
            let loop_id = tx.last_insert_rowid();
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
                let dst = project_ids[&extract::canonicalize(project)];
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
    tx.execute(
        "INSERT INTO index_meta (key, value) VALUES ('last_rebuilt', ?1)",
        params![chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)],
    )?;

    tx.commit()?;

    let stats = IndexStats {
        notes: all_notes.len(),
        projects: projects.len(),
        entities: entity_count as usize,
        edges: edge_count,
        loops: loop_count,
        git_cached,
        git_refreshed,
    };

    validate_index(&conn)?;
    drop(conn);
    temporary.install(live_path)?;

    Ok(stats)
}

/// Load cached git state from the live index: project path -> (fingerprint,
/// full meta JSON string). Opens read-only and tolerates any error (missing
/// index, stale schema, unreadable) by returning an empty cache, which just
/// means every repo gets refreshed - correctness never depends on the cache.
fn load_git_cache(live_path: &Path) -> Result<HashMap<String, (String, String)>> {
    let conn = Connection::open_with_flags(live_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Ok(HashMap::new());
    }
    let mut stmt =
        conn.prepare("SELECT path, meta FROM nodes WHERE kind = 'project' AND path IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        let path: String = row.get(0)?;
        let meta: String = row.get(1)?;
        Ok((path, meta))
    })?;
    let mut cache = HashMap::new();
    for row in rows {
        let (path, meta) = row?;
        if let Some(fp) = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| {
                v.get("git_fingerprint")
                    .and_then(|f| f.as_str())
                    .map(String::from)
            })
        {
            cache.insert(path, (fp, meta));
        }
    }
    Ok(cache)
}

/// Produce the meta JSON to store for a project node, reusing cached git
/// metadata when the repo's fingerprint is unchanged and refreshing (a git
/// subprocess) otherwise. The stored object carries `git_fingerprint`
/// alongside the `branch`/`commits` fields the queries read.
fn project_meta(
    project: &scan::Project,
    git_cache: &HashMap<String, (String, String)>,
    path_key: &str,
    refreshed: &mut usize,
    cached: &mut usize,
) -> String {
    if !project.is_repo {
        return "{}".to_string();
    }

    // Reuse the cached meta verbatim when the fingerprint matches.
    if let Some(fp) = &project.git_fingerprint {
        if let Some((cached_fp, cached_meta)) = git_cache.get(path_key) {
            if cached_fp == fp {
                *cached += 1;
                return cached_meta.clone();
            }
        }
    }

    // Miss: shell out for fresh metadata and stamp the current fingerprint.
    *refreshed += 1;
    let mut meta = scan::git_metadata(&project.path).unwrap_or_else(|| serde_json::json!({}));
    if let (Some(obj), Some(fp)) = (meta.as_object_mut(), &project.git_fingerprint) {
        obj.insert(
            "git_fingerprint".to_string(),
            serde_json::Value::String(fp.clone()),
        );
    }
    meta.to_string()
}

fn validate_index(conn: &Connection) -> Result<()> {
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        anyhow::bail!("rebuilt index failed integrity check: {integrity}");
    }

    // With an external-content FTS5 table, SELECT COUNT(*) reads through
    // `notes` and cannot detect a stale search index. rank=1 makes FTS5's
    // integrity command compare the index against that content table.
    conn.execute(
        "INSERT INTO notes_fts(notes_fts, rank) VALUES('integrity-check', 1)",
        [],
    )
    .context("rebuilt index failed FTS integrity check")?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        anyhow::bail!("rebuilt index has schema version {version}, expected {SCHEMA_VERSION}");
    }
    Ok(())
}

/// Common words that show up backticked (table headers, YAML keys,
/// prose emphasis) but carry no identity.
const STOPWORDS: &[&str] = &[
    "what", "how", "why", "when", "where", "who", "true", "false", "nil", "null", "none", "yes",
    "no", "n/a", "tbd", "todo", "done", "new", "old", "the", "and", "for", "not",
];

struct TargetNode {
    id: i64,
    is_project: bool,
}

/// Resolve extracted text to one graph identity. Known projects take
/// precedence over entities, regardless of case or edge punctuation. New
/// entities use their canonical name for identity and retain the first-seen
/// spelling in metadata for display.
fn resolve_target(
    tx: &rusqlite::Transaction,
    raw: &str,
    ignore: &HashSet<String>,
    project_ids: &HashMap<String, i64>,
) -> Result<Option<TargetNode>> {
    let canonical = extract::canonicalize(raw);
    if canonical.is_empty() || ignore.contains(&canonical) {
        return Ok(None);
    }
    if let Some(id) = project_ids.get(&canonical) {
        return Ok(Some(TargetNode {
            id: *id,
            is_project: true,
        }));
    }
    if canonical.len() < 3 || STOPWORDS.contains(&canonical.as_str()) {
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
    Ok(Some(TargetNode {
        id,
        is_project: false,
    }))
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
        let conn = create_db_at(path).unwrap();
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
    fn atomic_rebuild_replaces_the_live_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        seed_index(&path);
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("2026-07-11.md"), "working on [[Raft]]").unwrap();
        let config = Config {
            sources: vec![crate::config::Source {
                path: notes.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };

        let stats = rebuild_at(&config, &path).unwrap();

        assert_eq!(stats.notes, 1);
        let conn = open_db_at(&path).unwrap();
        assert_eq!(node_count(&conn), 2);
        validate_index(&conn).unwrap();
    }

    #[test]
    fn failed_scan_preserves_the_live_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raft.db");
        seed_index(&path);
        let missing = dir.path().join("missing-notes");
        let config = Config {
            sources: vec![crate::config::Source {
                path: missing.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };

        assert!(rebuild_at(&config, &path).is_err());

        let conn = open_db_at(&path).unwrap();
        assert_eq!(node_count(&conn), 1);
        let canary: String = conn
            .query_row("SELECT name FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(canary, "canary");
    }

    #[test]
    fn temporary_index_is_removed_when_validation_fails() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let temporary = TemporaryIndex::beside(&live_path).unwrap();
        let temporary_path = temporary.path.clone();
        let conn = create_db_at(&temporary_path).unwrap();
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('note', 'n.md', 'n.md', '{}')",
            [],
        )
        .unwrap();
        let note_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO notes (node_id, body) VALUES (?1, 'indexed text')",
            params![note_id],
        )
        .unwrap();
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])
            .unwrap();
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('delete-all')", [])
            .unwrap();

        assert!(validate_index(&conn).is_err());
        drop(conn);
        drop(temporary);

        assert!(!temporary_path.exists());
        assert!(!live_path.exists());
    }

    #[test]
    fn wiki_links_and_mentions_resolve_to_one_project_identity() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let projects = dir.path().join("projects");
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(projects.join("Raft")).unwrap();
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(
            notes.join("2026-07-11.md"),
            "Worked on [[RAFT,]] today. Need to fix `raft`.",
        )
        .unwrap();
        let config = Config {
            sources: vec![
                crate::config::Source {
                    path: projects.to_string_lossy().into_owned(),
                    kind: SourceKind::Projects,
                },
                crate::config::Source {
                    path: notes.to_string_lossy().into_owned(),
                    kind: SourceKind::Notes,
                },
            ],
            ignore: Vec::new(),
            daily_note: None,
        };

        let stats = rebuild_at(&config, &live_path).unwrap();
        let conn = open_db_at(&live_path).unwrap();

        assert_eq!(stats.projects, 1);
        assert_eq!(stats.entities, 0);
        let project_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE kind = 'project'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(project_count, 1);

        let about = crate::query::about(&conn, "raft,").unwrap().unwrap();
        assert_eq!(about.kind, "project");
        assert_eq!(about.notes.len(), 1);

        let facts = crate::query::why(&conn, "RAFT,", 0.0).unwrap().unwrap();
        assert!(facts.iter().any(|fact| fact.relation == "wikilink"));
        assert!(facts.iter().any(|fact| fact.relation == "mentions"));
    }

    #[test]
    fn canonical_wiki_link_variants_share_one_entity() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("note.md"), "Compare [[NixOS,]] with [[nixos]].").unwrap();
        let config = Config {
            sources: vec![crate::config::Source {
                path: notes.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };

        let stats = rebuild_at(&config, &live_path).unwrap();
        let conn = open_db_at(&live_path).unwrap();

        assert_eq!(stats.entities, 1);
        assert!(crate::query::about(&conn, "NIXOS,").unwrap().is_some());
    }

    #[test]
    fn repeated_loop_text_keeps_each_occurrence_context() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let older_path = notes.join("2026-07-01.md");
        let newer_path = notes.join("2026-07-10.md");
        std::fs::write(&older_path, "## Next steps\n- write tests\n").unwrap();
        std::fs::write(&newer_path, "## Follow-ups\n- write tests\n").unwrap();
        let config = Config {
            sources: vec![crate::config::Source {
                path: notes.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };

        let stats = rebuild_at(&config, &live_path).unwrap();
        let conn = open_db_at(&live_path).unwrap();
        let dangling = crate::query::dangling(&conn, None, 10).unwrap();

        assert_eq!(stats.loops, 2);
        assert_eq!(dangling.len(), 2);
        assert_eq!(dangling[0].text, "write tests");
        assert_eq!(dangling[0].section.as_deref(), Some("Next steps"));
        assert_eq!(dangling[0].first_seen.as_deref(), Some("2026-07-01"));
        assert_eq!(dangling[0].note_path, older_path.to_string_lossy());
        assert_eq!(dangling[1].section.as_deref(), Some("Follow-ups"));
        assert_eq!(dangling[1].first_seen.as_deref(), Some("2026-07-10"));
        assert_eq!(dangling[1].note_path, newer_path.to_string_lossy());
        assert!(dangling.iter().all(|item| item.sightings == 2));
    }

    #[test]
    fn status_reports_rebuild_metadata_counts_and_sources() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("note.md"), "See [[NixOS]].").unwrap();
        let config = Config {
            sources: vec![crate::config::Source {
                path: notes.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };
        rebuild_at(&config, &live_path).unwrap();

        let status = status_at(&config, &live_path).unwrap();

        assert!(status.indexed);
        assert!(status.healthy);
        assert_eq!(status.schema_version, Some(SCHEMA_VERSION));
        assert!(status.last_rebuilt.is_some());
        let counts = status.counts.unwrap();
        assert_eq!(counts.notes, 1);
        assert_eq!(counts.entities, 1);
        assert_eq!(counts.edges, 1);
        assert_eq!(status.sources.len(), 1);
        assert!(status.sources[0].healthy);
    }

    #[test]
    fn status_is_useful_when_index_and_source_are_missing() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let missing = dir.path().join("missing");
        let config = Config {
            sources: vec![crate::config::Source {
                path: missing.to_string_lossy().into_owned(),
                kind: SourceKind::Notes,
            }],
            ignore: Vec::new(),
            daily_note: None,
        };

        let status = status_at(&config, &live_path).unwrap();

        assert!(!status.indexed);
        assert!(!status.healthy);
        assert!(status.error.unwrap().contains("does not exist"));
        assert!(!status.sources[0].healthy);
        assert!(status.sources[0].error.is_some());
    }

    fn git(repo: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn projects_config(projects: &Path) -> Config {
        Config {
            sources: vec![crate::config::Source {
                path: projects.to_string_lossy().into_owned(),
                kind: SourceKind::Projects,
            }],
            ignore: Vec::new(),
            daily_note: None,
        }
    }

    #[test]
    fn git_metadata_reused_when_repo_unchanged_then_refreshed_after_commit() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let projects = dir.path().join("projects");
        let repo = projects.join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "one"]);

        let config = projects_config(&projects);

        // First build: nothing cached, so the repo is refreshed.
        let first = rebuild_at(&config, &live_path).unwrap();
        assert_eq!(first.git_refreshed, 1);
        assert_eq!(first.git_cached, 0);

        // Second build with no change: fingerprint matches, metadata reused.
        let second = rebuild_at(&config, &live_path).unwrap();
        assert_eq!(second.git_cached, 1, "unchanged repo should hit the cache");
        assert_eq!(second.git_refreshed, 0);

        // The cached metadata must still be correct/queryable.
        let conn = open_db_at(&live_path).unwrap();
        let about = crate::query::about(&conn, "myrepo").unwrap().unwrap();
        assert_eq!(about.kind, "project");
        assert!(about.git.is_some());
        drop(conn);

        // A new commit changes the fingerprint: refresh again.
        std::fs::write(repo.join("b.txt"), "two").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "two"]);
        let third = rebuild_at(&config, &live_path).unwrap();
        assert_eq!(third.git_refreshed, 1, "a commit must force a refresh");
        assert_eq!(third.git_cached, 0);
    }

    #[test]
    fn stored_project_meta_carries_fingerprint() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("raft.db");
        let projects = dir.path().join("projects");
        let repo = projects.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "x"]);

        let config = projects_config(&projects);
        rebuild_at(&config, &live_path).unwrap();

        let conn = open_db_at(&live_path).unwrap();
        let meta: String = conn
            .query_row(
                "SELECT meta FROM nodes WHERE kind = 'project' AND name = 'r'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert!(v.get("git_fingerprint").and_then(|f| f.as_str()).is_some());
        // The query-facing fields are still at the top level, not nested.
        assert!(v.get("branch").is_some());
        assert!(v.get("commits").is_some());
    }
}
