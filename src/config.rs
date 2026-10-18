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

use anyhow::{Context, Result, anyhow, bail};
use protocol::{FolderAction, Folders, Kind, Mode, Scan};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
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

/// Which files are indexed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sources {
    /// Folders to index, with everything under them.
    pub paths: Vec<PathBuf>,
    /// Files and folders to leave out (see Pattern). Hidden folders, packages, dependency folders, a project's build
    /// output and secrets are always left out.
    pub ignore: Vec<Pattern>,
    /// The file types of each kind, by extension. A kind left out keeps its defaults, and an empty list reads none.
    /// Screenshots are pictures macOS (or a screenshot tool) marked as one.
    #[serde(deserialize_with = "kinds_over_defaults")]
    pub kinds: BTreeMap<Kind, Vec<String>>,
    /// Read source code too, and in code projects their notes and documents as well.
    pub code: bool,
}

impl Default for Sources {
    fn default() -> Self {
        let types = |list: &[&str]| list.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        Self {
            paths: Vec::new(),
            ignore: Vec::new(),
            kinds: BTreeMap::from([
                (Kind::Plaintext, types(&["txt", "md", "markdown", "text", "org", "rst", "tex"])),
                (
                    Kind::Document,
                    types(&["pdf", "rtf", "rtfd", "doc", "docx", "odt", "html", "htm", "webarchive", "wordml"]),
                ),
                (
                    Kind::Image,
                    types(&[
                        "jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "tif", "tiff", "bmp", "avif", "jxl",
                        "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
                    ]),
                ),
                (Kind::Audio, types(&["mp3", "m4a", "aac", "wav", "aif", "aiff", "flac", "caf", "ogg", "opus", "m4b"])),
                (Kind::Video, types(&["mp4", "mov", "m4v", "avi", "mkv", "webm", "3gp"])),
                (
                    Kind::Code,
                    types(&[
                        "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "swift", "java", "kt", "c", "h", "cc",
                        "cpp", "hpp", "m", "mm", "rb", "php", "cs", "scala", "sh", "zsh", "lua", "sql", "vue",
                        "svelte", "dart", "ex", "exs", "hs", "ml", "zig", "ipynb",
                    ]),
                ),
            ]),
            code: false,
        }
    }
}

fn kinds_over_defaults<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<Kind, Vec<String>>, D::Error> {
    let mut kinds = Sources::default().kinds;
    kinds.extend(BTreeMap::<Kind, Vec<String>>::deserialize(deserializer)?);
    Ok(kinds)
}

impl Sources {
    /// The kind a file type is read as, if it's one at all (code too, whether or not code is read).
    pub fn kind_of(&self, extension: &str) -> Option<Kind> {
        self.kinds.iter().find(|(_, types)| types.iter().any(|t| t == extension)).map(|(kind, _)| *kind)
    }
}

/// Paths to leave out (`sources.ignore`), or to set reading values for (`reading.rules`):
///
///   *.psd             a name without "/" matches a file or folder of that name anywhere
///   ~/Documents/Old   one with "/" matches a path: from the top when it starts with "/" or "~/", else at any depth
///   **/Exports/**     * matches within a name, ** any number of folders, ? one character
///
/// A folder that matches counts for everything in it. Case doesn't matter.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    source: String,
    parts: Vec<String>,
    name_only: bool,
}

impl Pattern {
    /// The folder the pattern names by its full path ("~/Documents/Old"), if it does.
    fn folder(&self) -> Option<PathBuf> {
        (self.source.starts_with('/') || self.source.starts_with("~/")).then(|| expand(&self.source))
    }

    pub fn new(source: &str) -> Result<Self> {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            bail!("an empty pattern");
        }
        let expanded = if trimmed == "~" || trimmed.starts_with("~/") {
            expand(trimmed).to_string_lossy().into_owned()
        } else {
            trimmed.to_string()
        };
        let lower = expanded.trim_end_matches('/').to_lowercase();
        let name_only = !lower.contains('/');
        let mut parts: Vec<String> = lower.split('/').filter(|part| !part.is_empty()).map(String::from).collect();
        if parts.is_empty() {
            bail!("{source:?} would match everything");
        }
        if !name_only && !lower.starts_with('/') && parts[0] != "**" {
            parts.insert(0, "**".into());
        }
        Ok(Self { source: source.to_string(), parts, name_only })
    }

    /// The path matches, or a folder it's in does.
    pub fn matches(&self, path: &Path) -> bool {
        let lower = path.to_string_lossy().to_lowercase();
        let names: Vec<&str> = lower.split('/').filter(|name| !name.is_empty()).collect();
        if self.name_only {
            return names.iter().any(|name| wildcard(&self.parts[0], name));
        }
        (1..=names.len()).any(|depth| folders(&self.parts, &names[..depth]))
    }

    /// The path itself matches (a crawl, which never enters a folder that matched, asks only this).
    pub fn matches_itself(&self, path: &Path) -> bool {
        let lower = path.to_string_lossy().to_lowercase();
        let names: Vec<&str> = lower.split('/').filter(|name| !name.is_empty()).collect();
        if self.name_only {
            return names.last().is_some_and(|name| wildcard(&self.parts[0], name));
        }
        folders(&self.parts, &names)
    }
}

/// Path parts against pattern parts, where "**" stands for any number of them.
fn folders(pattern: &[String], names: &[&str]) -> bool {
    match pattern.split_first() {
        None => names.is_empty(),
        Some((first, rest)) if first == "**" => (0..=names.len()).any(|skip| folders(rest, &names[skip..])),
        Some((first, rest)) => names.first().is_some_and(|name| wildcard(first, name)) && folders(rest, &names[1..]),
    }
}

/// One name against a pattern with * (any run of characters) and ? (one character).
fn wildcard(pattern: &str, name: &str) -> bool {
    let (pattern, name): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut p, mut n) = (0, 0);
    // The last * seen, and where in the name it started matching.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                // Let the last * take one more character.
                Some((at, from)) => {
                    p = at + 1;
                    n = from + 1;
                    star = Some((at, from + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Pattern::new(&source).map_err(|error| D::Error::custom(format!("{source:?}: {error}")))
    }
}

/// The reading values: each is a field of Values, where it's optional (set at this step of the cascade or not), and
/// of Settings, where it's what a file is read with.
macro_rules! reading_values {
    ($($(#[$doc:meta])* $field:ident: $type:ty = $default:expr,)*) => {
        /// Reading values set at one step of the cascade (see Reading).
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct Values {
            $($(#[$doc])* #[serde(skip_serializing_if = "Option::is_none")] pub $field: Option<$type>,)*
        }

        /// The reading values a file is read with.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Settings {
            $($(#[$doc])* pub $field: $type,)*
        }

        impl Settings {
            /// Where the cascade starts: a value left out of config.json everywhere is this.
            pub const BASE: Settings = Settings { $($field: $default,)* };

            fn apply(&mut self, values: &Values) {
                $(if let Some(value) = &values.$field { self.$field = value.clone(); })*
            }
        }

        impl Values {
            /// All of them, at the start of the cascade.
            fn base() -> Self {
                Self { $($field: Some($default),)* }
            }

            /// These values, with those set in `over` in their place.
            fn overlay(&mut self, over: Values) {
                $(if over.$field.is_some() { self.$field = over.$field; })*
            }
        }
    };
}

reading_values! {
    /// How deep a file is read (see Depth).
    depth: Depth = Depth::Speech,
    /// The tokens the model sees a picture with: a photo reads well at 140, a video frame at 70.
    tokens: u32 = 140,
    /// The tokens for a picture with text on it: a screenshot, a scanned page. Small things are dense in it.
    text_tokens: u32 = 280,
    /// How different a video frame must be from the last one kept to start a new shot: the mean difference of a
    /// 16 × 9 grey thumbnail, 0 to 255.
    shot_change: f64 = 14.0,
    /// Within one shot, a frame is kept at least this often, in seconds.
    keep_every_s: f64 = 30.0,
    /// A video's sound is read too: heard, and at `speech`, written down.
    sound: bool = true,
    /// Text is cut into passages of about this many characters (about 300 tokens of English). Shorter passages point
    /// closer to the words that matched; longer ones carry more context per vector.
    passage_chars: usize = 1200,
    /// A passage is cut at this many characters at the latest.
    max_passage_chars: usize = 2000,
}

/// How files are read. A file's values resolve in four steps, the last one that sets a value winning: the values at
/// the top of `reading`, its kind's (`reading.video`), its file type's (`reading.types.mov`), then each rule whose
/// path matches it, in order (`reading.rules`). A value config.json leaves out keeps its default.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub all: Values,
    pub kinds: BTreeMap<Kind, Values>,
    pub types: BTreeMap<String, Values>,
    pub rules: Vec<Rule>,
}

/// Reading values for the files under a path (see Pattern).
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub path: Pattern,
    pub values: Values,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            all: Values::base(),
            kinds: BTreeMap::from([
                (Kind::Plaintext, Values::default()),
                (Kind::Document, Values::default()),
                (Kind::Image, Values::default()),
                (Kind::Audio, Values::default()),
                (Kind::Video, Values { tokens: Some(70), ..Default::default() }),
                (Kind::Code, Values { passage_chars: Some(600), ..Default::default() }),
            ]),
            types: BTreeMap::new(),
            rules: Vec::new(),
        }
    }
}

impl Reading {
    /// What a file is read with.
    pub fn resolve(&self, path: &Path, kind: Kind) -> Settings {
        let mut settings = Settings::BASE;
        settings.apply(&self.all);
        if let Some(values) = self.kinds.get(&settings_kind(kind)) {
            settings.apply(values);
        }
        if let Some(values) = self.types.get(&crate::crawl::extension_of(path)) {
            settings.apply(values);
        }
        for rule in &self.rules {
            if rule.path.matches(path) {
                settings.apply(&rule.values);
            }
        }
        settings
    }
}

/// How deep a file is read. Each depth holds the ones before it, and every file is read whole at its depth: what
/// was left to limits once (waiting, memory) the queue's order and streaming readers take care of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    /// Found by its name, its metadata and the words in it: text, a PDF's text layer, text on pictures. No model.
    Words,
    /// Also by what it means: its passages, pictures, video frames and sound, embedded.
    Meaning,
    /// Also by what's said in it, written down (in the background, once everything else is read) and embedded.
    Speech,
}

impl Settings {
    /// What's in the file is embedded.
    pub fn embeds(&self) -> bool {
        self.depth >= Depth::Meaning
    }
}

/// How `reading` is written in config.json: the values for everything, each kind's, then `types` and `rules`.
impl Serialize for Reading {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct File<'a> {
            #[serde(flatten)]
            all: &'a Values,
            #[serde(flatten)]
            kinds: &'a BTreeMap<Kind, Values>,
            types: &'a BTreeMap<String, Values>,
            rules: Vec<RuleFile<'a>>,
        }
        #[derive(Serialize)]
        struct RuleFile<'a> {
            path: &'a Pattern,
            #[serde(flatten)]
            values: &'a Values,
        }
        let rules = self.rules.iter().map(|rule| RuleFile { path: &rule.path, values: &rule.values }).collect();
        File { all: &self.all, kinds: &self.kinds, types: &self.types, rules }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Reading {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(deserializer)?;
        let values = |value: Value, at: &str| {
            serde_json::from_value::<Values>(value).map_err(|error| D::Error::custom(format!("reading{at}: {error}")))
        };
        let mut reading = Reading::default();
        let mut all = Map::new();
        for (key, value) in map {
            if key == "types" {
                let Value::Object(types) = value else { return Err(D::Error::custom("reading.types: not an object")) };
                for (file_type, value) in types {
                    let parsed = values(value, &format!(".types.{file_type}"))?;
                    reading.types.entry(file_type.trim_start_matches('.').to_lowercase()).or_default().overlay(parsed);
                }
            } else if key == "rules" {
                let Value::Array(rules) = value else { return Err(D::Error::custom("reading.rules: not a list")) };
                for (index, rule) in rules.into_iter().enumerate() {
                    let Value::Object(mut rule) = rule else {
                        return Err(D::Error::custom(format!("reading.rules[{index}]: not an object")));
                    };
                    let path = match rule.remove("path") {
                        Some(Value::String(path)) => Pattern::new(&path).map_err(D::Error::custom)?,
                        _ => return Err(D::Error::custom(format!("reading.rules[{index}]: which path?"))),
                    };
                    let parsed = values(Value::Object(rule), &format!(".rules[{index}]"))?;
                    reading.rules.push(Rule { path, values: parsed });
                }
            } else if let Some(kind) = Kind::parse(&key) {
                reading.kinds.entry(kind).or_default().overlay(values(value, &format!(".{key}"))?);
            } else {
                all.insert(key, value);
            }
        }
        reading.all.overlay(values(Value::Object(all), "")?);
        Ok(reading)
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
    /// recordings is written down beside the reading or after it (indexer::Plan). What's written is the same either
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

/// How the engine searches (engine.rs).
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

/// The kind whose settings a kind takes: a screenshot takes the image kind's.
fn settings_kind(kind: Kind) -> Kind {
    if kind == Kind::Screenshot { Kind::Image } else { kind }
}

/// An amount of memory in MB, or `auto`: what it is then depends on the Mac.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Budget {
    #[default]
    Auto,
    Mb(u64),
}

impl Budget {
    /// In bytes: as set, or else `share` of the Mac's memory.
    pub fn bytes(self, share: f64) -> u64 {
        match self {
            Budget::Mb(mb) => mb << 20,
            Budget::Auto => (crate::mac::memory() as f64 * share) as u64,
        }
    }
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

    fn pattern(source: &str) -> Pattern {
        Pattern::new(source).unwrap()
    }

    #[test]
    fn a_name_matches_anywhere_and_covers_a_folder() {
        assert!(pattern("*.psd").matches(Path::new("/Users/a/Art/cover.PSD")));
        assert!(pattern("node_modules").matches(Path::new("/p/node_modules/x/index.js")));
        assert!(!pattern("*.psd").matches(Path::new("/Users/a/psd/cover.png")));
        assert!(!pattern("*.psd").matches(Path::new("/Users/a/Art/cover.png")));
    }

    #[test]
    fn a_path_matches_from_the_top_or_at_any_depth() {
        assert!(pattern("/Users/a/Old").matches(Path::new("/Users/a/Old/2019/note.txt")));
        assert!(!pattern("/Users/a/Old").matches(Path::new("/Users/b/Users/a/Old/note.txt")));
        assert!(pattern("**/Exports/**").matches(Path::new("/Users/a/Photos/Exports")));
        assert!(pattern("**/Exports/**").matches(Path::new("/Users/a/Photos/Exports/x.jpg")));
        assert!(pattern("Exports/*.png").matches(Path::new("/x/Exports/a.png")));
        assert!(!pattern("Exports/*.png").matches_itself(Path::new("/x/Exports/sub/a.png")));
        assert!(pattern("~/Documents").matches(&dirs_home().join("Documents/a.txt")));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*", ""));
        assert!(wildcard("a*c", "abbbc"));
        assert!(wildcard("a?c", "abc"));
        assert!(!wildcard("a?c", "ac"));
        assert!(wildcard("*.tar.*", "x.tar.gz"));
        assert!(!wildcard("*.md", "md"));
    }

    #[test]
    fn values_resolve_in_order() {
        let mut config = Config::default();
        config.set("reading.video.keep_every_s", "10").unwrap();
        config.set("reading.types.mov", r#"{"keep_every_s": 20}"#).unwrap();
        config.set("reading.rules", r#"[{"path": "/talks", "keep_every_s": 5}]"#).unwrap();
        let reading = &config.reading;
        assert_eq!(reading.resolve(Path::new("/a/clip.mp4"), Kind::Video).keep_every_s, 10.0);
        assert_eq!(reading.resolve(Path::new("/a/clip.mov"), Kind::Video).keep_every_s, 20.0);
        assert_eq!(reading.resolve(Path::new("/talks/one.mov"), Kind::Video).keep_every_s, 5.0);
        assert_eq!(reading.resolve(Path::new("/a/clip.mp4"), Kind::Video).tokens, 70);
        assert_eq!(reading.resolve(Path::new("/a/song.mp3"), Kind::Audio).depth, Depth::Speech);
        config.set("reading.image.depth", "words").unwrap();
        assert_eq!(config.reading.resolve(Path::new("/a/shot.png"), Kind::Screenshot).depth, Depth::Words);
        assert!(config.set("reading.depth", "deep").is_err());
    }

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
    fn a_reading_value_changes_only_its_kinds_fingerprints() {
        let mut config = Config::default();
        let before = |config: &Config, kind| {
            let path = Path::new("/a/f");
            crate::ingest::fingerprint(path, kind, &config.reading.resolve(path, kind))
        };
        let (video, text) = (before(&config, Kind::Video), before(&config, Kind::Plaintext));
        config.set("reading.keep_every_s", "12").unwrap();
        assert_ne!(before(&config, Kind::Video), video);
        assert_eq!(before(&config, Kind::Plaintext), text);
        // Speech is written down after reading: no file is read again for it.
        config.set("reading.depth", "meaning").unwrap();
        assert_eq!(before(&config, Kind::Plaintext), text);
        // Without embedding, only what decides the words counts.
        config.set("reading.depth", "words").unwrap();
        let words = before(&config, Kind::Video);
        config.set("reading.keep_every_s", "20").unwrap();
        assert_eq!(before(&config, Kind::Video), words);
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
