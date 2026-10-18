//! `sift-it estimate <folder>...`: what indexing a folder means, before it starts: how many files of each kind, and about
//! how long the first pass takes (ingest/discover/cost.rs).

use crate::config::Config;
use crate::ingest::discover::{cost, crawl};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
