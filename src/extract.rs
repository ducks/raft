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

/// A language raft can extract top-level symbols from: which file
/// extensions it owns, its display name, and a regex whose first capture
/// group is the defined symbol name. Definition-level only (classes,
/// modules, functions) - not call graphs or references.
pub struct Lang {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    re: fn() -> &'static Regex,
}

impl Lang {
    pub fn def_re(&self) -> &'static Regex {
        (self.re)()
    }
}

/// The language table. Adding a language is a data change here, not a
/// change to the scanner. Each regex is anchored to line start (after
/// indentation) so it matches definitions, not mentions, and captures
/// the symbol name in group 1.
pub fn languages() -> &'static [Lang] {
    &[
        Lang {
            name: "ruby",
            exts: &["rb"],
            re: ruby_def_re,
        },
        Lang {
            name: "python",
            exts: &["py"],
            re: python_def_re,
        },
        Lang {
            name: "javascript",
            exts: &["js", "jsx", "ts", "tsx", "mjs"],
            re: js_def_re,
        },
    ]
}

/// Look up the language that owns a file extension, if any.
pub fn lang_for_ext(ext: &str) -> Option<&'static Lang> {
    languages().iter().find(|l| l.exts.contains(&ext))
}

/// Ruby: `class Foo` / `module Bar`.
fn ruby_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*(?:class|module)\s+([A-Z][A-Za-z0-9_:]*)").unwrap())
}

/// Python: `class Foo` / `def foo`.
fn python_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*(?:class|def)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap())
}

/// JavaScript/TypeScript: `class Foo`, `function foo`, and the common
/// `export (default) class/function/const Foo` forms.
fn js_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:export\s+(?:default\s+)?)?(?:class|function\*?|const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        )
        .unwrap()
    })
}

/// Skip a leading closed frontmatter block. Frontmatter is metadata,
/// not prose: keys like `tags: [raft]` must not become project
/// mentions or entities. An unterminated block is left alone (lint's
/// problem, not extraction's).
pub fn strip_frontmatter(body: &str) -> &str {
    let Some(rest) = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
    else {
        return body;
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    body // unterminated: treat as content
}

pub fn extract(body: &str, project_names: &HashSet<String>) -> Extraction {
    let mut out = Extraction::default();

    // Strip frontmatter (metadata, not prose), then fenced code blocks,
    // so neither pollutes extraction.
    let prose = strip_fences(strip_frontmatter(body));

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
    /// Zero-based line index of the item's first line in the note,
    /// so writers (`raft done`) can edit the exact line.
    pub line: usize,
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)$").unwrap())
}

fn checkbox_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*]\s+\[( |x|X)\]\s+(.+)$").unwrap())
}

/// Extract open loops from a note body. Per the strict note format
/// (reference/note-format.md), `- [ ]` is the one loop syntax,
/// recognized anywhere; checked boxes are done and skipped. Plain
/// bullets are never loops - completion and loop-ness live in the
/// syntax, not in header heuristics or prose markers. Continuation
/// lines (indented non-bullet text) fold into the preceding item.
pub fn extract_loops(body: &str) -> Vec<Loop> {
    let mut loops: Vec<Loop> = Vec::new();
    let mut current_header: Option<String> = None;
    let mut in_fence = false;
    let mut open_item = false;

    for (line_idx, line) in body.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            open_item = false;
            continue;
        }
        if in_fence {
            continue;
        }

        if let Some(cap) = header_re().captures(line) {
            current_header = Some(cap[2].trim().to_string());
            open_item = false;
            continue;
        }

        if let Some(cap) = checkbox_re().captures(line) {
            open_item = false;
            if &cap[1] == " " {
                loops.push(Loop {
                    text: cap[2].trim().to_string(),
                    section: current_header.clone(),
                    line: line_idx,
                });
                open_item = true;
            }
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
    }

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
        let e = extract(
            "prose\n```\nreplaybook in code\n```\n",
            &names(&["replaybook"]),
        );
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
    fn loops_plain_bullets_are_never_loops() {
        // Strict format: loop-ness lives in the checkbox syntax, not in
        // header heuristics. Bullets under "Next steps" style headers
        // are prose.
        let body = "# Notes\n- just a note\n## Next steps\n- ship it\n## TODO\n- also not\n";
        assert!(extract_loops(body).is_empty());
    }

    #[test]
    fn loops_checkbox_carries_its_section() {
        let body = "## Next steps\n- [ ] ship it\n";
        let loops = extract_loops(body);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].section.as_deref(), Some("Next steps"));
    }

    #[test]
    fn loops_fold_indented_continuation_lines() {
        let body = "## Next\n- [ ] do a thing\n  with more detail\n";
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
    fn loops_completion_is_the_checkbox_not_prose_markers() {
        // Strict format: done means [x]. Prose markers no longer filter.
        let body = "## Next\n- [x] shipped it\n- [ ] real one\n";
        let loops = extract_loops(body);
        let texts: Vec<&str> = loops.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["real one"]);
    }

    // --- frontmatter --------------------------------------------------------

    #[test]
    fn strip_frontmatter_removes_closed_block() {
        let body = "---\ntitle: x\npublish: false\n---\nreal prose\n";
        assert_eq!(strip_frontmatter(body), "real prose\n");
    }

    #[test]
    fn strip_frontmatter_leaves_unterminated_and_absent_alone() {
        assert_eq!(strip_frontmatter("no frontmatter\n"), "no frontmatter\n");
        let unterminated = "---\ntitle: x\nbody\n";
        assert_eq!(strip_frontmatter(unterminated), unterminated);
        // A thematic break later in the body is not frontmatter.
        let mid = "prose\n---\nmore\n";
        assert_eq!(strip_frontmatter(mid), mid);
    }

    #[test]
    fn extract_ignores_frontmatter_content() {
        // tags/keys in frontmatter must not become mentions or entities.
        let body = "---\ntags: [replaybook]\npublish: false\n---\nprose only\n";
        let e = extract(body, &names(&["replaybook"]));
        assert!(e.project_mentions.is_empty());
    }
}
