//! The index: what's known of every file, behind one facade.
//!
//! - The catalog (catalog.rs): one SQLite file. A row per file, a row per segment (a passage, a PDF page, a picture,
//!   a video frame, 30 s of sound, what's said in a recording) with where its vector is, a full-text index over every
//!   segment's words and its file's name and tags, and the queue: what's still to be read, or written down.
//! - The vector files (vectors.rs): every segment's vector, in flat files beside it, in three forms.
//! - The vectors as searches read them (mapped.rs): mapped, caught up with what's committed, and scanned.
//!
//! There's one Writer, the indexer's. It keeps the rule that a segment's vector is on disk before the segment that
//! points at it commits. Readers (the engine, status) read alongside it (WAL), and notice its commits through SQLite's
//! data_version. Transactions, generations and mapped files stay behind the facade, and so does every query.

mod catalog;
mod mapped;
mod vectors;

pub use catalog::now;
pub use mapped::Vectors;
pub use vectors::Form;

use protocol::{Kind, Segment};
use rusqlite::Connection;
use serde::Serialize;
use std::path::PathBuf;

/// Reads the index: what searches and status see of it. Never writes.
pub struct Reader {
    conn: Connection,
    path: PathBuf,
}

/// Writes the index: the indexer's, and there's one. It reads as a Reader does.
pub struct Writer {
    reader: Reader,
}

impl std::ops::Deref for Writer {
    type Target = Reader;

    fn deref(&self) -> &Reader {
        &self.reader
    }
}

/// A file to queue: where it is, what it is, and how it's to be read.
#[derive(Debug, Clone)]
pub struct NewFile {
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: f64,
    /// How it's read (ingest::fingerprint): when this changes, it's read again.
    pub reading: String,
    /// What's said in it is to be written down.
    pub speech: bool,
}

/// A queued or indexed file, as handed out to be read.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub modified: f64,
    /// How it's read (ingest::fingerprint).
    pub reading: String,
    /// What's said in it is to be written down.
    pub speech: bool,
}

/// A segment to write.
#[derive(Debug, Clone)]
pub struct NewSegment {
    pub segment: Segment,
    /// Page (from 1), line (from 1) or second, by segment.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    /// The words for the keyword index and excerpts.
    pub text: Option<String>,
    pub vector: Option<Vec<f32>>,
}

/// A segment as written, with its file: what a result is made of.
#[derive(Debug, Clone)]
pub struct SegmentRow {
    pub path: String,
    pub kind: Kind,
    pub modified: f64,
    pub size: u64,
    /// What reading the file found out: pages, duration, size in pixels, tags.
    pub info: serde_json::Value,
    pub segment: Segment,
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub text: Option<String>,
}

/// The work still queued (Reader::queued).
#[derive(Debug, Serialize)]
pub struct Queued {
    pub files: usize,
    /// About how long reading them takes (discover::cost).
    pub seconds: f64,
    /// The dearest of them, dearest first.
    pub heaviest: Vec<Heavy>,
}

#[derive(Debug, Serialize)]
pub struct Heavy {
    pub path: String,
    pub kind: Kind,
    pub seconds: f64,
}

/// Recordings and videos whose speech is written down (Reader::transcripts).
#[derive(Debug, Serialize)]
pub struct Transcripts {
    pub done: usize,
    pub total: usize,
    /// Sound still to be written down.
    pub seconds: f64,
}

/// What tests build indexes with.
#[cfg(test)]
pub mod testing {
    use super::{Kind, NewFile, NewSegment, Segment, Writer};
    use std::path::{Path, PathBuf};

    pub use super::vectors::unit;

    /// An index in a folder of its own, removed when the test ends.
    pub struct TempIndex {
        pub path: PathBuf,
    }

    impl TempIndex {
        pub fn new(name: &str) -> Self {
            let folder = std::env::temp_dir().join(format!("sift-it-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&folder);
            Self { path: folder.join("index.sqlite") }
        }
    }

    impl Drop for TempIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    /// A file of `kind` at `path`, to queue.
    pub fn file(path: &str, kind: Kind) -> NewFile {
        NewFile { path: path.into(), kind, size: 1, modified: 1.0, reading: "words".into(), speech: false }
    }

    /// A file of `kind` whose passages are these texts, with these vectors, written: its id.
    pub fn add(index: &mut Writer, path: &str, kind: Kind, passages: Vec<(String, Option<Vec<f32>>)>) -> i64 {
        index.queue(&file(path, kind), None, 0.0).unwrap();
        let row = index.pending().unwrap().into_iter().find(|row| row.path == Path::new(path)).unwrap();
        let segments: Vec<NewSegment> = passages
            .into_iter()
            .enumerate()
            .map(|(i, (text, vector))| NewSegment {
                segment: Segment::Passage,
                loc: Some(i as f64 + 1.0),
                loc_end: None,
                text: Some(text),
                vector,
            })
            .collect();
        assert!(index.finish_file(&row, &segments, &serde_json::json!({}), false).unwrap());
        row.id
    }
}
