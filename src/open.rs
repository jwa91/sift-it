//! Opening a result where it matched: a recording or video at its moment (QuickTime), a PDF at its page (Preview),
//! text and code at the line ($VISUAL or $EDITOR: vim, nvim, helix, emacs and nano in the terminal, VS Code, Cursor,
//! Zed and Sublime Text at the line), anything else in its app. `sift-it search` remembers its results, so
//! `sift-it open 2` opens the second.

use crate::config;
use crate::crawl;
use anyhow::{Context, Result, bail};
use protocol::Hit;
use protocol::Kind;
use protocol::Segment;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a result matched: enough to open it there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Place {
    pub path: PathBuf,
    pub kind: Kind,
    pub segment: Option<Segment>,
    pub loc: Option<f64>,
}

impl Place {
    pub fn of(hit: &Hit) -> Self {
        Self { path: PathBuf::from(&hit.path), kind: hit.kind, segment: Some(hit.segment), loc: hit.loc }
    }

    /// A file named on the command line, with a place in it (a page, a second, a line) or not.
    pub fn file(path: &Path, at: Option<f64>) -> Result<Self> {
        if !path.exists() {
            bail!("no such file: {}", path.display());
        }
        let sources = config::Config::load().unwrap_or_default().sources;
        let kind = sources.kind_of(&crawl::extension_of(path)).unwrap_or(Kind::Document);
        Ok(Self { path: path.to_path_buf(), kind, segment: None, loc: at })
    }
}

#[derive(Serialize, Deserialize)]
struct LastSearch {
    query: String,
    places: Vec<Place>,
}

fn last_search_path() -> PathBuf {
    config::home().join("last-search.json")
}

/// Keeps a search's results, in order, for `sift-it open <n>`.
pub fn remember(query: &str, hits: &[Hit]) {
    let last = LastSearch { query: query.to_string(), places: hits.iter().map(Place::of).collect() };
    if let Ok(bytes) = serde_json::to_vec(&last) {
        let _ = std::fs::create_dir_all(config::home());
        let _ = config::write_atomically(&last_search_path(), &bytes);
    }
}

/// Result `n` (from 1) of the last search.
pub fn recall(n: usize) -> Result<Place> {
    let bytes = std::fs::read(last_search_path()).context("no search to open from yet: run sift-it search first")?;
    let last: LastSearch = serde_json::from_slice(&bytes)?;
    if n == 0 || n > last.places.len() {
        bail!("the last search (\"{}\") had {} results", last.query, last.places.len());
    }
    Ok(last.places[n - 1].clone())
}

/// Opens the file where it matched, or shows it in Finder. Says what it did.
pub fn open(place: &Place, reveal: bool) -> Result<String> {
    let name = place.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if reveal {
        run("open", &["-R".as_ref(), place.path.as_os_str()])?;
        return Ok(format!("Showing {name} in Finder"));
    }
    let loc = place.loc;
    match (place.kind, loc) {
        (Kind::Audio | Kind::Video, Some(seconds)) => {
            if play_at(&place.path, seconds).is_ok() {
                return Ok(format!("Playing {name} from {}", timestamp(seconds)));
            }
            open_file(&place.path)?;
            Ok(format!("Opened {name}; the match is at {}", timestamp(seconds)))
        }
        (Kind::Document, Some(page)) if page > 1.0 && crawl::is_pdf(&place.path) => {
            if open_pdf_at(&place.path, page as usize).is_ok() {
                return Ok(format!("Opened {name} at page {page}"));
            }
            run("open", &["-a".as_ref(), "Preview".as_ref(), place.path.as_os_str()])?;
            Ok(format!("Opened {name}; the match is on page {page} (Preview's Go to Page needs Accessibility access)"))
        }
        (Kind::Plaintext | Kind::Code, Some(line)) if !place.path.to_string_lossy().ends_with(".ipynb") => {
            let line = line.max(1.0) as usize;
            if let Some(editor) = open_in_editor(&place.path, line)? {
                return Ok(format!("Opened {name} at line {line} in {editor}"));
            }
            open_file(&place.path)?;
            Ok(format!("Opened {name}; the match is at line {line} (set $EDITOR to open at the line)"))
        }
        _ => {
            open_file(&place.path)?;
            Ok(format!("Opened {name}"))
        }
    }
}

fn open_file(path: &Path) -> Result<()> {
    run("open", &[path.as_os_str()])
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let status = Command::new(program).args(args).status().with_context(|| format!("can't run {program}"))?;
    if !status.success() {
        bail!("{program} failed ({status})");
    }
    Ok(())
}

fn osascript(script: &str) -> Result<()> {
    let output = Command::new("osascript").arg("-e").arg(script).output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// An AppleScript string literal.
fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
}

/// QuickTime Player, at the moment (macOS asks once whether the terminal may control QuickTime).
fn play_at(path: &Path, seconds: f64) -> Result<()> {
    osascript(&format!(
        "tell application \"QuickTime Player\"
           activate
           set theDocument to open POSIX file {}
           delay 0.3
           set current time of theDocument to {}
           play theDocument
         end tell",
        quoted(path),
        seconds.max(0.0)
    ))
}

/// Preview, then its Go to Page (⌥⌘G): needs the terminal to be allowed to control the computer (Accessibility).
fn open_pdf_at(path: &Path, page: usize) -> Result<()> {
    osascript(&format!(
        "tell application \"Preview\"
           activate
           open POSIX file {}
         end tell
         delay 0.6
         tell application \"System Events\" to tell process \"Preview\"
           keystroke \"g\" using {{option down, command down}}
           delay 0.3
           keystroke \"{page}\"
           key code 36
         end tell",
        quoted(path)
    ))
}

/// Opens the file at the line in $VISUAL or $EDITOR, when it's an editor that can: returns its name.
fn open_in_editor(path: &Path, line: usize) -> Result<Option<String>> {
    let Some(editor) = std::env::var("VISUAL").ok().or_else(|| std::env::var("EDITOR").ok()) else { return Ok(None) };
    let mut words = editor.split_whitespace();
    let Some(program) = words.next() else { return Ok(None) };
    // Waiting flags are for git commit messages, not for this.
    let extra: Vec<&str> = words.filter(|w| *w != "--wait" && *w != "-w").collect();
    let name = Path::new(program).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let file = path.to_string_lossy().to_string();
    let args: Vec<String> = match name.as_str() {
        // In the terminal: they take over until you quit them.
        "vim" | "nvim" | "vi" | "nano" | "emacs" | "kak" | "micro" => vec![format!("+{line}"), file],
        "hx" | "helix" => vec![format!("{file}:{line}")],
        // Apps: they open at the line and return.
        "code" | "cursor" | "windsurf" | "codium" => vec!["-g".into(), format!("{file}:{line}")],
        "zed" | "subl" => vec![format!("{file}:{line}")],
        _ => return Ok(None),
    };
    let status =
        Command::new(program).args(&extra).args(&args).status().with_context(|| format!("can't run {program}"))?;
    if !status.success() {
        bail!("{program} failed ({status})");
    }
    Ok(Some(name))
}

pub fn timestamp(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
