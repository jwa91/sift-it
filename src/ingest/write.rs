//! Writing: files put together from their segments as they come, and written to the index. A file is written once all
//! of it is in, or, for a big one, in parts of PART segments as they come: memory doesn't grow with a file, and a
//! search finds what's in. A file that changed (or left) while it was read isn't written: it's been queued again.

use super::Event;
use super::embed::Embedded;
use crate::index::{FileRow, NewSegment, Writer};
use anyhow::Result;
use std::collections::HashMap;
use std::time::Instant;

/// Segments a file keeps before they're written as a part (Writer::write_part): about 1.2 MB of vectors, in their three forms.
const PART: usize = 256;

/// A file being put together from its segments.
struct Assembly {
    row: FileRow,
    started: Instant,
    /// The segments not written yet.
    segments: Vec<NewSegment>,
    /// Segments written as parts, and how many of them have a vector.
    written: usize,
    vectors: usize,
    /// Pieces whose vector failed: they stay in the keyword index.
    failures: usize,
    /// The file changed (or left) while it was read: what's left of it isn't embedded or written.
    stale: bool,
}

impl Assembly {
    fn new(row: &FileRow, started: Instant) -> Self {
        Self { row: row.clone(), started, segments: Vec::new(), written: 0, vectors: 0, failures: 0, stale: false }
    }
}

/// The files of a run being put together, and how many were written, failed or ended.
#[derive(Default)]
pub struct Files {
    assemblies: HashMap<i64, Assembly>,
    pub indexed: usize,
    pub failed: usize,
    pub completed: usize,
}

impl Files {
    /// Reading a file started.
    pub fn start(&mut self, row: &FileRow, started: Instant) {
        self.assemblies.insert(row.id, Assembly::new(row, started));
    }

    /// Whether a file is being put together, and still wanted: not once it changed.
    pub fn keeps(&self, file: i64) -> bool {
        self.assemblies.get(&file).is_some_and(|assembly| !assembly.stale)
    }

    /// A segment of a file.
    pub fn add(&mut self, (file, segment, failed): Embedded) {
        let Some(assembly) = self.assemblies.get_mut(&file) else { return };
        assembly.failures += failed as usize;
        assembly.segments.push(segment);
    }

    /// Writes the segments of files that have PART of them waiting, as a part.
    pub fn write_parts(&mut self, index: &mut Writer) -> Result<()> {
        for assembly in self.assemblies.values_mut().filter(|a| !a.stale && a.segments.len() >= PART) {
            let first = assembly.written == 0;
            if index.write_part(&assembly.row, &assembly.segments, first)? {
                assembly.written += assembly.segments.len();
                assembly.vectors += assembly.segments.iter().filter(|s| s.vector.is_some()).count();
            } else {
                // It changed while it was read: it's queued again, and read again from the start.
                assembly.stale = true;
            }
            assembly.segments.clear();
        }
        Ok(())
    }

    /// Reading a file ended: what's left of it is written, with what reading it learned (`info`), or it failed.
    pub fn end(
        &mut self,
        file: i64,
        result: Result<serde_json::Value>,
        index: &mut Writer,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        // A worker hands over a file's start, pieces and end in that order.
        let assembly = self.assemblies.remove(&file).expect("started");
        self.completed += 1;
        if assembly.stale {
            return Ok(());
        }
        let path = assembly.row.path.display().to_string();
        match result {
            Ok(mut info) => {
                if assembly.failures > 0 {
                    info["unembedded"] = serde_json::json!(assembly.failures);
                }
                let parts = assembly.written > 0;
                // Not written when the file changed (or left) while it was read: it's queued again.
                if index.finish_file(&assembly.row, &assembly.segments, &info, parts)? {
                    self.indexed += 1;
                    on_event(Event::File {
                        path,
                        kind: assembly.row.kind,
                        segments: assembly.written + assembly.segments.len(),
                        vectors: assembly.vectors + assembly.segments.iter().filter(|s| s.vector.is_some()).count(),
                        ms: assembly.started.elapsed().as_millis() as u64,
                    });
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                if index.fail_file(&assembly.row, &error)? {
                    self.failed += 1;
                    on_event(Event::Failed { path, error });
                }
            }
        }
        Ok(())
    }
}
