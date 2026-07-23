//! The publish kernel: compute exactly what would go public, before
//! anything renders.
//!
//! `PublishPlan` is a serializable manifest of every node and edge that
//! would appear on the published site. The privacy model:
//!
//! - Notes are opt-in per node, forever: only a note whose frontmatter
//!   says `publish: true` is included. Default private.
//! - Projects require an explicit allowlist in config (`[publish]
//!   repos`). A public remote is not consent; commit messages leak.
//! - Loops follow their containing note.
//! - Entities appear only when a public note or public project links to
//!   them. Symbol entities (which carry a file path in a repo) also
//!   require their repo to be allowlisted, or they are dropped entirely.
//! - Edges render only when BOTH endpoints are public. Private
//!   neighbors leave no trace - not even counts.
//!
//! The plan is deterministic: same index + same config = byte-identical
//! manifest JSON, so a publish can be reviewed as a diff.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::PublishConfig;
use crate::extract;

/// A note that would be published.
#[derive(Debug, Serialize)]
pub struct PlanNote {
    /// Full path, the note's node name in the index.
    pub path: String,
    pub note_date: Option<String>,
    /// Body text, available to the emitter but kept out of the
    /// serialized manifest so audits stay reviewable as diffs.
    #[serde(skip)]
    pub body: String,
}

/// A project that would be published.
#[derive(Debug, Serialize)]
pub struct PlanProject {
    pub name: String,
    /// Git metadata JSON as stored in the index (branch, recent commits).
    pub git: serde_json::Value,
}

/// An entity that would appear because public content references it.
#[derive(Debug, Serialize)]
pub struct PlanEntity {
    pub name: String,
    /// Display spelling or symbol definition metadata.
    pub meta: serde_json::Value,
}

/// An open loop carried along with its published note.
#[derive(Debug, Serialize)]
pub struct PlanLoop {
    /// Loop identity, `<note path>#loop-<ordinal>`.
    pub name: String,
    pub note_path: String,
    pub text: String,
    pub section: Option<String>,
}

/// An edge where both endpoints are public.
#[derive(Debug, Serialize)]
pub struct PlanEdge {
    pub src_kind: String,
    pub src_name: String,
    pub dst_kind: String,
    pub dst_name: String,
    pub kind: String,
    pub provenance: String,
    pub weight: f64,
    pub rationale: Option<String>,
}

/// Something the audit wants a human to look at before publishing.
#[derive(Debug, Serialize)]
pub struct PlanFlag {
    pub note_path: String,
    /// The wiki-link target as written in the public note's prose.
    pub target: String,
    pub reason: String,
}

/// The complete manifest of what `raft publish` would make public.
#[derive(Debug, Serialize)]
pub struct PublishPlan {
    pub notes: Vec<PlanNote>,
    pub projects: Vec<PlanProject>,
    pub entities: Vec<PlanEntity>,
    pub loops: Vec<PlanLoop>,
    pub edges: Vec<PlanEdge>,
    /// Wiki links in published prose whose targets are not public. The
    /// link text itself would still be visible on the site, so these
    /// need a human decision before emit.
    pub flags: Vec<PlanFlag>,
    /// Edges held back because exactly one endpoint is public. Owner-
    /// facing audit information only: it tells you how many connections
    /// the site will silently lack. Never serialized - the manifest is
    /// the public artifact and must carry no shadow of private nodes.
    #[serde(skip)]
    pub dropped_edges: usize,
}

/// True if the note body opts into publishing: a leading `---`
/// frontmatter block containing a `publish: true` line. Only the
/// frontmatter counts - the phrase appearing in prose does nothing.
fn is_published(body: &str) -> bool {
    let mut lines = body.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    let mut flag = false;
    for line in lines {
        if line.trim_end() == "---" {
            return flag; // only a closed frontmatter block counts
        }
        let mut parts = line.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if key == "publish" {
            flag = value == "true";
        }
    }
    false // unterminated frontmatter: treat as no frontmatter
}

/// Compute the publish manifest from the live index. Read-only and
/// deterministic: results are ordered by (kind, name) throughout.
pub fn plan(conn: &Connection, cfg: &PublishConfig) -> Result<PublishPlan> {
    // Node id -> (kind, name) for everything public, built up in stages.
    let mut public: BTreeMap<i64, (String, String)> = BTreeMap::new();

    // Stage 1: notes that opt in via frontmatter.
    let mut notes = Vec::new();
    let mut note_paths: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.name, no.body, no.note_date
             FROM nodes n JOIN notes no ON no.node_id = n.id
             WHERE n.kind = 'note' ORDER BY n.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, path, body, note_date) = row?;
            if !is_published(&body) {
                continue;
            }
            public.insert(id, ("note".into(), path.clone()));
            note_paths.insert(path.clone());
            notes.push(PlanNote {
                path,
                note_date,
                body,
            });
        }
    }

    // Stage 2: projects on the explicit allowlist.
    let allowed: HashSet<String> = cfg.repos.iter().map(|r| extract::canonicalize(r)).collect();
    let mut projects = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT id, name, meta FROM nodes WHERE kind = 'project' ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, name, meta) = row?;
            if !allowed.contains(&extract::canonicalize(&name)) {
                continue;
            }
            let mut git: serde_json::Value =
                serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
            // The fingerprint is an implementation detail of the git
            // cache, not something to publish.
            if let Some(obj) = git.as_object_mut() {
                obj.remove("fingerprint");
            }
            public.insert(id, ("project".into(), name.clone()));
            projects.push(PlanProject { name, git });
        }
    }

    // Stage 3: loops follow their containing note.
    let mut loops = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, name, path, meta FROM nodes WHERE kind = 'loop' ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, name, path, meta) = row?;
            let Some(path) = path else { continue };
            if !note_paths.contains(&path) {
                continue;
            }
            let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or_default();
            public.insert(id, ("loop".into(), name.clone()));
            loops.push(PlanLoop {
                name,
                note_path: path,
                text: meta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                section: meta
                    .get("section")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
    }

    // Stage 4: entities referenced by something already public. Symbol
    // entities (meta carries a repo) additionally require their repo on
    // the allowlist: a public note mentioning a private repo's class
    // must not drag that repo's file paths onto the site.
    let mut entities = Vec::new();
    {
        let seed_ids: HashSet<i64> = public.keys().copied().collect();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.id, e.name, e.meta
             FROM nodes e
             JOIN edges ed ON e.id IN (ed.src, ed.dst)
             WHERE e.kind = 'entity' ORDER BY e.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        // An entity is referenced-by-public if any edge touches it from
        // a seed node. Collect edge endpoints once.
        let mut touches: HashMap<i64, bool> = HashMap::new();
        {
            let mut estmt = conn.prepare("SELECT src, dst FROM edges")?;
            let erows =
                estmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            for erow in erows {
                let (src, dst) = erow?;
                if seed_ids.contains(&src) {
                    *touches.entry(dst).or_default() |= true;
                }
                if seed_ids.contains(&dst) {
                    *touches.entry(src).or_default() |= true;
                }
            }
        }
        for row in rows {
            let (id, name, meta) = row?;
            if !touches.get(&id).copied().unwrap_or(false) {
                continue;
            }
            let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or_default();
            if let Some(repo) = meta.get("repo").and_then(|v| v.as_str()) {
                if !allowed.contains(&extract::canonicalize(repo)) {
                    continue;
                }
            }
            public.insert(id, ("entity".into(), name.clone()));
            entities.push(PlanEntity { name, meta });
        }
    }

    // Stage 5: edges render only when both endpoints are public.
    let mut edges = Vec::new();
    let mut dropped_edges = 0usize;
    {
        let mut stmt = conn.prepare(
            "SELECT src, dst, kind, provenance, weight, rationale
             FROM edges ORDER BY src, dst, kind, provenance",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (src, dst, kind, provenance, weight, rationale) = row?;
            let (Some((src_kind, src_name)), Some((dst_kind, dst_name))) =
                (public.get(&src), public.get(&dst))
            else {
                // Held back because at least one endpoint is public but
                // the other is not: the owner-facing audit reports how
                // many backlinks the site will silently lack. Aggregate
                // count only, and never serialized - the manifest stays
                // shadow-free.
                if public.contains_key(&src) || public.contains_key(&dst) {
                    dropped_edges += 1;
                }
                continue;
            };
            edges.push(PlanEdge {
                src_kind: src_kind.clone(),
                src_name: src_name.clone(),
                dst_kind: dst_kind.clone(),
                dst_name: dst_name.clone(),
                kind,
                provenance,
                weight,
                rationale,
            });
        }
        edges.sort_by(|a, b| {
            (&a.src_kind, &a.src_name, &a.dst_kind, &a.dst_name, &a.kind).cmp(&(
                &b.src_kind,
                &b.src_name,
                &b.dst_kind,
                &b.dst_name,
                &b.kind,
            ))
        });
    }

    // Stage 6: audit flags. A wiki link written in published prose
    // renders its target's name even if the edge is dropped, so it
    // needs a human decision (rewrite the prose, publish the target,
    // or accept it) unless the target is an allowlisted project. Only
    // projects suppress a flag: an entity in the public set is derived
    // from the very prose being audited (the link itself creates it),
    // so its presence is not evidence of consent - and treating it as
    // consent silently publishes private note titles as entity pages.
    let public_projects: HashSet<String> = projects
        .iter()
        .map(|p| extract::canonicalize(&p.name))
        .collect();
    let mut flags = Vec::new();
    for note in &notes {
        let extraction = extract::extract(&note.body, &HashSet::new());
        for target in extraction.wiki_links {
            if !public_projects.contains(&extract::canonicalize(&target)) {
                flags.push(PlanFlag {
                    note_path: note.path.clone(),
                    target: target.clone(),
                    reason: format!(
                        "published prose links [[{target}]], whose target is not \
                         a public project; the name itself will be visible"
                    ),
                });
            }
        }
    }
    flags.sort_by(|a, b| (&a.note_path, &a.target).cmp(&(&b.note_path, &b.target)));

    Ok(PublishPlan {
        notes,
        projects,
        entities,
        loops,
        edges,
        flags,
        dropped_edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::open_in_memory;
    use rusqlite::params;

    fn insert_note(conn: &Connection, path: &str, body: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('note', ?1, ?1, '{}')",
            params![path],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO notes (node_id, body, note_date, mtime) VALUES (?1, ?2, '2026-07-01', NULL)",
            params![id, body],
        )
        .unwrap();
        id
    }

    fn insert_project(conn: &Connection, name: &str, meta: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('project', ?1, ?1, ?2)",
            params![name, meta],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_entity(conn: &Connection, name: &str, meta: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (kind, name, meta) VALUES ('entity', ?1, ?2)",
            params![name, meta],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_loop(conn: &Connection, note_path: &str, ordinal: usize, text: &str) -> i64 {
        let identity = format!("{note_path}#loop-{ordinal}");
        let meta = serde_json::json!({ "text": text, "section": null }).to_string();
        conn.execute(
            "INSERT INTO nodes (kind, name, path, meta) VALUES ('loop', ?1, ?2, ?3)",
            params![identity, note_path, meta],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_edge(conn: &Connection, src: i64, dst: i64, kind: &str) {
        conn.execute(
            "INSERT INTO edges (src, dst, kind, provenance, weight, rationale)
             VALUES (?1, ?2, ?3, 'indexer', 1.0, 'test edge')",
            params![src, dst, kind],
        )
        .unwrap();
    }

    fn cfg(repos: &[&str]) -> PublishConfig {
        PublishConfig {
            repos: repos.iter().map(|s| s.to_string()).collect(),
        }
    }

    const PUBLIC_BODY: &str = "---\npublish: true\n---\n# Hello\nprose\n";

    // --- opt-in ----------------------------------------------------------

    #[test]
    fn note_without_flag_is_private() {
        let conn = open_in_memory().unwrap();
        insert_note(&conn, "/n/private.md", "# No frontmatter\n");
        insert_note(&conn, "/n/public.md", PUBLIC_BODY);

        let plan = plan(&conn, &cfg(&[])).unwrap();

        assert_eq!(plan.notes.len(), 1);
        assert_eq!(plan.notes[0].path, "/n/public.md");
    }

    #[test]
    fn publish_true_in_prose_does_not_publish() {
        let conn = open_in_memory().unwrap();
        insert_note(&conn, "/n/a.md", "# Note\npublish: true\n");
        let plan = plan(&conn, &cfg(&[])).unwrap();
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn publish_false_and_other_frontmatter_stay_private() {
        let conn = open_in_memory().unwrap();
        insert_note(&conn, "/n/a.md", "---\npublish: false\n---\nbody\n");
        insert_note(&conn, "/n/b.md", "---\ntitle: x\n---\nbody\n");
        insert_note(&conn, "/n/c.md", "---\ntitle: x\npublish: true\n"); // unterminated
        let plan = plan(&conn, &cfg(&[])).unwrap();
        assert!(plan.notes.is_empty());
    }

    // --- repo allowlist --------------------------------------------------

    #[test]
    fn project_needs_explicit_allowlist() {
        let conn = open_in_memory().unwrap();
        insert_project(&conn, "raft", "{}");
        insert_project(&conn, "secret-repo", "{}");

        let plan = plan(&conn, &cfg(&["raft"])).unwrap();

        let names: Vec<&str> = plan.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["raft"]);
    }

    #[test]
    fn project_git_fingerprint_is_stripped() {
        let conn = open_in_memory().unwrap();
        insert_project(
            &conn,
            "raft",
            r#"{"branch":"main","commits":[],"fingerprint":"12345"}"#,
        );
        let plan = plan(&conn, &cfg(&["raft"])).unwrap();
        assert!(plan.projects[0].git.get("fingerprint").is_none());
        assert_eq!(
            plan.projects[0].git.get("branch").and_then(|b| b.as_str()),
            Some("main")
        );
    }

    // --- edges: both endpoints or nothing --------------------------------

    #[test]
    fn edge_to_private_endpoint_is_dropped_without_trace() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let priv_note = insert_note(&conn, "/n/private.md", "# Private\n");
        let pub_proj = insert_project(&conn, "raft", "{}");
        let priv_proj = insert_project(&conn, "secret", "{}");
        insert_edge(&conn, pub_note, pub_proj, "mentions");
        insert_edge(&conn, pub_note, priv_proj, "mentions");
        insert_edge(&conn, priv_note, pub_proj, "mentions");

        let plan = plan(&conn, &cfg(&["raft"])).unwrap();

        assert_eq!(plan.edges.len(), 1);
        assert_eq!(plan.edges[0].src_name, "/n/public.md");
        assert_eq!(plan.edges[0].dst_name, "raft");
        // No shadow: nothing anywhere in the manifest names the private
        // side or counts it.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("private.md"));
        assert!(!json.contains("secret"));
    }

    // --- loops follow their note -----------------------------------------

    #[test]
    fn loop_follows_containing_note() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        insert_note(&conn, "/n/private.md", "# Private\n- [ ] secret task\n");
        let pub_loop = insert_loop(&conn, "/n/public.md", 0, "ship the garden");
        let priv_loop = insert_loop(&conn, "/n/private.md", 0, "secret task");
        insert_edge(&conn, pub_note, pub_loop, "contains");
        let _ = priv_loop;

        let plan = plan(&conn, &cfg(&[])).unwrap();

        assert_eq!(plan.loops.len(), 1);
        assert_eq!(plan.loops[0].text, "ship the garden");
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("secret task"));
    }

    // --- entities --------------------------------------------------------

    #[test]
    fn entity_included_only_when_public_content_references_it() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let priv_note = insert_note(&conn, "/n/private.md", "# Private\n");
        let seen = insert_entity(&conn, "zola", r#"{"display":"zola"}"#);
        let unseen = insert_entity(&conn, "visa-officer", r#"{"display":"visa-officer"}"#);
        insert_edge(&conn, pub_note, seen, "mentions");
        insert_edge(&conn, priv_note, unseen, "mentions");

        let plan = plan(&conn, &cfg(&[])).unwrap();

        let names: Vec<&str> = plan.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zola"]);
        assert!(!serde_json::to_string(&plan)
            .unwrap()
            .contains("visa-officer"));
    }

    #[test]
    fn symbol_entity_requires_allowlisted_repo() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let sym = insert_entity(
            &conn,
            "SecretJob",
            r#"{"file":"app/jobs/secret_job.rb","repo":"work-repo","lang":"ruby"}"#,
        );
        insert_edge(&conn, pub_note, sym, "mentions");

        // Mentioned from public prose, but its repo is not allowlisted:
        // the entity (and its file path) must not appear.
        let denied = plan(&conn, &cfg(&[])).unwrap();
        assert!(denied.entities.is_empty());
        assert!(!serde_json::to_string(&denied)
            .unwrap()
            .contains("secret_job"));

        // Allowlisting the repo admits it.
        insert_project(&conn, "work-repo", "{}");
        let allowed = plan(&conn, &cfg(&["work-repo"])).unwrap();
        assert_eq!(allowed.entities.len(), 1);
    }

    // --- flags -----------------------------------------------------------

    #[test]
    fn wiki_link_to_private_target_is_flagged() {
        let conn = open_in_memory().unwrap();
        let body = "---\npublish: true\n---\nsee [[Visa Plan]] and [[raft]]\n";
        insert_note(&conn, "/n/public.md", body);
        insert_project(&conn, "raft", "{}");

        let plan = plan(&conn, &cfg(&["raft"])).unwrap();

        assert_eq!(plan.flags.len(), 1);
        assert_eq!(plan.flags[0].target, "Visa Plan");
    }

    #[test]
    fn entity_mirroring_private_note_title_still_flags() {
        // Regression for the closure hole found dogfooding: a wiki link
        // to a private note materializes an entity with the note's
        // title; that entity going public must not suppress the flag,
        // or private titles publish with zero review.
        let conn = open_in_memory().unwrap();
        let body = "---\npublish: true\n---\nsee [[secret plan]]\n";
        let note = insert_note(&conn, "/n/public.md", body);
        insert_note(&conn, "/n/secret-plan.md", "# secret plan\nprivate\n");
        let entity = insert_entity(&conn, "secret plan", r#"{"display":"secret plan"}"#);
        insert_edge(&conn, note, entity, "wikilink");

        let plan = plan(&conn, &cfg(&[])).unwrap();

        // The entity is in the manifest (its name is already in public
        // prose), but the flag must fire so a human decides before emit.
        assert_eq!(plan.entities.len(), 1);
        assert_eq!(plan.flags.len(), 1);
        assert_eq!(plan.flags[0].target, "secret plan");
    }

    // --- dropped-edge accounting ------------------------------------------

    #[test]
    fn dropped_edges_counts_half_public_only() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let priv_a = insert_note(&conn, "/n/priv-a.md", "# A\n");
        let priv_b = insert_note(&conn, "/n/priv-b.md", "# B\n");
        let pub_proj = insert_project(&conn, "raft", "{}");
        let priv_proj = insert_project(&conn, "secret", "{}");
        insert_edge(&conn, pub_note, pub_proj, "mentions"); // kept
        insert_edge(&conn, pub_note, priv_proj, "mentions"); // dropped: dst private
        insert_edge(&conn, priv_a, pub_proj, "mentions"); // dropped: src private
        insert_edge(&conn, priv_a, priv_b, "mentions"); // fully private: not counted

        let plan = plan(&conn, &cfg(&["raft"])).unwrap();

        assert_eq!(plan.edges.len(), 1);
        assert_eq!(plan.dropped_edges, 2);
    }

    #[test]
    fn dropped_edges_never_serialized() {
        let conn = open_in_memory().unwrap();
        let pub_note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let priv_proj = insert_project(&conn, "secret", "{}");
        insert_edge(&conn, pub_note, priv_proj, "mentions");

        let plan = plan(&conn, &cfg(&[])).unwrap();

        assert_eq!(plan.dropped_edges, 1);
        // The manifest JSON is the public artifact: no field, no count.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("dropped"));
        assert!(!json.contains("secret"));
    }

    // --- determinism -----------------------------------------------------

    #[test]
    fn plan_is_deterministic() {
        let conn = open_in_memory().unwrap();
        let n1 = insert_note(&conn, "/n/b.md", PUBLIC_BODY);
        let n2 = insert_note(&conn, "/n/a.md", PUBLIC_BODY);
        let p = insert_project(&conn, "raft", "{}");
        insert_edge(&conn, n1, p, "mentions");
        insert_edge(&conn, n2, p, "mentions");

        let a = serde_json::to_string(&plan(&conn, &cfg(&["raft"])).unwrap()).unwrap();
        let b = serde_json::to_string(&plan(&conn, &cfg(&["raft"])).unwrap()).unwrap();
        assert_eq!(a, b);
        // Ordered by name regardless of insertion order.
        let parsed: serde_json::Value = serde_json::from_str(&a).unwrap();
        let paths: Vec<&str> = parsed["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/n/a.md", "/n/b.md"]);
    }

    #[test]
    fn edges_sort_lexicographically_regardless_of_insertion_order() {
        // Regression: the first comparator mixed a/b fields, so edges
        // differing only in dst could order inconsistently.
        let conn = open_in_memory().unwrap();
        let note = insert_note(&conn, "/n/public.md", PUBLIC_BODY);
        let p_z = insert_project(&conn, "zola", "{}");
        let p_a = insert_project(&conn, "argo", "{}");
        let p_m = insert_project(&conn, "mid", "{}");
        insert_edge(&conn, note, p_z, "mentions");
        insert_edge(&conn, note, p_a, "mentions");
        insert_edge(&conn, note, p_m, "mentions");

        let plan = plan(&conn, &cfg(&["zola", "argo", "mid"])).unwrap();

        let dsts: Vec<&str> = plan.edges.iter().map(|e| e.dst_name.as_str()).collect();
        assert_eq!(dsts, vec!["argo", "mid", "zola"]);
    }

    #[test]
    fn publishing_one_note_changes_only_that_node_and_its_edges() {
        let conn = open_in_memory().unwrap();
        let n1 = insert_note(&conn, "/n/a.md", PUBLIC_BODY);
        let n2 = insert_note(&conn, "/n/b.md", "# Private\n");
        let p = insert_project(&conn, "raft", "{}");
        insert_edge(&conn, n1, p, "mentions");
        insert_edge(&conn, n2, p, "mentions");

        let before = plan(&conn, &cfg(&["raft"])).unwrap();
        assert_eq!(before.notes.len(), 1);
        assert_eq!(before.edges.len(), 1);

        // Flip b.md to published.
        conn.execute(
            "UPDATE notes SET body = ?1 WHERE node_id = ?2",
            params![PUBLIC_BODY, n2],
        )
        .unwrap();

        let after = plan(&conn, &cfg(&["raft"])).unwrap();
        assert_eq!(after.notes.len(), 2);
        assert_eq!(after.edges.len(), 2);
        // The previously public parts are unchanged.
        assert!(after.notes.iter().any(|n| n.path == "/n/a.md"));
        assert!(after
            .edges
            .iter()
            .any(|e| e.src_name == "/n/a.md" && e.dst_name == "raft"));
    }

    // --- is_published unit coverage --------------------------------------

    #[test]
    fn is_published_parses_frontmatter_only() {
        assert!(is_published("---\npublish: true\n---\nbody\n"));
        assert!(is_published("---\ntitle: x\npublish: true\n---\n"));
        assert!(!is_published("publish: true\n")); // no frontmatter block
        assert!(!is_published("---\npublish: false\n---\n"));
        assert!(!is_published("---\npublish: yes\n---\n")); // strict true only
        assert!(!is_published("body\n---\npublish: true\n---\n")); // not leading
        assert!(!is_published(""));
    }
}
