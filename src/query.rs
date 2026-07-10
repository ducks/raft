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
             WHERE name = ?1 AND kind IN ('project', 'entity')
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
