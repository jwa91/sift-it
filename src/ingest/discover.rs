//! Discovering: which files are new or changed, or are to be read another way (their fingerprint, ingest::fingerprint),
//! each queued with an estimate of what reading it costs (estimate::cost); files that are gone leave the index. `sync`
//! crawls the chosen folders; `sync_paths` looks at just the paths a file-system event named.

use super::Event;
use crate::config::Config;
use crate::crawl::{self, Found, Rescan};
use crate::index::{FileRow, Writer};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

/// Crawls the chosen folders and brings the queue in step: new and changed files are queued, gone ones leave the index.
pub fn sync(index: &Writer, config: &Config) -> Result<Event> {
    let started = Instant::now();
    let crawl = crawl::crawl(config);
    let existing: HashMap<PathBuf, FileRow> = index.files()?.into_iter().map(|row| (row.path.clone(), row)).collect();
    let found_paths: HashSet<&PathBuf> = crawl.found.iter().map(|f| &f.path).collect();
    let (added, changed, removed) = index.batch(|| {
        let (added, mut changed) = queue_found(index, &crawl.found, &existing)?;
        // Files where some piece couldn't be embedded, or reading got stuck: once in a while, try them again.
        changed += index.requeue_passing(6.0)?;
        let mut removed = 0;
        for (path, row) in &existing {
            if found_paths.contains(path) || crawl.unavailable.iter().any(|root| path.starts_with(root)) {
                continue;
            }
            index.remove_file(row.id)?;
            removed += 1;
        }
        Ok((added, changed, removed))
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
            let existing: HashMap<PathBuf, FileRow> =
                index.files_under(path)?.into_iter().map(|row| (row.path.clone(), row)).collect();
            let (a, c) = queue_found(index, &found, &existing)?;
            let found_paths: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
            for (existing_path, row) in &existing {
                if !found_paths.contains(existing_path) {
                    index.remove_file(row.id)?;
                    removed += 1;
                }
            }
            files += found.len();
            added += a;
            changed += c;
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

fn queue_found(index: &Writer, found: &[Found], existing: &HashMap<PathBuf, FileRow>) -> Result<(usize, usize)> {
    let (mut added, mut changed) = (0, 0);
    for file in found {
        match existing.get(&file.path) {
            None => {
                index.queue(&file.new_file(), None, crate::estimate::cost(file))?;
                added += 1;
            }
            Some(row) if is_changed(row, file) => {
                index.queue(&file.new_file(), Some(row.id), crate::estimate::cost(file))?;
                changed += 1;
            }
            Some(row) if row.speech != file.speech() => index.set_speech(row.id, file.speech())?,
            Some(_) => {}
        }
    }
    Ok((added, changed))
}

/// Changed, or to be read another way.
fn is_changed(row: &FileRow, found: &Found) -> bool {
    row.size != found.size
        || (row.modified - found.modified).abs() > 0.001
        || row.kind != found.kind
        || row.reading != found.reading()
}
