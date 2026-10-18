//! What Sift It does, and where it keeps its files: ~/Library/Application Support/Sift It (or $SIFT_IT_HOME).
//!
//!   config.json     what to index and how: one per Mac (`sift-it config`, `sift-it folders`)
//!   index.sqlite    the index
//!   vectors/        its vectors (index/vectors.rs)
//!   status.json     what the background indexer is doing (for front ends)
//!   models/         the model files
//!   previews/       thumbnails rendered for search results
//!
//! config.json has a section for each part of the work, and what a change costs follows from its section:
//!
//!   sources    which files: paths, ignore patterns, the file types of each kind      a sync
//!   reading    how deep and how files are read, per kind, type and folder         the files it affects are read again
//!   ranking    how results rank                                                     the next search
//!   indexing   when the background indexer works                                    at once
//!   search     how the engine searches                                              the next search
//!   log        the background indexer's log                                         at once
//!
//! Every value is written out, so the file shows all there is to change. One that's left out takes its default.

mod reading;
mod sources;

pub use reading::{Depth, Reading, Settings};
pub use sources::{Pattern, Sources};

use anyhow::{Context, Result, anyhow, bail};
use protocol::{FolderAction, Folders, Kind, Mode, Scan};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sources: Sources,
    pub reading: Reading,
    pub ranking: Ranking,
    pub indexing: Indexing,
    pub search: Search,
    pub log: Log,
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut config: Config =
                    serde_json::from_slice(&bytes).with_context(|| format!("can't read {}", path.display()))?;
                config.check().with_context(|| format!("can't read {}", path.display()))?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("can't read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        write_atomically(&path, &serde_json::to_vec_pretty(self)?)
    }

    /// The value at a dotted key ("reading.video"), or all of it.
    pub fn get(&self, key: Option<&str>) -> Result<Value> {
        let mut node = serde_json::to_value(self)?;
        for part in key.into_iter().flat_map(|key| key.split('.')) {
            node = node.get(part).cloned().ok_or_else(|| anyhow!("there's no {}", key.unwrap_or_default()))?;
        }
        Ok(node)
    }

    /// Sets the value at a dotted key ("reading.video.keep_every_s"), from JSON or else as a string. `null` puts it
    /// back to its default.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut tree = serde_json::to_value(&*self)?;
        let parsed: Value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
        let parts: Vec<&str> = key.split('.').collect();
        let (last, sections) = parts.split_last().ok_or_else(|| anyhow!("which key?"))?;
        let mut node = &mut tree;
        for part in sections {
            let object = node.as_object_mut().ok_or_else(|| anyhow!("{key}: {part} isn't a section"))?;
            node = object.entry(part.to_string()).or_insert_with(|| json!({}));
        }
        let object = node.as_object_mut().ok_or_else(|| anyhow!("{key} isn't in a section"))?;
        if parsed.is_null() {
            object.remove(*last);
        } else {
            object.insert(last.to_string(), parsed);
        }
        let mut updated: Config = serde_json::from_value(tree).with_context(|| format!("can't set {key}"))?;
        updated.check()?;
        *self = updated;
        Ok(())
    }

    /// The folders indexed, and the folders inside them left out (ignore patterns that name a folder by its path).
    pub fn folders(&self) -> Folders {
        Folders {
            folders: self.sources.paths.iter().map(|path| path.display().to_string()).collect(),
            excluded: self.sources.ignore.iter().filter_map(Pattern::folder).map(|f| f.display().to_string()).collect(),
        }
    }

    /// Changes which folders are indexed (save it after).
    pub fn change_folders(&mut self, action: FolderAction, paths: &[String]) -> Result<()> {
        for path in paths.iter().map(|path| expand(path)) {
            let sources = &mut self.sources;
            match action {
                FolderAction::Add => {
                    if !path.is_dir() {
                        bail!("not a folder: {}", path.display());
                    }
                    if !sources.paths.contains(&path) {
                        sources.paths.push(path);
                    }
                }
                FolderAction::Remove => {
                    sources.paths.retain(|folder| folder != &path);
                    sources.ignore.retain(|pattern| !pattern.folder().is_some_and(|folder| folder.starts_with(&path)));
                }
                FolderAction::Include => sources.ignore.retain(|pattern| pattern.folder().as_ref() != Some(&path)),
                FolderAction::Exclude => {
                    if !sources.ignore.iter().any(|pattern| pattern.folder().as_ref() == Some(&path)) {
                        sources.ignore.push(Pattern::new(&path.to_string_lossy())?);
                    }
                }
            }
        }
        Ok(())
    }

    /// Makes paths absolute, and checks what serde can't.
    fn check(&mut self) -> Result<()> {
        for path in &mut self.sources.paths {
            *path = expand(&path.to_string_lossy());
        }
        for types in self.sources.kinds.values_mut() {
            for file_type in types.iter_mut() {
                *file_type = file_type.trim_start_matches('.').to_lowercase();
            }
        }
        let mut seen: BTreeMap<&str, Kind> = BTreeMap::new();
        for (kind, types) in &self.sources.kinds {
            if *kind == Kind::Screenshot {
                bail!("sources.kinds: a screenshot is a picture macOS marked as one, not a file type");
            }
            for file_type in types {
                if let Some(other) = seen.insert(file_type, *kind) {
                    bail!("sources.kinds: .{file_type} is both {} and {}", other.as_str(), kind.as_str());
                }
            }
        }
        if self.reading.kinds.contains_key(&Kind::Screenshot) {
            bail!("reading: screenshots are read with the image values");
        }
        Ok(())
    }
}

/// How results rank (see query/rank.rs for what each value does to them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ranking {
    /// Lift (cosine above the query's median) a meaning match needs to be shown at all. On the testbed
    /// (evals/evals.json), answers lift 0.12–0.30 (nearly all above 0.135); the best results for queries without an
    /// answer lift 0.05–0.10, except a near miss at 0.144.
    pub shown_lift: f32,
    /// Lift a meaning match needs to count as a strong match.
    pub strong_lift: f32,
    /// The bonus for containing all of the query's words, in the same units, when one of them is a number or the
    /// search is of code or quoted words: the words decide.
    pub word_bonus: f32,
    /// The bonus for containing all of them otherwise: the words only choose between matches the meaning scores about
    /// the same. Any larger, and a passage that only mentions them outranks a better picture or recording of what they
    /// describe.
    pub plain_word_bonus: f32,
    /// A file of n segments has its lifts lowered by length_penalty × ln(n / length_free), and one of up to
    /// length_free segments (a photo, a few pages, a minute and a half of sound) not at all. That is about how far the
    /// best of n segments stands above a typical one by chance alone: the expected maximum of n draws, at the usual
    /// spread of a query's scores (0.03), less that of length_free. A 3-minute video loses 0.03, a novel 0.05–0.07.
    /// On a folder of 520 mixed files and eight books (docs/ranking.md), a book had come first for 21 of 95 queries
    /// about something else; now 1.
    pub length_penalty: f32,
    pub length_free: f32,
    /// Segments of one file within this much of its best are close enough for words to choose between them.
    pub close: f32,
    /// At most this many of one file's segments are candidates, so a long file can't use up what's taken.
    pub per_file: usize,
    /// A typical median: across the testbed's queries, a query's median segment scores 0.49–0.59. A small index's own
    /// median is pulled toward it (as if it held prior_weight more segments scoring that), or with a file or two the
    /// median would be the match itself, and nothing could ever stand out.
    pub prior_median: f32,
    pub prior_weight: f32,
}

impl Default for Ranking {
    fn default() -> Self {
        Self {
            shown_lift: 0.09,
            strong_lift: 0.13,
            word_bonus: 0.10,
            plain_word_bonus: 0.03,
            length_penalty: 0.0125,
            length_free: 4.0,
            close: 0.04,
            per_file: 10,
            prior_median: 0.55,
            prior_weight: 30.0,
        }
    }
}

/// When the background indexer works, and with how much memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Indexing {
    /// How much memory indexing may take, in MB: how many files are read at once, and whether what's said in
    /// recordings is written down beside the reading or after it (resources::IngestPlan). What's written is the same either
    /// way. At `auto`, a quarter of the Mac's memory.
    pub memory_mb: Budget,
    /// Index while on battery power. In Low Power Mode it always waits.
    pub on_battery: bool,
    /// A changed path is handled once it has been quiet this long: a download, a copy, an export settling.
    pub quiet_ms: u64,
    /// A full sync this often, in minutes, in case a file-system event went missing.
    pub full_sync_min: u64,
    /// The model is let go after this many seconds with nothing to index.
    pub release_model_s: u64,
}

impl Default for Indexing {
    fn default() -> Self {
        Self { memory_mb: Budget::Auto, on_battery: true, quiet_ms: 1500, full_sync_min: 30, release_model_s: 60 }
    }
}

/// How the engine searches (services/engine.rs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    /// What a search is by when it doesn't say.
    pub mode: Mode,
    /// Which form of the vectors a search by meaning scans.
    pub vectors: Scan,
    /// How much of the vectors a search may read, in MB: with `vectors` at `auto`, it scans the most precise form that
    /// fits. At `auto`, a tenth of the Mac's memory.
    pub memory_mb: Budget,
}

/// Sets the value at a dotted key in config.json when there's a value, and gives the settings: `sift-it config`, and
/// the protocol's `config`.
pub fn configure(key: Option<&str>, value: Option<&str>) -> Result<Config> {
    let mut config = Config::load()?;
    if let (Some(key), Some(value)) = (key, value) {
        config.set(key, value)?;
        config.save()?;
    }
    Ok(config)
}

/// An amount of memory in MB, or `auto`: what it is then depends on the Mac (resources.rs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Budget {
    #[default]
    Auto,
    Mb(u64),
}

impl Serialize for Budget {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Budget::Auto => serializer.serialize_str("auto"),
            Budget::Mb(mb) => serializer.serialize_u64(*mb),
        }
    }
}

impl<'de> Deserialize<'de> for Budget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(word) if word == "auto" => Ok(Budget::Auto),
            other => other
                .as_u64()
                .map(Budget::Mb)
                .ok_or_else(|| D::Error::custom(format!("{other}: a number of MB, or \"auto\""))),
        }
    }
}

/// The background indexer's log (indexer.log).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    /// Past this many megabytes, the log's older half goes.
    pub max_mb: u64,
}

impl Default for Log {
    fn default() -> Self {
        Self { max_mb: 4 }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips_and_rejects_typos() {
        let mut config = Config::default();
        config.set("sources.ignore", r#"["*.psd", "~/Old"]"#).unwrap();
        config.set("reading.rules", r#"[{"path": "**/Lectures/**", "keep_every_s": 10}]"#).unwrap();
        config.set("search.memory_mb", "512").unwrap();
        assert_eq!(config.search.memory_mb, Budget::Mb(512));
        let text = serde_json::to_string_pretty(&config).unwrap();
        let mut back: Config = serde_json::from_str(&text).unwrap();
        back.check().unwrap();
        assert_eq!(back, config);
        assert!(config.set("reading.video.keep_evry_s", "10").is_err());
        assert!(config.set("ranking.shown_lyft", "0.1").is_err());
        assert!(config.set("sources.kinds.image", r#"["mp4"]"#).is_err());
        assert!(config.set("search.memory_mb", "lots").is_err());
    }

    #[test]
    fn folders_are_added_left_out_and_removed() {
        let top = std::env::temp_dir().join(format!("sift-it-folders-{}", std::process::id()));
        let inner = top.join("Old");
        std::fs::create_dir_all(&inner).unwrap();
        let (top_s, inner_s) = (top.display().to_string(), inner.display().to_string());
        let mut config = Config::default();
        config.change_folders(FolderAction::Add, std::slice::from_ref(&top_s)).unwrap();
        config.change_folders(FolderAction::Exclude, std::slice::from_ref(&inner_s)).unwrap();
        assert_eq!(config.folders(), Folders { folders: vec![top_s.clone()], excluded: vec![inner_s.clone()] });
        config.change_folders(FolderAction::Include, std::slice::from_ref(&inner_s)).unwrap();
        assert!(config.folders().excluded.is_empty());
        // Removing a folder forgets what was left out of it.
        config.change_folders(FolderAction::Exclude, std::slice::from_ref(&inner_s)).unwrap();
        config.change_folders(FolderAction::Remove, &[top_s]).unwrap();
        assert_eq!(config.folders(), Folders::default());
        assert!(config.change_folders(FolderAction::Add, &["/no/such/folder".into()]).is_err());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn null_puts_a_value_back_to_its_default() {
        let mut config = Config::default();
        config.set("reading.video.tokens", "100").unwrap();
        config.set("reading.video.tokens", "null").unwrap();
        config.set("ranking.close", "null").unwrap();
        assert_eq!(config, Config::default());
        // A config.json with only some of it takes the rest from the defaults.
        let partial: Config = serde_json::from_str(r#"{"reading": {"video": {"keep_every_s": 10}}}"#).unwrap();
        assert_eq!(partial.reading.resolve(Path::new("/a.mov"), Kind::Video).tokens, 70);
        assert_eq!(partial.sources.kind_of("mov"), Some(Kind::Video));
    }
}
