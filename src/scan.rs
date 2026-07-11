use anyhow::Result;
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
    /// JSON blob of git metadata (branch, recent commits), if the dir is a repo.
    pub git_meta: Option<serde_json::Value>,
}

pub fn scan_notes(root: &Path) -> Result<Vec<NoteFile>> {
    let mut notes = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
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

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Ok(projects),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let git_meta = if path.join(".git").exists() {
            git_metadata(&path)
        } else {
            None
        };

        projects.push(Project {
            name,
            path,
            git_meta,
        });
    }

    Ok(projects)
}

fn git_metadata(repo: &Path) -> Option<serde_json::Value> {
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
