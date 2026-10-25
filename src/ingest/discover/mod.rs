//! Discovering: which files are new or changed, or are to be read another way (their fingerprint, ingest::fingerprint),
//! each queued for its words with an estimate of what reading them costs (cost.rs); files that are gone leave the
//! index. `sync` crawls the chosen folders; `sync_paths` looks at just the paths a file-system event named, and what it
//! queues is new: it goes first in every round it needs. A conversation's log that grew is read on from where its last
//! read ended.
//!
//! A file only in iCloud is never read, and its work waits for its download (Writer::wait_for_download): one never read
//! is found by its name meanwhile, and one macOS removed the download of keeps what was read of it. Removing the
//! download keeps its size and time, so when it's back, the same file goes on where it was.

mod cost;
pub mod crawl;

pub use cost::{cost, shares};

use super::Event;
use super::conversation;
use crate::config::Config;
use crate::index::{FileRow, NewFile, Writer};
use anyhow::Result;
use crawl::{Found, Rescan};
use protocol::Kind;
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
        let (added, changed, removed) = reconcile(index, &crawl.found, index.files()?, unavailable, false)?;
        // Files where some piece couldn't be embedded, or reading got stuck: once in a while, try them again.
        Ok((added, changed + index.requeue_passing(6.0)?, removed))
    })?;
    Ok(Event::Sync {
        files: crawl.found.len(),
        added,
        changed,
        removed,
        unavailable: crawl.unavailable.iter().map(|p| p.display().to_string()).collect(),
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
            let (a, c, r) = reconcile(index, &found, index.files_under(path)?, |_| false, true)?;
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
        ms: started.elapsed().as_millis() as u64,
    })
}

/// Brings the index in step with what was `found` where `existing` was: new and changed files are queued (as `new`
/// files, first in every round, when FSEvents said they changed), and those not found leave, unless `kept`. Returns
/// how many were added, changed and removed.
fn reconcile(
    index: &Writer,
    found: &[Found],
    existing: Vec<FileRow>,
    kept: impl Fn(&Path) -> bool,
    new: bool,
) -> Result<(usize, usize, usize)> {
    let existing: HashMap<PathBuf, FileRow> = existing.into_iter().map(|row| (row.path.clone(), row)).collect();
    let waiting = index.waiting_for_download()?;
    let (mut added, mut changed, mut removed) = (0, 0, 0);
    for file in found {
        match existing.get(&file.path) {
            None => {
                queue(index, file, None, new)?;
                added += 1;
            }
            // Only in iCloud, and the same file: what was read of it stays, and the rest waits. Whether it's to be read
            // another way is seen once it's back.
            Some(row) if file.cloud && !is_edited(row, file) => {
                if !waiting.contains(&row.id) {
                    index.wait_for_download(row.id)?;
                }
            }
            Some(row) if is_changed(row, file) => {
                if grown(index, row, file)? {
                    index.queue_grown(row.id, &NewFile { new, ..file.new_file() }, cost(file))?;
                } else {
                    queue(index, file, Some(row.id), new)?;
                }
                changed += 1;
            }
            Some(row) => {
                if waiting.contains(&row.id) {
                    index.downloaded(row.id, new)?;
                }
                if row.speech != file.speech() {
                    index.set_speech(row.id, file.speech())?;
                }
            }
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

/// Queues a file for its words (as a `new` one, first in every round, when FSEvents said it changed). One only in iCloud
/// waits for its download.
fn queue(index: &Writer, file: &Found, existing: Option<i64>, new: bool) -> Result<()> {
    let id = index.queue(&NewFile { new, ..file.new_file() }, existing, cost(file))?;
    if file.cloud {
        index.wait_for_download(id)?;
    }
    Ok(())
}

/// A conversation's log that grew as its session went on, read the same way, and still what it was up to where its
/// last read ended: the next read goes on from there. One only in iCloud isn't looked at.
fn grown(index: &Writer, row: &FileRow, found: &Found) -> Result<bool> {
    if found.cloud || found.kind != Kind::Conversation || row.kind != found.kind || row.reading != found.reading() {
        return Ok(false);
    }
    Ok(index.words_resume(row.id)?.is_some_and(|resume| conversation::goes_on(&found.path, &resume)))
}

/// Another file than the one indexed: it changed. While it's only in iCloud, a conversation's log stays one: what
/// tells a log, its first lines, can't be read meanwhile, so it's found by its type.
fn is_edited(row: &FileRow, found: &Found) -> bool {
    let kind = row.kind != found.kind && !(found.cloud && row.kind == Kind::Conversation);
    row.size != found.size || (row.modified - found.modified).abs() > 0.001 || kind
}

/// Changed, or to be read another way.
fn is_changed(row: &FileRow, found: &Found) -> bool {
    is_edited(row, found) || row.reading != found.reading()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Depth, Settings};
    use crate::index::NewSegment;
    use crate::index::testing::TempIndex;
    use protocol::{Kind, Round, Segment};
    use serde_json::json;

    fn found(path: &str, kind: Kind, cloud: bool) -> Found {
        Found { path: path.into(), kind, size: 100, modified: 1.0, settings: Settings::BASE, cloud }
    }

    /// Brings the index in step with `found`, as a crawl does: how many files were added, changed and removed.
    fn sync(index: &Writer, found: &[Found]) -> (usize, usize, usize) {
        index.batch(|| reconcile(index, found, index.files()?, |_| false, false)).unwrap()
    }

    /// The files with `word` in their name or their words.
    fn with(index: &Writer, word: &str) -> usize {
        let mut files = HashSet::new();
        index.word_matches(&[word.to_string()], false, |_, file| files.insert(file) || true).unwrap();
        files.len()
    }

    fn passage(text: &str) -> NewSegment {
        NewSegment { segment: Segment::Passage, text: Some(text.into()), ..Default::default() }
    }

    #[test]
    fn a_file_only_in_icloud_is_found_by_its_name_and_read_once_its_downloaded() {
        let temp = TempIndex::new("cloud-new");
        let index = Writer::open(&temp.path).unwrap();
        let notes = found("/iCloud/quokka-notes.txt", Kind::Plaintext, true);
        assert_eq!(sync(&index, std::slice::from_ref(&notes)), (1, 0, 0));
        // Nothing is read of it, and indexing has nothing left to do.
        assert_eq!(with(&index, "quokka"), 1);
        assert!(index.queues().unwrap().is_empty());
        assert!(index.rounds().unwrap().is_empty());
        let stats = index.stats().unwrap();
        assert_eq!((stats.files, stats.cloud, stats.pending, stats.done), (1, 1, 0, 0));
        // Downloaded: its words are read, and nothing else about it changed.
        assert_eq!(sync(&index, &[Found { cloud: false, ..notes }]), (0, 0, 0));
        assert_eq!(index.next(Round::Words, false, 10).unwrap().len(), 1);
        assert_eq!(index.stats().unwrap().cloud, 0);
    }

    #[test]
    fn a_file_whose_download_was_removed_keeps_what_was_read() {
        let temp = TempIndex::new("cloud-evicted");
        let mut index = Writer::open(&temp.path).unwrap();
        let memo = found("/iCloud/voice-memo.m4a", Kind::Audio, false);
        sync(&index, std::slice::from_ref(&memo));
        // Its words are read, and what's said in it waits to be written down.
        let words = index.next(Round::Words, false, 10).unwrap().remove(0);
        let read = [passage("Kangaroo Island")];
        assert!(
            index
                .write_words(&words, &read, &json!({"duration": 60.0}), &[(Round::Speech, 60.0)], false, None)
                .unwrap()
        );
        // macOS removed its download: it stays as it was, and its speech waits for the download.
        let evicted = Found { cloud: true, ..memo.clone() };
        assert_eq!(sync(&index, std::slice::from_ref(&evicted)), (0, 0, 0));
        assert_eq!(with(&index, "kangaroo"), 1);
        assert!(index.next(Round::Speech, false, 10).unwrap().is_empty());
        assert!(index.rounds().unwrap().iter().all(|totals| totals.round != Round::Speech));
        assert_eq!(index.stats().unwrap().cloud, 1);
        // To be read another way meanwhile: that's seen once it's back.
        let lower = Found { settings: Settings { depth: Depth::Words, ..Settings::BASE }, ..evicted };
        assert_eq!(sync(&index, &[lower]), (0, 0, 0));
        assert_eq!(with(&index, "kangaroo"), 1);
        // Downloaded again: its speech goes on, and nothing is read again.
        assert_eq!(sync(&index, &[memo]), (0, 0, 0));
        assert_eq!(index.next(Round::Speech, false, 10).unwrap().len(), 1);
        assert!(index.next(Round::Words, false, 10).unwrap().is_empty());
        assert_eq!(with(&index, "kangaroo"), 1);
        assert_eq!(index.stats().unwrap().cloud, 0);
    }

    #[test]
    fn a_conversation_whose_download_was_removed_stays_one() {
        let temp = TempIndex::new("cloud-conversation");
        let mut index = Writer::open(&temp.path).unwrap();
        let log = found("/iCloud/session.jsonl", Kind::Conversation, false);
        sync(&index, std::slice::from_ref(&log));
        let words = index.next(Round::Words, false, 10).unwrap().remove(0);
        let read =
            [NewSegment { segment: Segment::Exchange, loc: Some(1.0), ..passage("User: where do wombats sleep?") }];
        let resume = json!({ "from": 1.0 });
        assert!(index.write_words(&words, &read, &json!({}), &[], false, Some(&resume)).unwrap());
        // Only in iCloud, a log can't be told from other JSON Lines: it's found by its type. Nothing changes.
        let evicted = Found { kind: Kind::Structured, cloud: true, ..log.clone() };
        assert_eq!(sync(&index, &[evicted]), (0, 0, 0));
        assert_eq!(with(&index, "wombats"), 1);
        assert_eq!(index.files().unwrap()[0].kind, Kind::Conversation);
        // Downloaded again: nothing is read again, and it goes on from where it was when it grows.
        assert_eq!(sync(&index, &[log]), (0, 0, 0));
        assert!(index.next(Round::Words, false, 10).unwrap().is_empty());
        assert_eq!(index.words_resume(index.files().unwrap()[0].id).unwrap(), Some(resume));
    }

    #[test]
    fn a_file_changed_while_only_in_icloud_is_found_by_its_name_till_its_downloaded() {
        let temp = TempIndex::new("cloud-changed");
        let mut index = Writer::open(&temp.path).unwrap();
        let notes = found("/iCloud/notes.txt", Kind::Plaintext, false);
        sync(&index, std::slice::from_ref(&notes));
        let words = index.next(Round::Words, false, 10).unwrap().remove(0);
        assert!(index.write_words(&words, &[passage("the quokka smiles")], &json!({}), &[], false, None).unwrap());
        // Changed on another device, and not downloaded here: what was read of it is gone, its name stays.
        let changed = Found { size: 200, modified: 2.0, cloud: true, ..notes };
        assert_eq!(sync(&index, std::slice::from_ref(&changed)), (0, 1, 0));
        assert_eq!((with(&index, "quokka"), with(&index, "notes")), (0, 1));
        assert!(index.next(Round::Words, false, 10).unwrap().is_empty());
        // Downloaded: read again.
        assert_eq!(sync(&index, &[Found { cloud: false, ..changed }]), (0, 0, 0));
        assert_eq!(index.next(Round::Words, false, 10).unwrap().len(), 1);
    }
}
