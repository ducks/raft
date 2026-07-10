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
