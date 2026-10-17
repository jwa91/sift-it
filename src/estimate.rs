//! `sift-it estimate <folder>...`: what indexing a folder means, before it starts: how many files of each kind, and about
//! how long the first pass takes. The costs are measured ones (an M5; `sift-it bench-embed` measures yours): text at about
//! 10,000 tokens a second, a photo in 0.15 s, a screenshot (picture, text and OCR) in 0.5 s, 30 s of sound in 0.25 s.

use crate::config::Config;
use crate::crawl::{self, Found};
use protocol::Kind;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const TOKENS_PER_SECOND: f64 = 10_000.0;
/// Text that's only cut into passages, not embedded.
const CUT_BYTES_PER_SECOND: f64 = 100e6;
const PHOTO: f64 = 0.15;
const SCREENSHOT: f64 = 0.5;
/// Reading the text on a screenshot, without embedding it.
const OCR: f64 = 0.35;
/// A PDF page: about 300 tokens of text, and reading it.
const PDF_PAGE: f64 = 300.0 / TOKENS_PER_SECOND + 0.004;
const PDF_PAGE_TEXT: f64 = 0.004;
/// Recordings: a 30 s window every 20 s, at 0.25 s each. Video adds a frame every shot (about every 15 s here).
const SOUND_PER_SECOND: f64 = 0.25 / 20.0;
const FRAMES_PER_SECOND: f64 = 0.07 / 15.0;
/// Opening a recording for its tags and length.
const OPEN: f64 = 0.02;
/// textutil starting.
const TEXTUTIL: f64 = 0.05;

/// About how long reading a file takes, in seconds, at its depth: what orders the queue (cheapest first) and what
/// `sift-it status` says is left. Writing down speech comes after, and isn't in it.
pub fn cost(file: &Found) -> f64 {
    let embeds = file.settings.embeds();
    let size = file.size as f64;
    match file.kind {
        // Plain text: about 4 bytes a token.
        Kind::Plaintext | Kind::Code if embeds => size / 4.0 / TOKENS_PER_SECOND,
        Kind::Plaintext | Kind::Code => size / CUT_BYTES_PER_SECOND,
        Kind::Document if crawl::is_pdf(&file.path) => {
            let pages = objc2::rc::autoreleasepool(|_| crate::mac::pdf_pages(&file.path)).unwrap_or(1) as f64;
            pages * if embeds { PDF_PAGE } else { PDF_PAGE_TEXT }
        }
        // Other documents are mostly packaging (zip, markup): count a tenth of them as text, and textutil's start.
        Kind::Document if embeds => TEXTUTIL + size / 40.0 / TOKENS_PER_SECOND,
        Kind::Document => TEXTUTIL + size / 10.0 / CUT_BYTES_PER_SECOND,
        Kind::Image if embeds => PHOTO,
        Kind::Image => 0.0,
        Kind::Screenshot if embeds => SCREENSHOT,
        Kind::Screenshot => OCR,
        Kind::Audio if embeds => crate::av::duration(&file.path) * SOUND_PER_SECOND,
        Kind::Video if embeds => {
            let sound = if file.settings.sound { SOUND_PER_SECOND } else { 0.0 };
            crate::av::duration(&file.path) * (sound + FRAMES_PER_SECOND)
        }
        Kind::Audio | Kind::Video => OPEN,
    }
}

#[derive(Serialize)]
pub struct Estimate {
    pub folder: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub by_kind: BTreeMap<&'static str, usize>,
    /// About how long the first pass takes.
    pub seconds: f64,
    /// The folder can't be read (missing, or macOS privacy settings).
    pub unreadable: bool,
}

pub fn estimate(config: &Config, folder: PathBuf) -> Estimate {
    let mut single = config.clone();
    single.sources.paths = vec![folder.clone()];
    let crawled = crawl::crawl(&single);
    let mut estimate = Estimate {
        folder,
        files: crawled.found.len(),
        bytes: 0,
        by_kind: BTreeMap::new(),
        seconds: 0.0,
        unreadable: !crawled.unavailable.is_empty(),
    };
    for file in &crawled.found {
        estimate.bytes += file.size;
        *estimate.by_kind.entry(file.kind.as_str()).or_default() += 1;
        estimate.seconds += cost(file);
    }
    estimate
}

/// "about 12 minutes", "under a minute", "about 2 hours".
pub fn describe(seconds: f64) -> String {
    match seconds {
        s if s < 60.0 => "under a minute".into(),
        s if s < 90.0 => "about a minute".into(),
        s if s < 90.0 * 60.0 => format!("about {} minutes", (s / 60.0).round()),
        s => format!("about {:.0} hours", (s / 3600.0).round()),
    }
}
