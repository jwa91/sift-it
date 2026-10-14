//! Where Sift It keeps its files: ~/Library/Application Support/Sift It (or $SIFT_IT_HOME).
//!
//!   config.json     the folders to index and what to skip (written by the Raycast extension or `sift-it folders`)
//!   index.sqlite    the index
//!   status.json     what the indexer is doing (read by the Raycast menu bar command)
//!   models/         the model files
//!   previews/       thumbnails rendered for search results

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("SIFT_IT_HOME") {
        return PathBuf::from(home);
    }
    dirs_home().join("Library/Application Support/Sift It")
}

pub fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME isn't set"))
}

pub fn models_dir() -> PathBuf {
    std::env::var_os("SIFT_IT_MODELS").map(PathBuf::from).unwrap_or_else(|| home().join("models"))
}

pub fn index_path() -> PathBuf {
    home().join("index.sqlite")
}

pub fn config_path() -> PathBuf {
    home().join("config.json")
}

pub fn status_path() -> PathBuf {
    home().join("status.json")
}

pub fn previews_dir() -> PathBuf {
    home().join("previews")
}

/// What to index. Missing fields take their defaults, so an older or hand-written file still loads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Folders to index, with everything under them.
    pub folders: Vec<PathBuf>,
    /// Folders under those to leave out.
    pub excluded: Vec<PathBuf>,
    /// File extensions to leave out ("heic", "txt").
    pub excluded_extensions: Vec<String>,
    /// Also read source code (as text), inside code projects too.
    pub include_code: bool,
    /// Index while on battery power.
    pub index_on_battery: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            excluded: Vec::new(),
            excluded_extensions: Vec::new(),
            include_code: false,
            index_on_battery: true,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("can't read {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("can't read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        write_atomically(&path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Expands a leading ~ and makes the path absolute.
pub fn expand(path: &str) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs_home().join(rest)
    } else if path == "~" {
        dirs_home()
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map(|dir| dir.join(&expanded)).unwrap_or(expanded)
    }
}
