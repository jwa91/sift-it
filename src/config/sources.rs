//! Which files are indexed (`sources`): the folders, what's left out of them, and the file types of each kind. Patterns
//! also say which files a reading rule is for.

use super::expand;
use anyhow::{Result, bail};
use protocol::Kind;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    pub fn folder(&self) -> Option<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::dirs_home;

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
}
