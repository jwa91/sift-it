//! Which files get indexed, and finding them. Only the chosen folders are read, and in them never hidden folders,
//! app bundles and libraries, build output, dependency folders, keys and other secrets, iCloud files that are only
//! in the cloud (reading those would download them), or what `sources.ignore` leaves out.

use crate::config::{Config, Settings};
use crate::readers;
use protocol::Kind;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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
    /// What it's read with (config.json's reading values, resolved for it).
    pub settings: Settings,
}

impl Found {
    /// How it's read (readers::fingerprint): when this changes, it's read again.
    pub fn reading(&self) -> String {
        readers::fingerprint(&self.path, self.kind, &self.settings)
    }

    /// What's said in it is to be written down.
    pub fn speech(&self) -> bool {
        readers::transcribes(self.kind, &self.settings)
    }
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
    for root in &config.sources.paths {
        if config.sources.ignore.iter().any(|pattern| pattern.matches(root)) {
            continue;
        }
        match std::fs::read_dir(root) {
            Ok(_) => walk(root, config, false, &mut crawl.found),
            Err(_) => crawl.unavailable.push(root.clone()),
        }
    }
    // A chosen folder inside another one: its files were found twice.
    let mut seen = std::collections::HashSet::new();
    crawl.found.retain(|file| seen.insert(file.path.clone()));
    crawl
}

fn walk(folder: &Path, config: &Config, in_project: bool, found: &mut Vec<Found>) {
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
        if file_type.is_symlink() || config.sources.ignore.iter().any(|pattern| pattern.matches_itself(&path)) {
            continue;
        }
        if file_type.is_dir() {
            if is_skipped_folder(&path, in_project) {
                continue;
            }
            walk(&path, config, in_project, found);
        } else if file_type.is_file()
            && let Some(file) = consider(&path, &name, config, in_project)
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
    /// In a chosen folder, but somewhere never read (a hidden or skipped folder, an ignored one).
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
        config.sources.paths.iter().filter(|root| path.starts_with(root)).max_by_key(|root| root.as_os_str().len())
    else {
        return Rescan::Outside;
    };
    if std::fs::read_dir(root).is_err() {
        return Rescan::Unavailable;
    }
    if config.sources.ignore.iter().any(|pattern| pattern.matches(path)) {
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
    let Ok(metadata) = std::fs::symlink_metadata(path) else { return Rescan::Found(Vec::new()) };
    let mut found = Vec::new();
    if metadata.is_dir() {
        if path != root.as_path() && is_skipped_folder(path, in_project) {
            return Rescan::Skipped;
        }
        walk(path, config, in_project, &mut found);
    } else if metadata.is_file() {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') {
            return Rescan::Skipped;
        }
        found.extend(consider(path, &name, config, in_project));
    }
    Rescan::Found(found)
}

/// The file, if it's one to index.
fn consider(path: &Path, name: &str, config: &Config, in_project: bool) -> Option<Found> {
    let extension = extension_of(path);
    if is_secret(name, &extension) {
        return None;
    }
    let kind = config.sources.kind_of(&extension)?;
    // In a code project only pictures, PDFs and recordings are read, unless code is: its text files and documents
    // are READMEs, fixtures and data, which would bury everything else.
    let prose = kind == Kind::Plaintext || (kind == Kind::Document && !is_pdf(path));
    if (kind == Kind::Code || (in_project && prose)) && !config.sources.code {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if is_cloud_only(&metadata) || metadata.len() == 0 {
        return None;
    }
    let kind = if kind == Kind::Image && is_screenshot(path, name) { Kind::Screenshot } else { kind };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    Some(Found {
        path: path.to_path_buf(),
        kind,
        size: metadata.len(),
        modified,
        settings: config.reading.resolve(path, kind),
    })
}

pub fn extension_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// PDFs are documents read page by page (PDFKit), where other documents are text (textutil).
pub fn is_pdf(path: &Path) -> bool {
    extension_of(path) == "pdf"
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

/// A file's kind, by its type (`sources.kinds`), a picture of the screen being a screenshot. None for a type that
/// isn't read.
pub fn kind_of(config: &Config, path: &Path) -> Option<Kind> {
    let kind = config.sources.kind_of(&extension_of(path))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Some(if kind == Kind::Image && is_screenshot(path, &name) { Kind::Screenshot } else { kind })
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
        let mut config = Config::default();
        config.sources.paths = vec![root.to_path_buf()];
        config
    }

    fn found(config: &Config) -> Vec<String> {
        let mut names: Vec<String> =
            crawl(config).found.iter().map(|file| file.path.file_name().unwrap().to_string_lossy().into()).collect();
        names.sort();
        names
    }

    #[test]
    fn ignored_files_and_folders_are_left_out() {
        let temp = TempFolder::new("ignore");
        let root = temp.0.join("root");
        std::fs::create_dir_all(root.join("Old/2019")).unwrap();
        std::fs::write(root.join("cover.psd.png"), "x").unwrap();
        std::fs::write(root.join("note.txt"), "a note").unwrap();
        std::fs::write(root.join("Old/2019/old.txt"), "an old note").unwrap();
        let mut config = config(&root);
        assert_eq!(found(&config), ["cover.psd.png", "note.txt", "old.txt"]);
        config.set("sources.ignore", r#"["old", "*.psd.*"]"#).unwrap();
        assert_eq!(found(&config), ["note.txt"]);
        assert!(matches!(rescan(&config, &root.join("Old/2019/old.txt")), Rescan::Skipped));
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
