//! Which files get indexed, and finding them. Only the chosen folders are read, and in them never hidden folders,
//! app bundles and libraries, build output, dependency folders, keys and other secrets, or iCloud files that are only
//! in the cloud (reading those would download them).

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Plain text and Markdown.
    Text,
    /// Rich documents: rtf, doc, docx, odt, html, pages.
    Doc,
    Pdf,
    Image,
    /// A picture of a screen: read for its text as well as what it shows.
    Screenshot,
    Audio,
    Video,
    /// Source code (only with `include_code`).
    Code,
}

impl Kind {
    pub const ALL: [Kind; 8] =
        [Kind::Text, Kind::Doc, Kind::Pdf, Kind::Image, Kind::Screenshot, Kind::Audio, Kind::Video, Kind::Code];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Doc => "doc",
            Kind::Pdf => "pdf",
            Kind::Image => "image",
            Kind::Screenshot => "screenshot",
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Code => "code",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// How much indexing a file of this kind costs, for ordering the work: cheap kinds first, so most files are
    /// searchable soon and the long recordings come last.
    pub fn cost_rank(self) -> u8 {
        match self {
            Kind::Text | Kind::Doc | Kind::Code => 0,
            Kind::Screenshot | Kind::Image => 1,
            Kind::Pdf => 2,
            Kind::Audio => 3,
            Kind::Video => 4,
        }
    }
}

pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "tif", "tiff", "bmp", "avif", "jxl", "dng", "cr2", "cr3",
    "nef", "arw", "raf", "orf", "rw2",
];
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4a", "aac", "wav", "aif", "aiff", "flac", "caf", "ogg", "opus", "m4b"];
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "avi", "mkv", "webm", "3gp"];
pub const TEXT_EXTENSIONS: &[&str] = &["txt", "md", "markdown", "text", "org", "rst", "tex"];
pub const DOC_EXTENSIONS: &[&str] = &["rtf", "rtfd", "doc", "docx", "odt", "html", "htm", "webarchive", "wordml"];
pub const CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "swift", "java", "kt", "c", "h", "cc", "cpp", "hpp", "m", "mm",
    "rb", "php", "cs", "scala", "sh", "zsh", "lua", "sql", "vue", "svelte", "dart", "ex", "exs", "hs", "ml", "zig",
    "ipynb",
];

/// Folder names never entered: dependencies, caches, the user's Library.
const SKIPPED_FOLDERS: &[&str] = &[
    "node_modules",
    "DerivedData",
    "Pods",
    "Carthage",
    "__pycache__",
    "site-packages",
    "bower_components",
    "Caches",
    "Library",
];

/// Folder names not entered inside a code project: build output, vendored code, environments.
const SKIPPED_PROJECT_FOLDERS: &[&str] =
    &["target", "build", "dist", "out", "venv", "env", "vendor", "tmp", "cache", "coverage", "obj", "bin"];

/// Folder extensions of packages: apps, libraries, bundles. Opaque to Finder, and never what someone searches for.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "xcodeproj",
    "xcworkspace",
    "xcassets",
    "sparsebundle",
    "dmg",
    "lproj",
    "pkg",
    "mpkg",
    "logicx",
    "band",
    "photoslibrary",
    "aplibrary",
    "playground",
    "rtfd",
    "pages",
    "numbers",
    "key",
];

/// Marks a folder as a code project.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "Package.swift",
    "composer.json",
    "mix.exs",
    "CMakeLists.txt",
    "Makefile",
];

/// Secrets: never read, whatever folder they're in.
const SECRET_EXTENSIONS: &[&str] = &[
    "pem",
    "key",
    "p12",
    "pfx",
    "cer",
    "crt",
    "der",
    "keychain",
    "keychain-db",
    "kdbx",
    "agilekeychain",
    "gpg",
    "asc",
    "ovpn",
    "mobileprovision",
    "p8",
];

#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: f64,
}

/// A whole crawl: what was found, and which chosen folders couldn't be read (an unplugged drive). Files indexed from
/// those stay in the index.
#[derive(Debug, Default)]
pub struct Crawl {
    pub found: Vec<Found>,
    pub unavailable: Vec<PathBuf>,
}

pub fn crawl(config: &Config) -> Crawl {
    let mut crawl = Crawl::default();
    let excluded_extensions: Vec<String> =
        config.excluded_extensions.iter().map(|e| e.trim_start_matches('.').to_lowercase()).collect();
    let rules = Rules { config, excluded_extensions: &excluded_extensions };
    for root in &config.folders {
        match std::fs::read_dir(root) {
            Ok(_) => walk(root, &rules, false, &mut crawl.found),
            Err(_) => crawl.unavailable.push(root.clone()),
        }
    }
    // A chosen folder inside another one: its files were found twice.
    let mut seen = std::collections::HashSet::new();
    crawl.found.retain(|file| seen.insert(file.path.clone()));
    crawl
}

struct Rules<'a> {
    config: &'a Config,
    excluded_extensions: &'a [String],
}

fn walk(folder: &Path, rules: &Rules, in_project: bool, found: &mut Vec<Found>) {
    if rules.config.excluded.iter().any(|excluded| folder.starts_with(excluded)) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let entries: Vec<_> = entries.flatten().collect();
    let in_project = in_project
        || entries.iter().any(|entry| PROJECT_MARKERS.contains(&entry.file_name().to_string_lossy().as_ref()));
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_skipped_folder(&path, in_project) {
                continue;
            }
            walk(&path, rules, in_project, found);
        } else if file_type.is_file()
            && let Some(file) = consider(&path, &name, rules, in_project)
        {
            found.push(file);
        }
    }
}

/// A folder never entered: dependencies, caches, packages, a project's build output.
fn is_skipped_folder(path: &Path, in_project: bool) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    name.starts_with('.')
        || SKIPPED_FOLDERS.contains(&name.as_str())
        || (in_project && SKIPPED_PROJECT_FOLDERS.contains(&name.as_str()))
        || PACKAGE_EXTENSIONS.contains(&extension_of(path).as_str())
}

fn is_project(folder: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|marker| folder.join(marker).exists())
}

/// What a change at one path means for the index.
#[derive(Debug)]
pub enum Rescan {
    /// Not in any chosen folder.
    Outside,
    /// In a chosen folder, but somewhere never read (a hidden or skipped folder, an excluded one).
    Skipped,
    /// In a chosen folder that can't be read now (an unplugged drive): what was indexed from it stays, as in a full
    /// crawl.
    Unavailable,
    /// What's there now: the file itself, everything in a folder, or nothing when it's gone.
    Found(Vec<Found>),
}

/// Looks again at one changed path (a file, or a folder and all in it), by the same rules as a full crawl.
pub fn rescan(config: &Config, path: &Path) -> Rescan {
    let Some(root) =
        config.folders.iter().filter(|root| path.starts_with(root)).max_by_key(|root| root.as_os_str().len())
    else {
        return Rescan::Outside;
    };
    if std::fs::read_dir(root).is_err() {
        return Rescan::Unavailable;
    }
    if config.excluded.iter().any(|excluded| path.starts_with(excluded)) {
        return Rescan::Skipped;
    }
    // The folders between the root and the path: none may be skipped or a symlink (a crawl doesn't follow those, and
    // reading the path would), and any may make it a code project.
    let mut in_project = false;
    let mut folder = root.clone();
    for component in path.strip_prefix(root).unwrap_or(Path::new("")).parent().into_iter().flat_map(|p| p.components())
    {
        in_project |= is_project(&folder);
        folder.push(component);
        if is_skipped_folder(&folder, in_project)
            || std::fs::symlink_metadata(&folder).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Rescan::Skipped;
        }
    }
    in_project |= is_project(&folder);
    let excluded_extensions: Vec<String> =
        config.excluded_extensions.iter().map(|e| e.trim_start_matches('.').to_lowercase()).collect();
    let rules = Rules { config, excluded_extensions: &excluded_extensions };
    let Ok(metadata) = std::fs::symlink_metadata(path) else { return Rescan::Found(Vec::new()) };
    let mut found = Vec::new();
    if metadata.is_dir() {
        if path != root.as_path() && is_skipped_folder(path, in_project) {
            return Rescan::Skipped;
        }
        walk(path, &rules, in_project, &mut found);
    } else if metadata.is_file() {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') {
            return Rescan::Skipped;
        }
        found.extend(consider(path, &name, &rules, in_project));
    }
    Rescan::Found(found)
}

/// The file, if it's one to index.
fn consider(path: &Path, name: &str, rules: &Rules, in_project: bool) -> Option<Found> {
    let extension = extension_of(path);
    if rules.excluded_extensions.contains(&extension) || is_secret(name, &extension) {
        return None;
    }
    let mut kind = kind_of(&extension, rules.config.include_code)?;
    // In a code project only pictures, PDFs and recordings are read, unless code is wanted: its text files are
    // READMEs, fixtures and data, which would bury everything else.
    if in_project && matches!(kind, Kind::Text | Kind::Doc) && !rules.config.include_code {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if is_cloud_only(&metadata) || metadata.len() == 0 {
        return None;
    }
    if kind == Kind::Image && is_screenshot(path, name) {
        kind = Kind::Screenshot;
    }
    let limit: u64 = match kind {
        Kind::Text | Kind::Code => 20 << 20,
        Kind::Doc => 100 << 20,
        Kind::Image | Kind::Screenshot => 200 << 20,
        Kind::Pdf => 1 << 30,
        Kind::Audio | Kind::Video => u64::MAX,
    };
    if metadata.len() > limit {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    Some(Found { path: path.to_path_buf(), kind, size: metadata.len(), modified })
}

pub fn extension_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn kind_of(extension: &str, include_code: bool) -> Option<Kind> {
    let has = |list: &[&str]| list.contains(&extension);
    if has(TEXT_EXTENSIONS) {
        Some(Kind::Text)
    } else if has(DOC_EXTENSIONS) {
        Some(Kind::Doc)
    } else if extension == "pdf" {
        Some(Kind::Pdf)
    } else if has(IMAGE_EXTENSIONS) {
        Some(Kind::Image)
    } else if has(AUDIO_EXTENSIONS) {
        Some(Kind::Audio)
    } else if has(VIDEO_EXTENSIONS) {
        Some(Kind::Video)
    } else if include_code && has(CODE_EXTENSIONS) {
        Some(Kind::Code)
    } else {
        None
    }
}

fn is_secret(name: &str, extension: &str) -> bool {
    let lower = name.to_lowercase();
    SECRET_EXTENSIONS.contains(&extension)
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with(".env")
        || lower.contains("password")
        || lower.contains("wachtwoord")
        || lower.contains("secret")
        || lower.contains("credential")
}

/// An iCloud file that's only in the cloud: its data would be downloaded when read.
fn is_cloud_only(metadata: &std::fs::Metadata) -> bool {
    use std::os::darwin::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    metadata.st_flags() & SF_DATALESS != 0
}

/// macOS marks the screenshots it takes (kMDItemIsScreenCapture); other tools name them so.
fn is_screenshot(path: &Path, name: &str) -> bool {
    let lower = name.to_lowercase();
    if ["screenshot", "screen shot", "schermafbeelding", "bildschirmfoto", "capture d", "cleanshot"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    has_xattr(path, "com.apple.metadata:kMDItemIsScreenCapture")
}

fn has_xattr(path: &Path, name: &str) -> bool {
    let (Ok(path), Ok(name)) = (CString::new(path.as_os_str().as_bytes()), CString::new(name)) else {
        return false;
    };
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) >= 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own under the temporary folder, removed when the test ends.
    struct TempFolder(PathBuf);
    impl TempFolder {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sift-it-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config(root: &Path) -> Config {
        Config { folders: vec![root.to_path_buf()], ..Default::default() }
    }

    #[test]
    fn a_chosen_folder_that_cant_be_read_is_left_alone() {
        let temp = TempFolder::new("unavailable");
        let root = temp.0.join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "a note").unwrap();
        let config = config(&root);
        // Deleted from a folder that's there: gone.
        assert!(matches!(rescan(&config, &root.join("other.txt")), Rescan::Found(found) if found.is_empty()));
        // The folder itself gone (an unplugged drive): nothing to conclude about what's in it.
        std::fs::rename(&root, temp.0.join("unplugged")).unwrap();
        assert!(matches!(rescan(&config, &root), Rescan::Unavailable));
        assert!(matches!(rescan(&config, &root.join("note.txt")), Rescan::Unavailable));
    }

    #[test]
    fn a_folder_that_became_a_symlink_isnt_followed() {
        let temp = TempFolder::new("symlink");
        let (root, elsewhere) = (temp.0.join("root"), temp.0.join("elsewhere"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("note.txt"), "not in a chosen folder").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();
        let config = config(&root);
        assert!(crawl(&config).found.is_empty());
        assert!(matches!(rescan(&config, &root.join("link/note.txt")), Rescan::Skipped));
    }
}
