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
