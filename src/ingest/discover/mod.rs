//! Discovering: which files are new or changed, or are to be read another way (their fingerprint, ingest::fingerprint),
//! each queued with an estimate of what reading it costs (cost.rs); files that are gone leave the index. `sync`
//! crawls the chosen folders; `sync_paths` looks at just the paths a file-system event named.

mod cost;
pub mod crawl;

pub use cost::{cost, speech};

use super::Event;
use crate::config::Config;
use crate::index::{FileRow, Writer};
use anyhow::Result;
use crawl::{Found, Rescan};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Crawls the chosen folders and brings the queue in step: new and changed files are queued, gone ones leave the index.
pub fn sync(index: &Writer, config: &Config) -> Result<Event> {
    let started = Instant::now();
    let crawl = crawl::crawl(config);
    let (added, changed, removed) = index.batch(|| {
        // What's in a folder that can't be read now stays.
        let unavailable = |path: &Path| crawl.unavailable.iter().any(|root| path.starts_with(root));
        let (added, changed, removed) = reconcile(index, &crawl.found, index.files()?, unavailable)?;
        // Files where some piece couldn't be embedded, or reading got stuck: once in a while, try them again.
        Ok((added, changed + index.requeue_passing(6.0)?, removed))
    })?;
    Ok(Event::Sync {
        files: crawl.found.len(),
        added,
        changed,
        removed,
        unavailable: crawl.unavailable.iter().map(|p| p.display().to_string()).collect(),
        pending: index.pending_count()?,
        ms: started.elapsed().as_millis() as u64,
    })
}

/// A sync of just these paths (files, or folders with everything in them).
pub fn sync_paths(index: &Writer, config: &Config, paths: &[PathBuf]) -> Result<Event> {
    let started = Instant::now();
    let (mut files, mut added, mut changed, mut removed) = (0, 0, 0, 0);
    index.batch(|| {
        for path in paths {
            let found = match crawl::rescan(config, path) {
                Rescan::Outside | Rescan::Unavailable => continue,
                Rescan::Skipped => Vec::new(),
                Rescan::Found(found) => found,
            };
            let (a, c, r) = reconcile(index, &found, index.files_under(path)?, |_| false)?;
            files += found.len();
            added += a;
            changed += c;
            removed += r;
        }
        Ok(())
    })?;
    Ok(Event::Sync {
        files,
        added,
        changed,
        removed,
        unavailable: Vec::new(),
        pending: index.pending_count()?,
        ms: started.elapsed().as_millis() as u64,
    })
}

/// Brings the index in step with what was `found` where `existing` was: new and changed files are queued, and those not
/// found leave, unless `kept`. Returns how many were added, changed and removed.
fn reconcile(
    index: &Writer,
    found: &[Found],
    existing: Vec<FileRow>,
    kept: impl Fn(&Path) -> bool,
) -> Result<(usize, usize, usize)> {
    let existing: HashMap<PathBuf, FileRow> = existing.into_iter().map(|row| (row.path.clone(), row)).collect();
    let (mut added, mut changed, mut removed) = (0, 0, 0);
    for file in found {
        match existing.get(&file.path) {
            None => {
                index.queue(&file.new_file(), None, cost(file))?;
                added += 1;
            }
            Some(row) if is_changed(row, file) => {
                index.queue(&file.new_file(), Some(row.id), cost(file))?;
                changed += 1;
            }
            Some(row) if row.speech != file.speech() => index.set_speech(row.id, file.speech())?,
            Some(_) => {}
        }
    }
    let found: HashSet<&Path> = found.iter().map(|file| file.path.as_path()).collect();
    for (path, row) in &existing {
        if !found.contains(path.as_path()) && !kept(path) {
            index.remove_file(row.id)?;
            removed += 1;
        }
    }
    Ok((added, changed, removed))
}

/// Changed, or to be read another way.
fn is_changed(row: &FileRow, found: &Found) -> bool {
    row.size != found.size
        || (row.modified - found.modified).abs() > 0.001
        || row.kind != found.kind
        || row.reading != found.reading()
}
