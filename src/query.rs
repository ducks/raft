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
    let pattern = format!("%{}%", term.to_lowercase());
    let mut stmt = conn.prepare(
        "SELECT nodes.path, notes.note_date, notes.body
         FROM notes JOIN nodes ON nodes.id = notes.node_id
         WHERE lower(notes.body) LIKE ?1
         ORDER BY notes.note_date DESC NULLS LAST
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![pattern, limit as i64], |row| {
        let path: String = row.get(0)?;
        let note_date: Option<String> = row.get(1)?;
        let body: String = row.get(2)?;
        Ok((path, note_date, body))
    })?;

    let term_lower = term.to_lowercase();
    let mut hits = Vec::new();
    for row in rows {
        let (path, note_date, body) = row?;
        hits.push(SearchHit {
            path,
            note_date,
            snippet: snippet_around(&body, &term_lower),
        });
    }
    Ok(hits)
}

fn snippet_around(body: &str, term_lower: &str) -> String {
    let lower = body.to_lowercase();
    let Some(pos) = lower.find(term_lower) else {
        return String::new();
    };
    let start = body[..pos]
        .char_indices()
        .rev()
        .nth(60)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = body[pos..]
        .char_indices()
        .nth(term_lower.len() + 60)
        .map(|(i, _)| pos + i)
        .unwrap_or(body.len());
    body[start..end].replace('\n', " ").trim().to_string()
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

pub fn about(conn: &Connection, name: &str) -> Result<Option<About>> {
    let node: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, kind, meta FROM nodes
             WHERE lower(name) = lower(?1) AND kind IN ('project', 'entity')
             ORDER BY CASE kind WHEN 'project' THEN 0 ELSE 1 END
             LIMIT 1",
            params![name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;

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
        "SELECT nodes.path, notes.note_date
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
            let id = conn
                .query_row(
                    "SELECT id FROM nodes
                     WHERE lower(name) = lower(?1) AND kind IN ('project', 'entity')
                     ORDER BY CASE kind WHEN 'project' THEN 0 ELSE 1 END
                     LIMIT 1",
                    params![name],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
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
