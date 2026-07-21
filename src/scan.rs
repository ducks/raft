use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

pub struct NoteFile {
    pub path: PathBuf,
    /// Date parsed from a YYYY-MM-DD filename, if the note is a daily note.
    pub note_date: Option<String>,
    /// File mtime as YYYY-MM-DD, staleness fallback for undated notes.
    pub mtime_date: Option<String>,
    pub body: String,
}

pub struct Project {
    pub name: String,
    pub path: PathBuf,
    /// Whether this project directory is a git repository.
    pub is_repo: bool,
    /// Cheap staleness signal for the repo's git metadata (newest mtime among
    /// the reflog / HEAD / index). If this matches the value stored in the
    /// live index, the cached git metadata can be reused instead of shelling
    /// out to git again. `None` for non-repos or when it can't be read.
    pub git_fingerprint: Option<String>,
}

/// A code symbol (class/module/function) found in a repo. Becomes a graph
/// entity linked to the repo it lives in and the file that defines it.
pub struct CodeSymbol {
    /// Symbol name as written, e.g. `SummariesBackfill`.
    pub name: String,
    /// Repo-relative file path, e.g.
    /// `plugins/discourse-ai/app/jobs/scheduled/summaries_backfill.rb`.
    pub file: String,
    /// The repo's directory name (the semantic "where it lives").
    pub repo: String,
    /// Which language's extractor produced this symbol.
    pub lang: &'static str,
}

/// Walk a code repo and extract top-level symbol definitions, dispatching
/// per file extension through the language table in `extract`. Deliberately
/// shallow and dependency-free: definitions only (regex, not a parser);
/// call graphs and references are out of scope. Adding a language is a
/// change to `extract::languages()`, not to this function.
pub fn scan_code(root: &Path) -> Result<Vec<CodeSymbol>> {
    let repo = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let mut symbols = Vec::new();

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            // Skip VCS, vendored deps, and test trees - noise, not structure.
            let name = e.file_name().to_str().unwrap_or("");
            !matches!(
                name,
                ".git" | "vendor" | "node_modules" | "spec" | "test" | "tmp"
            )
        })
        .flatten()
    {
        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let lang = match crate::extract::lang_for_ext(ext) {
            Some(l) => l,
            None => continue,
        };
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let file = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        for caps in lang.def_re().captures_iter(&body) {
            if let Some(name) = caps.get(1) {
                symbols.push(CodeSymbol {
                    name: name.as_str().to_string(),
                    file: file.clone(),
                    repo: repo.clone(),
                    lang: lang.name,
                });
            }
        }
    }

    Ok(symbols)
}

pub fn scan_notes(root: &Path) -> Result<Vec<NoteFile>> {
    let mut notes = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry =
            entry.with_context(|| format!("could not walk notes source {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("could not read note {}", path.display()))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let note_date = parse_date_stem(stem);
        let mtime_date = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%d")
                    .to_string()
            });

        notes.push(NoteFile {
            path: path.to_path_buf(),
            note_date,
            mtime_date,
            body,
        });
    }

    Ok(notes)
}

fn parse_date_stem(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits_ok = stem.char_indices().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == '-'
        } else {
            c.is_ascii_digit()
        }
    });
    digits_ok.then(|| stem.to_string())
}

pub fn scan_projects(root: &Path) -> Result<Vec<Project>> {
    let mut projects = Vec::new();

    let entries = std::fs::read_dir(root)
        .with_context(|| format!("could not read projects source {}", root.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("could not read entry in projects source {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let is_repo = path.join(".git").exists();
        // Cheap stat-only fingerprint here; the expensive `git log` is
        // deferred to `git_metadata`, called only on a cache miss.
        let git_fingerprint = if is_repo {
            git_fingerprint(&path)
        } else {
            None
        };

        projects.push(Project {
            name,
            path,
            is_repo,
            git_fingerprint,
        });
    }

    Ok(projects)
}

/// A cheap staleness signal for a repo's git state: the newest mtime among
/// `.git/logs/HEAD` (touched by every commit/checkout/reset/merge), `.git/HEAD`
/// (branch switches), and `.git/index` (staging). No subprocess. Returns None
/// if none can be stat'd (e.g. reflogs disabled and a bare-ish layout), in
/// which case the caller treats the repo as always-stale and refreshes.
pub fn git_fingerprint(repo: &Path) -> Option<String> {
    let git_dir = repo.join(".git");
    let candidates = [
        git_dir.join("logs").join("HEAD"),
        git_dir.join("HEAD"),
        git_dir.join("index"),
    ];
    let newest = candidates
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .max()?;
    Some(newest.to_string())
}

/// Full git metadata (branch + recent commits) for a repo. This is the
/// expensive path: two `git` subprocesses. Called only when the fingerprint
/// shows the repo changed since the last index.
pub fn git_metadata(repo: &Path) -> Option<serde_json::Value> {
    let branch = git_output(repo, &["branch", "--show-current"])?;
    let log = git_output(
        repo,
        &["log", "--format=%h\x1f%ad\x1f%s", "--date=short", "-20"],
    )
    .unwrap_or_default();

    let commits: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\x1f');
            let sha = parts.next()?;
            let date = parts.next()?;
            let subject = parts.next()?;
            Some(json!({ "sha": sha, "date": date, "subject": subject }))
        })
        .collect();

    Some(json!({
        "branch": branch.trim(),
        "commits": commits,
    }))
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scan_code_extracts_definitions_across_languages() {
        let dir = TempDir::new().unwrap();
        let jobs = dir.path().join("app/jobs/scheduled");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(
            jobs.join("summaries_backfill.rb"),
            "module Jobs\n  class SummariesBackfill < ::Jobs::Scheduled\n    def execute(args); end\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sync.py"),
            "class ProfileSync:\n    def run(self):\n        pass\n\ndef helper():\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("widget.ts"),
            "export class Widget {}\nexport default function render() {}\nconst config = {};\n",
        )
        .unwrap();
        // Noise that must be skipped: a spec dir and an unknown extension.
        let spec = dir.path().join("spec");
        std::fs::create_dir_all(&spec).unwrap();
        std::fs::write(spec.join("thing_spec.rb"), "class ShouldBeSkipped; end\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# class NotCode\n").unwrap();

        let symbols = scan_code(dir.path()).unwrap();
        let named = |n: &str| symbols.iter().find(|s| s.name == n);

        // Ruby
        assert!(named("Jobs").is_some());
        let backfill = named("SummariesBackfill").expect("ruby class");
        assert_eq!(backfill.lang, "ruby");
        assert!(backfill.file.ends_with("summaries_backfill.rb"));
        // Python
        assert_eq!(named("ProfileSync").expect("py class").lang, "python");
        assert!(named("helper").is_some(), "python def");
        // JS/TS
        assert_eq!(named("Widget").expect("ts class").lang, "javascript");
        assert!(named("render").is_some(), "ts function");
        // Skipped
        assert!(named("ShouldBeSkipped").is_none(), "spec/ skipped");
        assert!(named("NotCode").is_none(), "non-code ext skipped");
    }

    fn error_message<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected scan to fail"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn parse_date_stem_accepts_iso_dates() {
        assert_eq!(
            parse_date_stem("2026-07-10"),
            Some("2026-07-10".to_string())
        );
        assert_eq!(
            parse_date_stem("1999-01-01"),
            Some("1999-01-01".to_string())
        );
    }

    #[test]
    fn parse_date_stem_rejects_wrong_shape() {
        // Wrong length, wrong separators, or non-daily filenames.
        assert_eq!(parse_date_stem("2026-7-10"), None); // not zero-padded
        assert_eq!(parse_date_stem("2026/07/10"), None); // wrong separator
        assert_eq!(parse_date_stem("notes"), None);
        assert_eq!(parse_date_stem("2026-07-10-notes"), None);
        assert_eq!(parse_date_stem(""), None);
    }

    #[test]
    fn parse_date_stem_rejects_non_digits_in_date_slots() {
        // Right shape, but letters where digits belong.
        assert_eq!(parse_date_stem("20xx-07-10"), None);
        assert_eq!(parse_date_stem("2026-ab-10"), None);
    }

    #[test]
    fn missing_notes_source_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        let err = error_message(scan_notes(&missing));

        assert!(err.contains("could not walk notes source"), "{err}");
        assert!(err.contains(missing.to_str().unwrap()), "{err}");
    }

    #[test]
    fn unreadable_note_content_is_an_error() {
        let dir = TempDir::new().unwrap();
        let note = dir.path().join("invalid.md");
        std::fs::write(&note, [0xff, 0xfe]).unwrap();

        let err = error_message(scan_notes(dir.path()));

        assert!(err.contains("could not read note"), "{err}");
        assert!(err.contains(note.to_str().unwrap()), "{err}");
    }

    #[test]
    fn missing_projects_source_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        let err = error_message(scan_projects(&missing));

        assert!(err.contains("could not read projects source"), "{err}");
        assert!(err.contains(missing.to_str().unwrap()), "{err}");
    }

    fn git(repo: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "one"]);
    }

    #[test]
    fn git_fingerprint_none_for_non_repo() {
        let dir = TempDir::new().unwrap();
        assert!(git_fingerprint(dir.path()).is_none());
    }

    #[test]
    fn git_fingerprint_stable_without_changes() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let a = git_fingerprint(dir.path());
        let b = git_fingerprint(dir.path());
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn git_fingerprint_changes_after_commit() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let before = git_fingerprint(dir.path());
        std::fs::write(dir.path().join("b.txt"), "two").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "two"]);
        let after = git_fingerprint(dir.path());
        assert_ne!(before, after, "a commit must change the fingerprint");
    }

    #[test]
    fn scan_projects_reports_repo_and_fingerprint() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("myrepo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        std::fs::create_dir(root.path().join("plain")).unwrap();

        let projects = scan_projects(root.path()).unwrap();
        let repo_p = projects.iter().find(|p| p.name == "myrepo").unwrap();
        let plain_p = projects.iter().find(|p| p.name == "plain").unwrap();

        assert!(repo_p.is_repo);
        assert!(repo_p.git_fingerprint.is_some());
        assert!(!plain_p.is_repo);
        assert!(plain_p.git_fingerprint.is_none());
    }
}
