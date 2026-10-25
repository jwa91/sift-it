//! The index: what's known of every file, behind one facade.
//!
//! - The catalog (catalog.rs): one SQLite file. A row per file, a row per segment (a passage, a PDF page, a picture,
//!   a video frame, 30 s of sound, what's said in a recording) with where its vector is or which round it waits for, a
//!   full-text index over every segment's words and its file's name and tags, and the queues: a row per file and
//!   round of indexing it needs (ingest/rounds.rs), with how far it is.
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

use protocol::{Kind, Round, Segment};
use rusqlite::Connection;
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
    /// It just appeared or changed (FSEvents said): it goes first in every round it needs.
    pub new: bool,
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
#[derive(Debug, Clone, Default)]
pub struct NewSegment {
    pub segment: Segment,
    /// Page (from 1), line (from 1) or second, by segment.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    /// The words for the keyword index and excerpts.
    pub text: Option<String>,
    /// What a passage is embedded with besides its text: the heading, signature or header row it was cut from.
    pub context: Option<String>,
    /// The round its vector comes from, when it waits for one: text, or pictures.
    pub due: Option<Round>,
    pub vector: Option<Vec<f32>>,
}

/// A round's work for a file, as handed out: how much there is, and how much is done (in the round's unit).
#[derive(Debug, Clone)]
pub struct Work {
    pub file: FileRow,
    pub round: Round,
    pub amount: f64,
    pub done: f64,
    /// Where the next item goes on, for a round that goes in items (a video's last frame kept).
    pub resume: Option<serde_json::Value>,
    /// A new file's: it goes first.
    pub new: bool,
}

/// How an item of a round ended (Writer::write_item): how much of the file's work is done now, where the next item
/// goes on, whether that's all of it, and what was learned about the file.
#[derive(Debug, Clone, Default)]
pub struct Done {
    pub done: f64,
    pub resume: Option<serde_json::Value>,
    pub finished: bool,
    pub info: serde_json::Value,
}

/// A passage waiting for its vector: the text round's input.
#[derive(Debug, Clone)]
pub struct DueText {
    pub id: i64,
    pub file: FileRow,
    pub segment: Segment,
    pub text: String,
    pub context: Option<String>,
    /// The title tag of a recording or video, which what's said in it is embedded with.
    pub title: Option<String>,
}

/// How far a round is (Reader::rounds), in its unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundTotals {
    pub round: Round,
    pub done: f64,
    pub amount: f64,
    /// Files with something left in it.
    pub files: usize,
    /// Of those, new files.
    pub new: usize,
    pub failed: usize,
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

/// A file with much left to do in a round: a long recording, a long video.
#[derive(Debug, serde::Serialize)]
pub struct Heavy {
    pub path: String,
    pub round: Round,
    /// What's left, in the round's unit.
    pub left: f64,
}

/// What tests build indexes with.
#[cfg(test)]
pub mod testing {
    use super::{Kind, NewFile, NewSegment, Round, Segment, Writer};
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
        NewFile { path: path.into(), kind, size: 1, modified: 1.0, reading: "words".into(), speech: false, new: false }
    }

    /// A file of `kind` whose passages are these texts, with these vectors, written: its id.
    pub fn add(index: &mut Writer, path: &str, kind: Kind, passages: Vec<(String, Option<Vec<f32>>)>) -> i64 {
        index.queue(&file(path, kind), None, 0.0).unwrap();
        let words = index.next(Round::Words, false, 100).unwrap().into_iter().find(|w| w.file.path == Path::new(path));
        let segments: Vec<NewSegment> = passages
            .into_iter()
            .enumerate()
            .map(|(i, (text, vector))| NewSegment {
                segment: Segment::Passage,
                loc: Some(i as f64 + 1.0),
                text: Some(text),
                vector,
                ..Default::default()
            })
            .collect();
        let words = words.unwrap();
        assert!(index.write_words(&words, &segments, &serde_json::json!({}), &[], false, None).unwrap());
        words.file.id
    }
}
