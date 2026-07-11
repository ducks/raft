use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Entities pulled out of one note's text.
#[derive(Debug, Default)]
pub struct Extraction {
    /// [[wiki links]], target names as written.
    pub wiki_links: Vec<String>,
    /// `backticked` spans that look like entities (not code blocks).
    pub code_spans: Vec<String>,
    /// Project names matched against the known-project dictionary,
    /// with mention counts.
    pub project_mentions: HashMap<String, u32>,
}

fn wiki_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]\|#]+)(?:[#\|][^\]]*)?\]\]").unwrap())
}

fn code_span_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([^`\n]{2,80})`").unwrap())
}

pub fn extract(body: &str, project_names: &HashSet<String>) -> Extraction {
    let mut out = Extraction::default();

    // Strip fenced code blocks so their contents don't pollute extraction.
    let prose = strip_fences(body);

    for cap in wiki_link_re().captures_iter(&prose) {
        let target = cap[1].trim().to_string();
        if !target.is_empty() {
            out.wiki_links.push(target);
        }
    }

    for cap in code_span_re().captures_iter(&prose) {
        let span = cap[1].trim();
        if span.is_empty() || span.contains(' ') && span.len() > 40 {
            continue;
        }
        out.code_spans.push(span.to_string());
    }

    // Dictionary matching: project names as whole words in prose or code spans.
    let lower = prose.to_lowercase();
    for name in project_names {
        let mut count = 0u32;
        let needle = name.to_lowercase();
        let mut start = 0;
        while let Some(pos) = lower[start..].find(&needle) {
            let abs = start + pos;
            let before_ok = abs == 0 || !is_word_char(lower.as_bytes()[abs - 1]);
            let end = abs + needle.len();
            let after_ok = end >= lower.len() || !is_word_char(lower.as_bytes()[end]);
            if before_ok && after_ok {
                count += 1;
            }
            start = end;
        }
        if count > 0 {
            out.project_mentions.insert(name.clone(), count);
        }
    }

    // Very short names ("log", "ops") match English words too easily;
    // only trust them when they appear backticked.
    let spans_lower: HashSet<String> = out.code_spans.iter().map(|s| s.to_lowercase()).collect();
    out.project_mentions
        .retain(|name, _| name.len() >= 4 || spans_lower.contains(&name.to_lowercase()));

    out
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Canonical form for entity identity: lowercase, edge punctuation
/// trimmed. `Jolteon`, `jolteon` and `jolteon,` are one entity.
pub fn canonicalize(name: &str) -> String {
    name.trim_matches(|c: char| c.is_whitespace() || ".,:;!?'\"()[]{}".contains(c))
        .to_lowercase()
}

fn strip_fences(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// An open loop: a checkbox or a bullet under a follow-up style header.
#[derive(Debug)]
pub struct Loop {
    pub text: String,
    /// The header the item was found under, if any.
    pub section: Option<String>,
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)$").unwrap())
}

fn followup_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(follow.?ups?|next steps?|todos?|loose ends?|open (?:questions?|threads?|loops?)|\bnext\b)")
            .unwrap()
    })
}

fn checkbox_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s+\[( |x|X)\]\s+(.+)$").unwrap())
}

fn bullet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s+(.+)$").unwrap())
}

/// Extract open loops from a note body. Unchecked checkboxes count
/// anywhere; plain bullets count only under follow-up style headers.
/// Checked boxes are done and skipped. Continuation lines (indented
/// non-bullet text) are folded into the preceding item.
pub fn extract_loops(body: &str) -> Vec<Loop> {
    let mut loops: Vec<Loop> = Vec::new();
    let mut current_header: Option<String> = None;
    let mut in_followup_section = false;
    let mut in_fence = false;
    let mut open_item = false;

    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            open_item = false;
            continue;
        }
        if in_fence {
            continue;
        }

        if let Some(cap) = header_re().captures(line) {
            let title = cap[2].trim().to_string();
            in_followup_section = followup_header_re().is_match(&title);
            current_header = Some(title);
            open_item = false;
            continue;
        }

        if let Some(cap) = checkbox_re().captures(line) {
            open_item = false;
            if &cap[1] == " " {
                loops.push(Loop {
                    text: cap[2].trim().to_string(),
                    section: current_header.clone(),
                });
                open_item = true;
            }
            continue;
        }

        if in_followup_section {
            if let Some(cap) = bullet_re().captures(line) {
                loops.push(Loop {
                    text: cap[1].trim().to_string(),
                    section: current_header.clone(),
                });
                open_item = true;
                continue;
            }
            // Fold indented continuation lines into the open item.
            if open_item && line.starts_with("  ") && !line.trim().is_empty() {
                if let Some(last) = loops.last_mut() {
                    last.text.push(' ');
                    last.text.push_str(line.trim());
                }
                continue;
            }
            open_item = false;
        } else {
            open_item = false;
        }
    }

    // Items hand-marked as finished in prose still show up here;
    // drop the common completion markers.
    loops.retain(|l| {
        let lower = l.text.to_lowercase();
        !l.text.contains('✓')
            && !l.text.contains("~~")
            && !lower.contains("(completed)")
            && !lower.contains("(done)")
            && !lower.starts_with("done:")
    });

    loops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // --- canonicalize -----------------------------------------------------

    #[test]
    fn canonicalize_lowercases_and_trims_edge_punctuation() {
        assert_eq!(canonicalize("Jolteon"), "jolteon");
        assert_eq!(canonicalize("jolteon,"), "jolteon");
        assert_eq!(canonicalize("  (Replaybook)  "), "replaybook");
        assert_eq!(canonicalize("\"quoted\""), "quoted");
    }

    #[test]
    fn canonicalize_keeps_interior_punctuation() {
        // Only edge punctuation is trimmed; interior dots/slashes stay.
        assert_eq!(canonicalize("node.js"), "node.js");
        assert_eq!(canonicalize("a/b"), "a/b");
    }

    #[test]
    fn canonicalize_empty_when_all_punctuation() {
        assert_eq!(canonicalize("..."), "");
        assert_eq!(canonicalize("   "), "");
    }

    // --- strip_fences -----------------------------------------------------

    #[test]
    fn strip_fences_removes_fenced_blocks_and_fence_lines() {
        let body = "before\n```\ncode line\n```\nafter\n";
        let stripped = strip_fences(body);
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
        assert!(!stripped.contains("code line"));
        assert!(!stripped.contains("```"));
    }

    #[test]
    fn strip_fences_handles_language_hint_fences() {
        let body = "text\n```rust\nlet x = 1;\n```\nmore\n";
        let stripped = strip_fences(body);
        assert!(!stripped.contains("let x = 1"));
        assert!(stripped.contains("more"));
    }

    #[test]
    fn strip_fences_unterminated_fence_swallows_rest() {
        // An opening fence with no close drops everything after it.
        let body = "keep\n```\ndropped\nalso dropped\n";
        let stripped = strip_fences(body);
        assert!(stripped.contains("keep"));
        assert!(!stripped.contains("dropped"));
    }

    // --- wiki links -------------------------------------------------------

    #[test]
    fn extract_pulls_wiki_links() {
        let e = extract("see [[On-call training]] and [[Replaybook]]", &names(&[]));
        assert_eq!(e.wiki_links, vec!["On-call training", "Replaybook"]);
    }

    #[test]
    fn extract_wiki_link_strips_anchor_and_alias() {
        // [[target#section]] and [[target|display]] keep only the target.
        let e = extract("[[Note#heading]] [[Target|shown as this]]", &names(&[]));
        assert_eq!(e.wiki_links, vec!["Note", "Target"]);
    }

    #[test]
    fn extract_ignores_wiki_links_inside_fences() {
        let e = extract("real [[Kept]]\n```\n[[Dropped]]\n```\n", &names(&[]));
        assert_eq!(e.wiki_links, vec!["Kept"]);
    }

    // --- code spans -------------------------------------------------------

    #[test]
    fn extract_pulls_short_code_spans() {
        let e = extract("run `cargo` in `raft`", &names(&[]));
        assert!(e.code_spans.contains(&"cargo".to_string()));
        assert!(e.code_spans.contains(&"raft".to_string()));
    }

    #[test]
    fn extract_keeps_spaced_span_under_40_chars() {
        // Precedence: is_empty() || (contains(' ') && len > 40).
        // A spaced span <= 40 chars is kept.
        let e = extract("do `cargo build` now", &names(&[]));
        assert!(e.code_spans.contains(&"cargo build".to_string()));
    }

    #[test]
    fn extract_drops_long_spaced_span() {
        let long = "a very long backticked phrase that exceeds forty chars";
        let e = extract(&format!("`{long}`"), &names(&[]));
        assert!(!e.code_spans.iter().any(|s| s == long));
    }

    // --- project dictionary matching -------------------------------------

    #[test]
    fn extract_matches_project_name_as_whole_word() {
        let e = extract("working on replaybook today", &names(&["replaybook"]));
        assert_eq!(e.project_mentions.get("replaybook"), Some(&1));
    }

    #[test]
    fn extract_counts_repeated_mentions() {
        let e = extract("replaybook and replaybook again", &names(&["replaybook"]));
        assert_eq!(e.project_mentions.get("replaybook"), Some(&2));
    }

    #[test]
    fn extract_respects_word_boundaries() {
        // "arf" must not match inside "scarf" or "arfle".
        let e = extract("a scarf and arfle", &names(&["arf"]));
        assert!(!e.project_mentions.contains_key("arf"));
    }

    #[test]
    fn extract_short_name_only_counts_when_backticked() {
        // Names < 4 chars are dropped unless they appear as a code span.
        let plain = extract("i will go home", &names(&["go"]));
        assert!(!plain.project_mentions.contains_key("go"));

        let backticked = extract("the `go` toolchain", &names(&["go"]));
        assert_eq!(backticked.project_mentions.get("go"), Some(&1));
    }

    #[test]
    fn extract_ignores_project_mentions_in_fences() {
        let e = extract("prose\n```\nreplaybook in code\n```\n", &names(&["replaybook"]));
        assert!(!e.project_mentions.contains_key("replaybook"));
    }

    // --- open loops -------------------------------------------------------

    #[test]
    fn loops_unchecked_boxes_anywhere() {
        let body = "# Random\n- [ ] fix the thing\n- [x] already done\n";
        let loops = extract_loops(body);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].text, "fix the thing");
    }

    #[test]
    fn loops_plain_bullets_only_under_followup_headers() {
        let body = "# Notes\n- just a note\n## Next steps\n- ship it\n";
        let loops = extract_loops(body);
        let texts: Vec<&str> = loops.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["ship it"]);
        assert_eq!(loops[0].section.as_deref(), Some("Next steps"));
    }

    #[test]
    fn loops_followup_header_variants_match() {
        for header in ["## Follow-ups", "## TODO", "## Loose ends", "## Open questions"] {
            let body = format!("{header}\n- an item\n");
            let loops = extract_loops(&body);
            assert_eq!(loops.len(), 1, "header {header:?} should open a loop section");
        }
    }

    #[test]
    fn loops_fold_indented_continuation_lines() {
        let body = "## Next\n- do a thing\n  with more detail\n";
        let loops = extract_loops(body);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].text, "do a thing with more detail");
    }

    #[test]
    fn loops_skip_fenced_content() {
        let body = "## Next\n```\n- [ ] not a real loop\n```\n- [ ] real loop\n";
        let loops = extract_loops(body);
        let texts: Vec<&str> = loops.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["real loop"]);
    }

    #[test]
    fn loops_drop_completion_markers() {
        let body = "## Next\n- [ ] done: shipped it\n- [ ] ~~scrapped~~\n- [ ] real one\n";
        let loops = extract_loops(body);
        let texts: Vec<&str> = loops.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["real one"]);
    }

    #[test]
    fn loops_header_resets_followup_section() {
        // A non-followup header after a followup one stops plain-bullet capture.
        let body = "## Next steps\n- captured\n## Random\n- not captured\n";
        let loops = extract_loops(body);
        let texts: Vec<&str> = loops.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["captured"]);
    }
}
