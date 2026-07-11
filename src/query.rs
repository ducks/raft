use crate::extract;
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub note_date: Option<String>,
    pub snippet: String,
}

pub fn search(conn: &Connection, term: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let Some(match_query) = fts_query(term) else {
        // Term had no indexable tokens (all punctuation/whitespace).
        return Ok(Vec::new());
    };

    // bm25() ranks by relevance (lower is better in SQLite's signed form,
    // so ORDER BY ascending). snippet() returns a windowed excerpt with the
    // matched terms bracketed, collapsing the hand-rolled snippet logic.
    let mut stmt = conn.prepare(
        "SELECT nodes.path, notes.note_date,
                snippet(notes_fts, 0, '', '', ' ... ', 12)
         FROM notes_fts
         JOIN notes ON notes.node_id = notes_fts.rowid
         JOIN nodes ON nodes.id = notes.node_id
         WHERE notes_fts MATCH ?1
         ORDER BY bm25(notes_fts)
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![match_query, limit as i64], |row| {
        Ok(SearchHit {
            path: row.get(0)?,
            note_date: row.get(1)?,
            snippet: normalize_snippet(&row.get::<_, String>(2)?),
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Turn a raw user term into a safe FTS5 MATCH query. Splits on anything
/// that isn't alphanumeric and quotes each token as a phrase, so
/// punctuation-heavy input (`c++`, `foo-bar`, stray quotes) can't produce
/// an FTS syntax error. Multiple tokens become an AND of quoted phrases.
/// Returns None when nothing indexable remains.
fn fts_query(term: &str) -> Option<String> {
    let tokens: Vec<String> = term
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

/// Collapse whitespace/newlines in an FTS snippet to a single line.
fn normalize_snippet(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Serialize)]
pub struct About {
    pub name: String,
    pub kind: String,
    pub git: Option<serde_json::Value>,
    pub notes: Vec<NoteRef>,
    pub co_mentioned: Vec<CoMention>,
}

#[derive(Debug, Serialize)]
pub struct NoteRef {
    pub path: String,
    pub note_date: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CoMention {
    pub name: String,
    pub kind: String,
    pub shared_notes: i64,
}

fn find_named_node(conn: &Connection, name: &str) -> Result<Option<(i64, String, String)>> {
    let canonical = extract::canonicalize(name);
    let mut stmt = conn.prepare(
        "SELECT id, kind, meta, name FROM nodes
         WHERE kind IN ('project', 'entity')
         ORDER BY CASE kind WHEN 'project' THEN 0 ELSE 1 END, id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let stored_name: String = row.get(3)?;
        if extract::canonicalize(&stored_name) == canonical {
            return Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?)));
        }
    }
    Ok(None)
}

pub fn about(conn: &Connection, name: &str) -> Result<Option<About>> {
    let node = find_named_node(conn, name)?;

    let Some((id, kind, meta)) = node else {
        return Ok(None);
    };

    let git = if kind == "project" {
        serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .filter(|v| v.get("branch").is_some())
    } else {
        None
    };

    let mut stmt = conn.prepare(
        "SELECT DISTINCT nodes.path, notes.note_date
         FROM edges
         JOIN nodes ON nodes.id = edges.src
         JOIN notes ON notes.node_id = edges.src
         WHERE edges.dst = ?1
         ORDER BY notes.note_date DESC NULLS LAST",
    )?;
    let notes: Vec<NoteRef> = stmt
        .query_map(params![id], |row| {
            Ok(NoteRef {
                path: row.get(0)?,
                note_date: row.get(1)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Other targets mentioned by the same notes, ranked by overlap.
    let mut stmt = conn.prepare(
        "SELECT other.name, other.kind, COUNT(DISTINCT e2.src) AS shared
         FROM edges e1
         JOIN edges e2 ON e2.src = e1.src AND e2.dst != e1.dst
         JOIN nodes other ON other.id = e2.dst
         WHERE e1.dst = ?1
         GROUP BY other.id
         HAVING shared >= 2
         ORDER BY shared DESC
         LIMIT 20",
    )?;
    let co_mentioned: Vec<CoMention> = stmt
        .query_map(params![id], |row| {
            Ok(CoMention {
                name: row.get(0)?,
                kind: row.get(1)?,
                shared_notes: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    Ok(Some(About {
        name: name.to_string(),
        kind,
        git,
        notes,
        co_mentioned,
    }))
}

/// One inbound edge to a target, with the evidence that produced it.
#[derive(Debug, Serialize)]
pub struct EdgeFact {
    /// The node the edge comes from (a note path, or a loop/entity name).
    pub from: String,
    pub from_kind: String,
    pub relation: String,
    pub provenance: String,
    pub weight: f64,
    pub rationale: Option<String>,
}

/// Inspect why a target is in the graph: every edge pointing at it, with
/// origin, confidence weight, and the rationale that created it. This makes
/// inferred relationships auditable rather than opaque - you can see that a
/// mention came from a backticked span (weak) versus a wiki link a human
/// wrote (ground truth). Returns None if the name is unknown, ordered
/// strongest-evidence first. `min_weight` filters out weak edges.
pub fn why(conn: &Connection, name: &str, min_weight: f64) -> Result<Option<Vec<EdgeFact>>> {
    let target = find_named_node(conn, name)?.map(|node| node.0);

    let Some(target_id) = target else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT src.name, src.kind, e.kind, e.provenance, e.weight, e.rationale
         FROM edges e
         JOIN nodes src ON src.id = e.src
         WHERE e.dst = ?1 AND e.weight >= ?2
         ORDER BY e.weight DESC, src.name",
    )?;
    let facts: Vec<EdgeFact> = stmt
        .query_map(params![target_id, min_weight], |row| {
            Ok(EdgeFact {
                from: row.get(0)?,
                from_kind: row.get(1)?,
                relation: row.get(2)?,
                provenance: row.get(3)?,
                weight: row.get(4)?,
                rationale: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    Ok(Some(facts))
}

#[derive(Debug, Serialize)]
pub struct Dangling {
    pub text: String,
    pub section: Option<String>,
    pub first_seen: Option<String>,
    pub age_days: Option<i64>,
    pub sightings: i64,
    pub note_path: String,
}

/// Open loops, stalest first. `about` filters to loops whose text
/// mentions the given project or entity.
pub fn dangling(conn: &Connection, about: Option<&str>, limit: usize) -> Result<Vec<Dangling>> {
    let about_id: Option<i64> = match about {
        None => None,
        Some(name) => {
            let id = find_named_node(conn, name)?.map(|node| node.0);
            match id {
                Some(id) => Some(id),
                None => return Ok(Vec::new()),
            }
        }
    };

    let mut stmt = conn.prepare(
        "SELECT l.name, l.meta,
                MIN(COALESCE(n.note_date, n.mtime)) AS first_seen,
                COUNT(DISTINCT e.src) AS sightings,
                MIN(src_node.path) AS note_path
         FROM nodes l
         JOIN edges e ON e.dst = l.id AND e.kind = 'contains'
         JOIN notes n ON n.node_id = e.src
         JOIN nodes src_node ON src_node.id = e.src
         WHERE l.kind = 'loop'
           AND (?1 IS NULL OR l.id IN
                (SELECT src FROM edges WHERE dst = ?1 AND kind = 'mentions'))
         GROUP BY l.id
         ORDER BY first_seen ASC NULLS LAST
         LIMIT ?2",
    )?;

    let today = chrono::Local::now().date_naive();
    let rows = stmt.query_map(params![about_id, limit as i64], |row| {
        let text: String = row.get(0)?;
        let meta: String = row.get(1)?;
        let first_seen: Option<String> = row.get(2)?;
        let sightings: i64 = row.get(3)?;
        let note_path: String = row.get(4)?;
        Ok((text, meta, first_seen, sightings, note_path))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (text, meta, first_seen, sightings, note_path) = row?;
        let section = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| v.get("section").and_then(|s| s.as_str()).map(String::from));
        let age_days = first_seen
            .as_deref()
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .map(|d| (today - d).num_days());
        out.push(Dangling {
            text,
            section,
            first_seen,
            age_days,
            sightings,
            note_path,
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct CoMentionPair {
    pub a: String,
    pub a_kind: String,
    pub b: String,
    pub b_kind: String,
    pub shared_notes: i64,
    pub span_days: i64,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct TemporalPair {
    pub a: String,
    pub b: String,
    pub shared_days: usize,
}

#[derive(Debug, Serialize)]
pub struct Connections {
    pub co_mentions: Vec<CoMentionPair>,
    pub temporal: Vec<TemporalPair>,
}

/// Surface connections nobody wrote down: node pairs that keep
/// appearing in the same notes (affinity-scored so hub nodes don't
/// drown everything), and projects whose commits land on the same days.
pub fn connect(conn: &Connection, min_shared: i64, limit: usize) -> Result<Connections> {
    // A pair is only interesting if it keeps reuniting over time;
    // co-occurrence inside one burst of notes is just one story's
    // vocabulary. Require the shared notes to span at least two weeks.
    let mut stmt = conn.prepare(
        "WITH mention AS (
             SELECT DISTINCT e.src AS note, e.dst AS target,
                    COALESCE(nt.note_date, nt.mtime) AS d
             FROM edges e
             JOIN nodes n ON n.id = e.dst
             JOIN notes nt ON nt.node_id = e.src
             WHERE e.kind IN ('mentions', 'wikilink')
               AND n.kind IN ('project', 'entity')
         ),
         freq AS (SELECT target, COUNT(*) AS c FROM mention GROUP BY target)
         SELECT na.name, na.kind, nb.name, nb.kind,
                COUNT(*) AS shared, fa.c, fb.c,
                CAST(julianday(MAX(a.d)) - julianday(MIN(a.d)) AS INTEGER) AS span
         FROM mention a
         JOIN mention b ON a.note = b.note AND a.target < b.target
         JOIN freq fa ON fa.target = a.target
         JOIN freq fb ON fb.target = b.target
         JOIN nodes na ON na.id = a.target
         JOIN nodes nb ON nb.id = b.target
         GROUP BY a.target, b.target
         HAVING shared >= ?1 AND span >= 14",
    )?;

    let mut co_mentions: Vec<CoMentionPair> = stmt
        .query_map(params![min_shared], |row| {
            let shared: i64 = row.get(4)?;
            let fa: i64 = row.get(5)?;
            let fb: i64 = row.get(6)?;
            Ok(CoMentionPair {
                a: row.get(0)?,
                a_kind: row.get(1)?,
                b: row.get(2)?,
                b_kind: row.get(3)?,
                shared_notes: shared,
                span_days: row.get(7)?,
                score: shared as f64 / ((fa * fb) as f64).sqrt(),
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    co_mentions.sort_by(|x, y| y.score.total_cmp(&x.score));
    co_mentions.truncate(limit);

    // Projects whose recent commits share days.
    let mut stmt = conn.prepare("SELECT name, meta FROM nodes WHERE kind = 'project'")?;
    let projects: Vec<(String, std::collections::HashSet<String>)> = stmt
        .query_map([], |row| {
            let name: String = row.get(0)?;
            let meta: String = row.get(1)?;
            Ok((name, meta))
        })?
        .filter_map(|r| r.ok())
        .map(|(name, meta)| {
            let days: std::collections::HashSet<String> =
                serde_json::from_str::<serde_json::Value>(&meta)
                    .ok()
                    .and_then(|v| v.get("commits").cloned())
                    .and_then(|c| c.as_array().cloned())
                    .map(|commits| {
                        commits
                            .iter()
                            .filter_map(|c| {
                                c.get("date").and_then(|d| d.as_str()).map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            (name, days)
        })
        .collect();

    let mut temporal = Vec::new();
    for (i, (name_a, days_a)) in projects.iter().enumerate() {
        if days_a.is_empty() {
            continue;
        }
        for (name_b, days_b) in projects.iter().skip(i + 1) {
            let shared = days_a.intersection(days_b).count();
            if shared as i64 >= min_shared {
                temporal.push(TemporalPair {
                    a: name_a.clone(),
                    b: name_b.clone(),
                    shared_days: shared,
                });
            }
        }
    }
    temporal.sort_by(|x, y| y.shared_days.cmp(&x.shared_days));
    temporal.truncate(limit);

    Ok(Connections {
        co_mentions,
        temporal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::open_in_memory;

    /// Insert a note node + body, then refresh the FTS index.
    fn add_note(conn: &Connection, path: &str, date: Option<&str>, body: &str) {
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('note', ?1, ?1, '{}')",
            params![path],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO notes (node_id, body, note_date, mtime) VALUES (?1, ?2, ?3, NULL)",
            params![id, body, date],
        )
        .unwrap();
    }

    fn rebuild_fts(conn: &Connection) {
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])
            .unwrap();
    }

    fn add_node(conn: &Connection, kind: &str, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES (?1, ?2, NULL, '{}')",
            params![kind, name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[allow(clippy::too_many_arguments)]
    fn add_edge(
        conn: &Connection,
        src: i64,
        dst: i64,
        kind: &str,
        provenance: &str,
        weight: f64,
        rationale: &str,
    ) {
        conn.execute(
            "INSERT INTO edges (src, dst, kind, provenance, weight, rationale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![src, dst, kind, provenance, weight, rationale],
        )
        .unwrap();
    }

    #[test]
    fn why_returns_edges_strongest_first_with_provenance() {
        let conn = open_in_memory().unwrap();
        let note = add_node(&conn, "note", "/n/2026-07-01.md");
        let target = add_node(&conn, "entity", "nixos");
        // A weak backticked-span edge and a strong dictionary-match edge.
        add_edge(
            &conn,
            note,
            target,
            "mentions",
            "indexer",
            0.3,
            "backticked span `nixos`",
        );
        let note2 = add_node(&conn, "note", "/n/2026-07-02.md");
        add_edge(
            &conn,
            note2,
            target,
            "mentions",
            "indexer",
            4.0,
            "matched project name 'nixos' 4x in prose",
        );

        let facts = why(&conn, "nixos", 0.0).unwrap().unwrap();
        assert_eq!(facts.len(), 2);
        // Strongest evidence first.
        assert_eq!(facts[0].weight, 4.0);
        assert_eq!(facts[1].weight, 0.3);
        assert_eq!(facts[0].provenance, "indexer");
        assert_eq!(
            facts[0].rationale.as_deref(),
            Some("matched project name 'nixos' 4x in prose")
        );
    }

    #[test]
    fn why_min_weight_filters_weak_edges() {
        let conn = open_in_memory().unwrap();
        let note = add_node(&conn, "note", "/n/a.md");
        let target = add_node(&conn, "entity", "log");
        add_edge(
            &conn,
            note,
            target,
            "mentions",
            "indexer",
            0.3,
            "backticked span `log`",
        );
        let note2 = add_node(&conn, "note", "/n/b.md");
        add_edge(
            &conn,
            note2,
            target,
            "mentions",
            "indexer",
            2.0,
            "matched project name 'log' 2x in prose",
        );

        // Filtering above the weak span drops it.
        let facts = why(&conn, "log", 1.0).unwrap().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].weight, 2.0);
    }

    #[test]
    fn why_unknown_target_is_none() {
        let conn = open_in_memory().unwrap();
        assert!(why(&conn, "does-not-exist", 0.0).unwrap().is_none());
    }

    #[test]
    fn why_target_with_no_edges_is_empty_not_none() {
        let conn = open_in_memory().unwrap();
        add_node(&conn, "entity", "lonely");
        let facts = why(&conn, "lonely", 0.0).unwrap();
        assert_eq!(facts.unwrap().len(), 0);
    }

    #[test]
    fn fts_query_quotes_tokens_and_drops_punctuation() {
        assert_eq!(fts_query("replaybook"), Some("\"replaybook\"".into()));
        assert_eq!(fts_query("foo-bar"), Some("\"foo\" \"bar\"".into()));
        assert_eq!(fts_query("c++"), Some("\"c\"".into()));
        assert_eq!(
            fts_query("a \"quoted\" term"),
            Some("\"a\" \"quoted\" \"term\"".into())
        );
    }

    #[test]
    fn fts_query_none_for_punctuation_only() {
        assert_eq!(fts_query("+++"), None);
        assert_eq!(fts_query("   "), None);
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn search_finds_matching_note() {
        let conn = open_in_memory().unwrap();
        add_note(
            &conn,
            "a.md",
            Some("2026-07-01"),
            "notes about replaybook today",
        );
        add_note(&conn, "b.md", Some("2026-07-02"), "unrelated grocery list");
        rebuild_fts(&conn);

        let hits = search(&conn, "replaybook", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.md");
        assert!(hits[0].snippet.to_lowercase().contains("replaybook"));
    }

    #[test]
    fn search_is_token_based_not_substring() {
        // A LIKE '%cargo%' scan would match "cargofoo"; FTS tokenizes,
        // so a bare word query must not.
        let conn = open_in_memory().unwrap();
        add_note(&conn, "a.md", None, "the cargofoo wrapper");
        add_note(&conn, "b.md", None, "run cargo build");
        rebuild_fts(&conn);

        let hits = search(&conn, "cargo", 10).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md"]);
    }

    #[test]
    fn search_ranks_by_relevance() {
        let conn = open_in_memory().unwrap();
        add_note(&conn, "dense.md", None, "raft raft raft raft everywhere");
        add_note(
            &conn,
            "sparse.md",
            None,
            "one mention of raft among much other prose here",
        );
        rebuild_fts(&conn);

        let hits = search(&conn, "raft", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // bm25 favors the denser, shorter document.
        assert_eq!(hits[0].path, "dense.md");
    }

    #[test]
    fn search_multi_word_requires_all_terms() {
        let conn = open_in_memory().unwrap();
        add_note(&conn, "both.md", None, "the czechia visa paperwork");
        add_note(&conn, "one.md", None, "the visa office was closed");
        rebuild_fts(&conn);

        let hits = search(&conn, "czechia visa", 10).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["both.md"]);
    }

    #[test]
    fn search_punctuation_only_term_returns_empty() {
        let conn = open_in_memory().unwrap();
        add_note(&conn, "a.md", None, "anything at all");
        rebuild_fts(&conn);

        assert!(search(&conn, "+++", 10).unwrap().is_empty());
    }

    #[test]
    fn search_respects_limit() {
        let conn = open_in_memory().unwrap();
        for i in 0..5 {
            add_note(&conn, &format!("n{i}.md"), None, "shared keyword here");
        }
        rebuild_fts(&conn);

        assert_eq!(search(&conn, "keyword", 3).unwrap().len(), 3);
    }
}
