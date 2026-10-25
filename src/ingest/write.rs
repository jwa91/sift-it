//! Writing: what an item of a round found in a file, put together from its pieces as they come, and written to the
//! index when the item ends: a file's words, or an item of its OCR, pictures or sound. A file's words are written in
//! parts of PART segments as they come, so memory doesn't grow with a long text, and a search finds what's in. A file
//! that changed (or left) while it was read isn't written: it's been queued again.

use super::Event;
use super::read::Ended;
use crate::adapters::macos::icloud;
use crate::index::{Done, FileRow, NewSegment, Work, Writer};
use anyhow::Result;
use protocol::Round;
use std::collections::HashMap;
use std::time::Instant;

/// Segments of a file's words kept before they're written as a part (Writer::write_words_part).
const PART: usize = 256;

/// An item: a round's work for a file, and where in it.
pub type Key = (i64, Round);

/// An item being put together from its pieces.
struct Assembly {
    work: Work,
    started: Instant,
    /// The segments not written yet.
    segments: Vec<NewSegment>,
    /// Segments written as parts, and how many of them have a vector.
    written: usize,
    vectors: usize,
    /// Pieces whose vector couldn't be made.
    failures: usize,
    /// The file changed (or left) while it was read: what's left of it isn't embedded or written.
    stale: bool,
}

/// The items of a run being put together, and how many files' rounds were written or failed.
#[derive(Default)]
pub struct Items {
    assemblies: HashMap<Key, Assembly>,
    pub files: usize,
    pub failed: usize,
}

impl Items {
    /// Reading an item started.
    pub fn start(&mut self, work: Work, started: Instant) {
        let key = (work.file.id, work.round);
        let assembly =
            Assembly { work, started, segments: Vec::new(), written: 0, vectors: 0, failures: 0, stale: false };
        self.assemblies.insert(key, assembly);
    }

    /// Whether an item is being put together, and still wanted: not once its file changed.
    pub fn keeps(&self, key: Key) -> bool {
        self.assemblies.get(&key).is_some_and(|assembly| !assembly.stale)
    }

    /// A segment of an item, and whether its vector couldn't be made.
    pub fn add(&mut self, key: Key, segment: NewSegment, failed: bool) {
        let Some(assembly) = self.assemblies.get_mut(&key) else { return };
        assembly.failures += failed as usize;
        assembly.segments.push(segment);
    }

    /// A long item's checkpoint (`at`): what it found so far is written, with how far it has got, and it goes on.
    /// Returns how much more of it is done, in the round's unit, when it was written.
    pub fn checkpoint(&mut self, key: Key, at: &Ended, index: &mut Writer) -> Result<Option<f64>> {
        let Some(assembly) = self.assemblies.get_mut(&key).filter(|a| !a.stale) else { return Ok(None) };
        if assembly.failures > 0 {
            // What it found so far has a gap: its end says so, and it's read again later.
            return Ok(None);
        }
        let before = assembly.work.done;
        let done = Done { done: at.done, resume: at.resume.clone(), ..Default::default() };
        let written = index.write_item(&assembly.work, &assembly.segments, &done)?;
        if written {
            assembly.written += assembly.segments.len();
            assembly.vectors += assembly.segments.iter().filter(|s| s.vector.is_some()).count();
            assembly.work.done = at.done;
            assembly.work.resume = at.resume.clone();
        } else {
            // It changed while it was read: it's queued again.
            assembly.stale = true;
        }
        assembly.segments.clear();
        Ok(written.then_some(at.done - before))
    }

    /// Writes a file's words as a part when PART of them are waiting.
    pub fn write_part(&mut self, key: Key, index: &mut Writer) -> Result<()> {
        let Some(assembly) = self.assemblies.get_mut(&key).filter(|a| !a.stale && a.segments.len() >= PART) else {
            return Ok(());
        };
        if index.write_words_part(&assembly.work, &assembly.segments, assembly.written == 0)? {
            assembly.written += assembly.segments.len();
            assembly.vectors += assembly.segments.iter().filter(|s| s.vector.is_some()).count();
        } else {
            // It changed while it was read: it's queued again, and read again from the start.
            assembly.stale = true;
        }
        assembly.segments.clear();
        Ok(())
    }

    /// Reading an item ended: what it found is written, with how far the file's work in its round is now, or its
    /// round failed. Returns how much of it was done, in the round's unit, when it was written.
    pub fn end(
        &mut self,
        key: Key,
        result: Result<Ended>,
        index: &mut Writer,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Option<f64>> {
        // A lane hands over an item's start, pieces and end in that order.
        let assembly = self.assemblies.remove(&key).expect("started");
        if assembly.stale {
            return Ok(None);
        }
        let Assembly { work, started, segments, written, vectors, failures, .. } = assembly;
        let path = work.file.path.display().to_string();
        let result = match result {
            // A vector that couldn't be made may well be made later: the item is read again then.
            Ok(_) if failures > 0 => Err(anyhow::anyhow!("{failures} of its pieces couldn't be embedded")),
            result => result,
        };
        let ended = match result {
            Ok(ended) => ended,
            // Reading it failed rather than download it: macOS removed its download meanwhile.
            Err(_) if failures == 0 && waits_for_download(index, &work.file)? => return Ok(None),
            Err(error) => {
                let error = format!("{error:#}");
                if index.fail(&work, &error, failures > 0)? {
                    self.failed += 1;
                    on_event(Event::Failed { round: work.round, path, error });
                }
                return Ok(None);
            }
        };
        let count = written + segments.len();
        let vectors = vectors + segments.iter().filter(|s| s.vector.is_some()).count();
        let stored = if work.round == Round::Words {
            index.write_words(&work, &segments, &ended.info, &ended.rounds, written > 0, ended.resume.as_ref())?
        } else {
            let done = Done { done: ended.done, resume: ended.resume, finished: ended.finished, info: ended.info };
            index.write_item(&work, &segments, &done)?
        };
        if !stored {
            return Ok(None);
        }
        if ended.finished {
            self.files += 1;
            let ms = started.elapsed().as_millis() as u64;
            on_event(Event::File { round: work.round, path, kind: work.file.kind, segments: count, vectors, ms });
        }
        Ok(Some(ended.done - work.done))
    }
}

/// Whether a file's work waits for its download: it's only in iCloud now (macOS removed its download since it was
/// found), and reading it would download it. Then its work waits (Writer::wait_for_download), and goes on once it's
/// downloaded again (discover).
pub fn waits_for_download(index: &Writer, file: &FileRow) -> Result<bool> {
    if !icloud::cloud_only_at(&file.path) {
        return Ok(false);
    }
    index.batch(|| index.wait_for_download(file.id))?;
    Ok(true)
}
