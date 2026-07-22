use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Names that never become entities or project mentions
    /// (compared canonically, so case-insensitive).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// strftime-style path template for today's daily note, used by
    /// `raft log`. Example: "~/notes/%Y/%Y-%m-%d.md"
    #[serde(default)]
    pub daily_note: Option<String>,
    /// What `raft publish` is allowed to make public. Absent means
    /// nothing beyond opted-in notes.
    #[serde(default)]
    pub publish: PublishConfig,
}

/// Publish consent that lives in config rather than in the content.
/// Notes opt in individually via frontmatter; repos can't carry
/// frontmatter, so they are allowlisted here. A public remote is not
/// consent - commit messages leak.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PublishConfig {
    /// Project names (as raft knows them, i.e. directory names) whose
    /// git metadata may be published.
    #[serde(default)]
    pub repos: Vec<String>,
}

// deny_unknown_fields catches a real TOML trap: top-level keys placed
// after a [[sources]] table silently become fields of that source.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub path: String,
    pub kind: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A tree of markdown notes.
    Notes,
    /// A directory whose immediate children are project repos.
    Projects,
    /// A single code repo whose source files are scanned for symbols
    /// (classes, modules, jobs) that become graph entities. Turns "this
    /// repo exists" into "this class lives here, in this file".
    Code,
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine config directory")?;
    Ok(dir.join("raft").join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir().context("could not determine data directory")?;
    Ok(dir.join("raft"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("raft.db"))
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path()?;
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "could not read config at {} (run `raft init`)",
                path.display()
            )
        })?;
        let config: Config = toml::from_str(&raw).context("could not parse config")?;
        Ok(config)
    }

    pub fn write_default() -> Result<PathBuf> {
        let path = config_path()?;
        if path.exists() {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let default = r#"# raft configuration
#
# Sources are directories raft indexes. Three kinds:
#   notes    - a tree of markdown files
#   projects - a directory whose immediate children are project repos
#   code     - a single repo, scanned for source symbols (classes,
#              modules, functions in Ruby/Python/JS/TS) that become
#              graph entities

# [[sources]]
# path = "~/notes"
# kind = "notes"

# [[sources]]
# path = "~/dev"
# kind = "projects"

# [[sources]]
# path = "~/src/some-repo"
# kind = "code"
"#;
        std::fs::write(&path, default)?;
        Ok(path)
    }
}
