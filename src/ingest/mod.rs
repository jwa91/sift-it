//! The ingest path: keeping the index in step with the chosen folders, in rounds over the whole collection, each
//! with one tool (rounds.rs; docs/design.md, "Rounds"). Its stages, each owning what it works on:
//!
//! - discover (discover/): which files are new or changed, queued for their words with what reading them costs, and
//!   which are gone.
//! - read (read.rs): an item of a round, extracted (extract/), cut into segments (segment/) and prepared for the model
//!   (prepare.rs), as a stream of pieces.
//! - embed (embed.rs): the model's only owner. It embeds the text round's batches, and pictures and sound as they come.
//! - write (write.rs): what an item found, put together and written. The index's writer (index::Writer) is the
//!   Indexer's, and only it writes.
//! - schedule (schedule.rs): the run. The plan says which rounds run side by side (resources::Plan); it hands their
//!   items to the lanes, new files first, and keeps how far each round is (progress.rs).
//! - listen (listen.rs): the speech lane, which writes down what's said in recordings and videos through the speech
//!   helper. Speech is an extractor too (extract/speech.rs): sound in, words with their times out, cut into passages
//!   for the text round like any text.
//!
//! The Indexer holds the index's writer and the model, for the daemon and `sift-it index`.

pub mod discover;
mod embed;
mod extract;
mod listen;
mod prepare;
pub mod progress;
pub mod read;
pub mod rounds;
mod schedule;
mod segment;
mod write;

pub use extract::conversation;
pub use read::{Payload, Piece, fingerprint};
pub use rounds::transcribes;
pub use schedule::{Facts, Outcome, Stuck};

use crate::config::Config;
use crate::index::Writer;
use anyhow::Result;
use embed::Embed;
use protocol::{Kind, Round, RoundStatus};
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
        ms: u64,
    },
    /// What runs side by side changed (resources::Plan), and what some of the rest waits for besides its turn.
    Plan {
        rounds: Vec<Round>,
        readers: usize,
        waiting: Option<crate::resources::Wait>,
    },
    /// A round is done with a file: what its last item wrote.
    File {
        round: Round,
        path: String,
        kind: Kind,
        segments: usize,
        vectors: usize,
        ms: u64,
    },
    Failed {
        round: Round,
        path: String,
        error: String,
    },
    /// A stretch of a recording or video was written down: what's said up to `at` seconds of `duration`.
    Transcribing {
        path: String,
        at: f64,
        duration: f64,
    },
    /// How far each round is (at most once a second while it runs).
    Rounds {
        rounds: Vec<RoundStatus>,
    },
    Finished {
        files: usize,
        failed: usize,
        seconds: f64,
        stopped: bool,
    },
    /// Writing down speech stopped: the speech helper couldn't start, or it stopped. It's tried again later.
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
            eprintln!("what's said in audio was written down by another speech model: it's written down again");
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

    /// A sync of just these paths (files, or folders with everything in them): what FSEvents said changed, so what
    /// it queues goes first in every round it needs.
    pub fn sync_paths(&mut self, config: &Config, paths: &[PathBuf]) -> Result<Event> {
        discover::sync_paths(&self.index, config, paths)
    }
}
