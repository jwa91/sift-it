//! The ingest path: keeping the index in step with the chosen folders. Its stages, each owning what it works on:
//!
//! - discover (discover/): which files are new or changed, queued with what reading each costs, and which are gone.
//! - read (read.rs): a file extracted (extract/), cut into segments (segment/) and prepared for the model (prepare.rs),
//!   as a stream of pieces.
//! - embed (embed.rs): the model's only owner. It batches texts and hands back vectors.
//! - write (write.rs): files put together from their segments and written, whole or in parts. The index's writer
//!   (index::Writer) is the Indexer's, and only it writes.
//! - schedule (schedule.rs): what's read first, how many files at once (resources::IngestPlan), and the run that joins
//!   the stages.
//! - listen (listen.rs): what's said in recordings and videos, written down on a lane of its own, beside the reading
//!   when memory allows, else after it. Speech is an extractor too (extract/speech.rs): sound in, words with their
//!   times out, cut into passages and embedded like any text.
//!
//! The Indexer holds the index's writer and the model, for the daemon and `sift-it index`.

pub mod discover;
mod embed;
mod extract;
mod listen;
mod prepare;
mod read;
mod schedule;
mod segment;
mod write;

pub use read::{Content, Payload, Piece, fingerprint, read, transcribes};
pub use schedule::Stuck;

use crate::config::Config;
use crate::index::Writer;
use anyhow::Result;
use embed::Embed;
use protocol::Kind;
use serde::Serialize;
use std::path::PathBuf;

/// What indexing does, as it does it: for the daemon's status and log, and `sift-it index`.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Sync {
        files: usize,
        added: usize,
        changed: usize,
        removed: usize,
        unavailable: Vec<String>,
        pending: usize,
        ms: u64,
    },
    Started {
        pending: usize,
    },
    /// Files that appeared while indexing joined the queue.
    Queued {
        count: usize,
    },
    File {
        path: String,
        kind: Kind,
        segments: usize,
        vectors: usize,
        ms: u64,
    },
    Failed {
        path: String,
        error: String,
    },
    Finished {
        indexed: usize,
        failed: usize,
        seconds: f64,
        stopped: bool,
    },
    /// A stretch of a recording or video was written down: what's said up to `at` seconds of `duration`.
    Transcribing {
        path: String,
        at: f64,
        duration: f64,
    },
    /// A recording or video was written down to its end.
    Transcribed {
        path: String,
        passages: usize,
        seconds: f64,
        ms: u64,
    },
    TranscribeFailed {
        path: String,
        error: String,
    },
    /// Writing down speech beside the reading stopped: the speech helper couldn't start, or it stopped. It goes on
    /// once everything is read.
    SpeechStopped {
        error: String,
    },
}

pub struct Indexer {
    pub index: Writer,
    embed: Embed,
}

impl Indexer {
    /// An indexer writing to `index`. Vectors from another source (another model, or a pipeline that makes other
    /// vectors) are dropped, and their files queued again: they'd never compare right with new queries. Vector files no partition uses (what a
    /// crash left, or what was dropped) go.
    pub fn new(mut index: Writer) -> Result<Self> {
        if index.adopt_vector_source(embedder::VECTOR_SOURCE)? {
            eprintln!("the index was made by another model or pipeline: everything is indexed again");
        }
        if index.adopt_speech_source(crate::adapters::speech::SOURCE)? {
            eprintln!("what's said in recordings was written down by another speech model: it's written down again");
        }
        index.tidy_vectors()?;
        Ok(Self { index, embed: Embed::default() })
    }

    /// Frees the model until it's needed again.
    pub fn release_model(&mut self) {
        self.embed.release();
    }

    pub fn model_loaded(&self) -> bool {
        self.embed.loaded()
    }

    /// Crawls the chosen folders and brings the queue in step with them (discover/).
    pub fn sync(&mut self, config: &Config) -> Result<Event> {
        discover::sync(&self.index, config)
    }

    /// A sync of just these paths (files, or folders with everything in them).
    pub fn sync_paths(&mut self, config: &Config, paths: &[PathBuf]) -> Result<Event> {
        discover::sync_paths(&self.index, config, paths)
    }
}
