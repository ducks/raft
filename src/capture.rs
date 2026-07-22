//! Write commands: the only two places raft touches source notes.
//! Files stay the source of truth; the index is rebuilt afterwards.

use crate::config::{self, Config};
use crate::extract;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Append a timestamped entry to today's daily note (created if
/// missing) under a `## Log` section. Returns the note path.
pub fn append_log(config: &Config, text: &str) -> Result<PathBuf> {
    let template = config
        .daily_note
        .as_deref()
        .context("no daily_note in config; set e.g. daily_note = \"~/notes/%Y/%Y-%m-%d.md\"")?;

    let now = chrono::Local::now();
    let path_str = now
        .format(&config::expand_tilde(template).to_string_lossy())
        .to_string();
    let path = PathBuf::from(path_str);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        format!("# {}\n", now.format("%Y-%m-%d"))
    };

    let has_log_section = existing.contains("\n## Log") || existing.starts_with("## Log");

    // Normalize to a known state: no trailing whitespace to reason about.
    let mut body = existing.trim_end().to_string();

    if !has_log_section {
        body.push_str("\n\n## Log");
    }
    body.push_str(&format!("\n- {} {}\n", now.format("%H:%M"), text));

    std::fs::write(&path, body)?;
    Ok(path)
}

/// Mark an open loop done in every note that contains it: checkboxes
/// flip to `[x]`, plain follow-up bullets get a `(done)` marker (which
/// the extractor treats as closed). Returns the files changed.
pub fn mark_done(loop_text: &str, note_paths: &[String]) -> Result<Vec<String>> {
    let mut changed = Vec::new();

    for note_path in note_paths {
        let body = std::fs::read_to_string(note_path)
            .with_context(|| format!("could not read {note_path}"))?;

        // Re-extract from the live file: the index may be stale, and
        // line numbers are only trustworthy from the current content.
        let Some(target) = extract::extract_loops(&body)
            .into_iter()
            .find(|l| l.text == loop_text)
        else {
            continue; // already closed or edited away since indexing
        };

        let mut lines: Vec<String> = body.lines().map(String::from).collect();
        let line = &mut lines[target.line];
        if line.contains("[ ]") {
            *line = line.replacen("[ ]", "[x]", 1);
        } else {
            line.push_str(" (done)");
        }

        let mut new_body = lines.join("\n");
        if body.ends_with('\n') {
            new_body.push('\n');
        }
        std::fs::write(note_path, new_body)?;
        changed.push(note_path.clone());
    }

    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_daily(template: &str) -> Config {
        Config {
            sources: Vec::new(),
            ignore: Vec::new(),
            daily_note: Some(template.to_string()),
            publish: Default::default(),
        }
    }

    #[test]
    fn log_creates_note_then_appends_under_one_header() {
        let dir = tempfile::tempdir().unwrap();
        let template = format!("{}/%Y-%m-%d.md", dir.path().to_string_lossy());
        let config = config_with_daily(&template);

        let path = append_log(&config, "first entry").unwrap();
        append_log(&config, "second entry").unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.matches("## Log").count(), 1);
        assert!(body.contains("first entry"));
        assert!(body.contains("second entry"));
        // first entry must come before second (append order preserved)
        assert!(body.find("first entry") < body.find("second entry"));
        // no blank line between consecutive entries
        assert!(!body.contains("first entry\n\n"));
    }

    #[test]
    fn marks_checkbox_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, "# note\n\n- [ ] fix the thing\n- [ ] other task\n").unwrap();

        let changed = mark_done("fix the thing", &[path.to_string_lossy().to_string()]).unwrap();

        assert_eq!(changed.len(), 1);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("- [x] fix the thing"));
        assert!(body.contains("- [ ] other task"));
    }

    #[test]
    fn marks_followup_bullet_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(
            &path,
            "# note\n\n## Follow-ups\n\n- unpin try after vacation\n- roll the container\n",
        )
        .unwrap();

        let changed = mark_done(
            "unpin try after vacation",
            &[path.to_string_lossy().to_string()],
        )
        .unwrap();

        assert_eq!(changed.len(), 1);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("- unpin try after vacation (done)"));
        assert!(body.contains("- roll the container\n"));
        // Round-trip: the extractor no longer sees it as open.
        let still_open = extract::extract_loops(&body);
        assert!(!still_open.iter().any(|l| l.text.starts_with("unpin try")));
        assert!(still_open.iter().any(|l| l.text == "roll the container"));
    }

    #[test]
    fn skips_notes_where_loop_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, "# note\n\nnothing here\n").unwrap();

        let changed = mark_done("vanished loop", &[path.to_string_lossy().to_string()]).unwrap();
        assert!(changed.is_empty());
    }
}
