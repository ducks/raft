//! Enforce the strict note format (reference/note-format.md in the
//! notes corpus). Every rule here is a contract the extractor and the
//! publish pipeline rely on; lint failing means extraction is guessing.

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Serialize)]
pub struct LintIssue {
    pub path: String,
    /// 1-based line number, 0 for whole-file issues.
    pub line: usize,
    pub rule: &'static str,
    pub message: String,
}

/// A line that is exactly a TOML array-of-tables header (`[[bin]]`,
/// `[[categories.projects]]`). Outside a fence this is indistinguishable
/// from a wiki link and would mint a false human-provenance edge.
fn toml_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[\[[a-z0-9_.-]+\]\]\s*$").unwrap())
}

/// Checkbox syntax close enough to be intended but wrong enough to be
/// invisible to the extractor: `- []`, `-[ ]`, `- [  ]`.
fn malformed_checkbox_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s*\[( {2,}|)\]|^\s*[-*]\[").unwrap())
}

fn daily_stem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap())
}

/// Lint one note body. `stem` is the filename without extension, used
/// for the daily H1 rule.
pub fn lint_note(path: &Path, body: &str) -> Vec<LintIssue> {
    let path_str = path.to_string_lossy().to_string();
    let mut issues = Vec::new();
    let issue = |line: usize, rule: &'static str, message: String| LintIssue {
        path: path_str.clone(),
        line,
        rule,
        message,
    };

    // --- frontmatter: present, closed, explicit boolean publish key ---
    let mut fm_end_line = 0usize; // 1-based line of the closing ---
    if !(body.starts_with("---\n") || body.starts_with("---\r\n")) {
        issues.push(issue(
            1,
            "frontmatter-missing",
            "note must start with a closed frontmatter block declaring `publish:`".into(),
        ));
    } else {
        let mut publish_seen = false;
        let mut closed = false;
        for (idx, line) in body.lines().enumerate().skip(1) {
            if line.trim_end() == "---" {
                fm_end_line = idx + 1;
                closed = true;
                break;
            }
            let mut parts = line.splitn(2, ':');
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if key == "publish" {
                publish_seen = true;
                if value != "true" && value != "false" {
                    issues.push(issue(
                        idx + 1,
                        "publish-not-boolean",
                        format!("`publish: {value}` - must be literally true or false"),
                    ));
                }
            }
        }
        if !closed {
            issues.push(issue(
                1,
                "frontmatter-unterminated",
                "frontmatter block never closes with `---`".into(),
            ));
        } else if !publish_seen {
            issues.push(issue(
                1,
                "publish-missing",
                "frontmatter must declare `publish: true` or `publish: false`".into(),
            ));
        }
    }

    // --- daily H1 must match the filename date ---
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if daily_stem_re().is_match(stem) {
        let h1 = body
            .lines()
            .enumerate()
            .skip(fm_end_line)
            .find(|(_, l)| l.starts_with("# "));
        match h1 {
            Some((idx, l)) if l.trim() != format!("# {stem}") => issues.push(issue(
                idx + 1,
                "daily-h1-mismatch",
                format!("daily note H1 is '{}', expected '# {stem}'", l.trim()),
            )),
            None => issues.push(issue(
                0,
                "daily-h1-missing",
                format!("daily note has no `# {stem}` heading"),
            )),
            _ => {}
        }
    }

    // --- line rules, fence-aware ---
    let mut in_fence = false;
    for (idx, line) in body.lines().enumerate().skip(fm_end_line) {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if toml_table_re().is_match(line) {
            issues.push(issue(
                idx + 1,
                "unfenced-config",
                format!(
                    "'{}' looks like a TOML table header, which parses as a wiki \
                     link; fence the config snippet",
                    line.trim()
                ),
            ));
        }
        if malformed_checkbox_re().is_match(line) {
            issues.push(issue(
                idx + 1,
                "malformed-checkbox",
                format!(
                    "'{}' is almost a checkbox; the loop syntax is `- [ ]`",
                    line.trim()
                ),
            ));
        }
    }
    if in_fence {
        issues.push(issue(
            0,
            "unterminated-fence",
            "odd number of ``` fences; the rest of the note is swallowed during extraction".into(),
        ));
    }

    issues
}

/// Lint every note under the configured notes sources. Returns all
/// issues, ordered by path then line.
pub fn lint_all(cfg: &crate::config::Config) -> Result<Vec<LintIssue>> {
    let mut issues = Vec::new();
    for source in &cfg.sources {
        if source.kind != crate::config::SourceKind::Notes {
            continue;
        }
        let root = crate::config::expand_tilde(&source.path);
        for note in crate::scan::scan_notes(&root)? {
            issues.extend(lint_note(&note.path, &note.body));
        }
    }
    issues.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lint(name: &str, body: &str) -> Vec<LintIssue> {
        lint_note(&PathBuf::from(name), body)
    }

    fn rules(issues: &[LintIssue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.rule).collect()
    }

    const GOOD_DAILY: &str = "---\npublish: false\n---\n# 2026-07-23\n\n## raft: things\n\nprose `code` here.\n\n- [ ] a loop\n";

    #[test]
    fn clean_daily_passes() {
        assert!(lint("2026-07-23.md", GOOD_DAILY).is_empty());
    }

    #[test]
    fn missing_frontmatter_flags() {
        let issues = lint("2026-07-23.md", "# 2026-07-23\nprose\n");
        assert!(rules(&issues).contains(&"frontmatter-missing"));
    }

    #[test]
    fn unterminated_frontmatter_flags() {
        let issues = lint("a.md", "---\npublish: false\nbody\n");
        assert_eq!(rules(&issues), vec!["frontmatter-unterminated"]);
    }

    #[test]
    fn missing_publish_key_flags() {
        let issues = lint("a.md", "---\ntitle: x\n---\nbody\n");
        assert_eq!(rules(&issues), vec!["publish-missing"]);
    }

    #[test]
    fn non_boolean_publish_flags() {
        let issues = lint("a.md", "---\npublish: yes\n---\nbody\n");
        assert_eq!(rules(&issues), vec!["publish-not-boolean"]);
    }

    #[test]
    fn daily_h1_must_match_filename() {
        let issues = lint("2026-07-23.md", "---\npublish: false\n---\n# 2026-07-22\n");
        assert_eq!(rules(&issues), vec!["daily-h1-mismatch"]);

        let issues = lint("2026-07-23.md", "---\npublish: false\n---\nno heading\n");
        assert_eq!(rules(&issues), vec!["daily-h1-missing"]);
    }

    #[test]
    fn topic_notes_need_no_h1_rule() {
        let issues = lint(
            "raft/publish-design.md",
            "---\npublish: false\n---\nany shape\n",
        );
        assert!(issues.is_empty());
    }

    #[test]
    fn unfenced_toml_table_flags() {
        let body = "---\npublish: false\n---\n# 2026-07-23\n\n[[bin]]\nname = \"raft\"\n";
        let issues = lint("2026-07-23.md", body);
        assert!(rules(&issues).contains(&"unfenced-config"));
    }

    #[test]
    fn fenced_toml_table_is_fine() {
        let body = "---\npublish: false\n---\n# 2026-07-23\n\n```toml\n[[bin]]\n```\n";
        assert!(lint("2026-07-23.md", body).is_empty());
    }

    #[test]
    fn real_wiki_link_line_is_not_config() {
        // Wiki links to real things have capitals or spaces; and inline
        // links are never just the link on a line.
        let body = "---\npublish: false\n---\nsee [[Replaybook]] today\n";
        assert!(lint("a.md", body).is_empty());
    }

    #[test]
    fn malformed_checkboxes_flag() {
        for bad in ["- [] x\n", "-[ ] x\n", "- [  ] x\n"] {
            let body = format!("---\npublish: false\n---\n{bad}");
            let issues = lint("a.md", &body);
            assert_eq!(rules(&issues), vec!["malformed-checkbox"], "{bad:?}");
        }
    }

    #[test]
    fn unterminated_fence_flags() {
        let body = "---\npublish: false\n---\nkeep\n```\nswallowed\n";
        let issues = lint("a.md", body);
        assert!(rules(&issues).contains(&"unterminated-fence"));
    }
}
