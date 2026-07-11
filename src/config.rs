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
}

#[derive(Debug, Serialize, Deserialize)]
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
# Sources are directories raft indexes. Two kinds:
#   notes    - a tree of markdown files
#   projects - a directory whose immediate children are project repos

# [[sources]]
# path = "~/notes"
# kind = "notes"

# [[sources]]
# path = "~/dev"
# kind = "projects"
"#;
        std::fs::write(&path, default)?;
        Ok(path)
    }
}
