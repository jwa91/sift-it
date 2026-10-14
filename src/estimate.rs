//! `sift-it estimate <folder>...`: what indexing a folder means, before it starts: how many files of each kind, and about
//! how long the first pass takes. The costs are measured ones (an M5; `sift-it bench-embed` measures yours): text at about
//! 10,000 tokens a second, a photo in 0.15 s, a screenshot (picture, text and OCR) in 0.5 s, 30 s of sound in 0.25 s.

use crate::config::Config;
use crate::crawl::{self, Kind};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const TOKENS_PER_SECOND: f64 = 10_000.0;
const PHOTO: f64 = 0.15;
const SCREENSHOT: f64 = 0.5;
/// A PDF page: about 300 tokens of text, and reading it.
const PDF_PAGE: f64 = 300.0 / TOKENS_PER_SECOND + 0.004;
/// Recordings: a 30 s window every 20 s, at 0.25 s each. Video adds a frame every shot (about every 15 s here).
const SOUND_PER_SECOND: f64 = 0.25 / 20.0;
const FRAMES_PER_SECOND: f64 = 0.07 / 15.0;

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
    single.folders = vec![folder.clone()];
    let crawl = crawl::crawl(&single);
    let mut estimate = Estimate {
        folder,
        files: crawl.found.len(),
        bytes: 0,
        by_kind: BTreeMap::new(),
        seconds: 0.0,
        unreadable: !crawl.unavailable.is_empty(),
    };
    for file in &crawl.found {
        estimate.bytes += file.size;
        *estimate.by_kind.entry(file.kind.as_str()).or_default() += 1;
        estimate.seconds += match file.kind {
            // Plain text: about 4 bytes a token.
            Kind::Text | Kind::Code => file.size as f64 / 4.0 / TOKENS_PER_SECOND,
            // Documents are mostly packaging (zip, markup): count a tenth of them as text, and textutil's start.
            Kind::Doc => file.size as f64 / 40.0 / TOKENS_PER_SECOND + 0.05,
            Kind::Pdf => {
                objc2::rc::autoreleasepool(|_| crate::mac::pdf_pages(&file.path)).unwrap_or(1) as f64 * PDF_PAGE
            }
            Kind::Image => PHOTO,
            Kind::Screenshot => SCREENSHOT,
            Kind::Audio => crate::av::duration(&file.path) * SOUND_PER_SECOND,
            Kind::Video => crate::av::duration(&file.path) * (SOUND_PER_SECOND + FRAMES_PER_SECOND),
        };
    }
    estimate
}

/// "about 12 minutes", "under a minute", "about 2 hours".
pub fn describe(seconds: f64) -> String {
    match seconds {
        s if s < 60.0 => "under a minute".into(),
        s if s < 90.0 * 60.0 => format!("about {} minutes", (s / 60.0).round().max(1.0)),
        s => format!("about {:.0} hours", (s / 3600.0).round().max(1.0)),
    }
}
