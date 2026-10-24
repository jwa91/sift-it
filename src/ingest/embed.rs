//! Embedding: the model's only owner. It loads the model when something first needs a vector: with its image and audio
//! encoders for the reading, or the text model alone (a third of the memory, quick to load) when only text does.
//! Texts are embedded in batches, one model call for up to BATCH of them.

use super::{Payload, Piece};
use crate::index::NewSegment;
use anyhow::Result;
use embedder::{Embedder, ModelFiles};
use std::path::PathBuf;

/// Texts embedded in one model call, at most.
const BATCH: usize = 16;

/// A piece embedded, for its file: as a segment (without a vector when it couldn't have one), and whether embedding
/// it failed (it stays in the keyword index).
pub type Embedded = (i64, NewSegment, bool);

pub struct Embed {
    models: PathBuf,
    embedder: Option<Embedder>,
    /// The loaded model has the image and audio encoders (else it's the text model alone).
    media: bool,
    /// Texts waiting for a model call, with their file.
    batch: Vec<(i64, Piece)>,
}

impl Embed {
    /// Embedding with the model in `models`, loaded on first need.
    pub fn new(models: PathBuf) -> Self {
        Self { models, embedder: None, media: false, batch: Vec::new() }
    }

    /// Frees the model (about 1 GB) until it's needed again.
    pub fn release(&mut self) {
        self.embedder = None;
    }

    pub fn loaded(&self) -> bool {
        self.embedder.is_some()
    }

    /// A text to embed with the next batch.
    pub fn queue(&mut self, file: i64, piece: Piece) {
        self.batch.push((file, piece));
    }

    /// Drops the texts waiting: the run they're from is over, and a file not finished is read again from the start.
    pub fn forget(&mut self) {
        self.batch.clear();
    }

    /// The batch is as large as one model call takes.
    pub fn full(&self) -> bool {
        self.batch.len() >= BATCH
    }

    /// Embeds the batched texts of the files `keep` takes, in one model call. When it fails they're segments
    /// without vectors.
    pub fn flush(&mut self, keep: impl Fn(i64) -> bool) -> Result<Vec<Embedded>> {
        self.batch.retain(|(file, _)| keep(*file));
        if self.batch.is_empty() {
            return Ok(Vec::new());
        }
        let pieces = std::mem::take(&mut self.batch);
        let texts: Vec<&str> = pieces
            .iter()
            .map(|(_, piece)| match &piece.payload {
                Payload::Text(text) => text.as_str(),
                _ => "",
            })
            .collect();
        // Experiment: the texts the indexer embeds, one JSON string a line, to time encoders on (bench-texts).
        if let Some(path) = std::env::var_os("SIFT_IT_DUMP_TEXTS") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            for text in &texts {
                writeln!(file, "{}", serde_json::to_string(text)?)?;
            }
        }
        let vectors = self.model(true)?.embed_texts(&texts);
        let failed = vectors.is_err();
        let mut vectors = vectors.unwrap_or_default().into_iter();
        Ok(pieces.into_iter().map(|(file, piece)| (file, segment(piece, vectors.next()), failed)).collect())
    }

    /// A picture or sound embedded now, or a piece without a vector (words alone). True with it when it couldn't be
    /// embedded.
    pub fn one(&mut self, piece: Piece) -> Result<(NewSegment, bool)> {
        let vector = match &piece.payload {
            Payload::None | Payload::Text(_) => Ok(None),
            Payload::Image(image) => self.model(true)?.embed_image(image).map(Some),
            Payload::Audio(samples) => self.model(true)?.embed_audio(samples).map(Some),
        };
        let failed = vector.is_err();
        Ok((segment(piece, vector.ok().flatten()), failed))
    }

    /// Texts embedded together, with whichever model is loaded (else the text model alone): their vectors, or none
    /// when the call failed.
    pub fn texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.model(false)?.embed_texts(texts).unwrap_or_default())
    }

    /// Loads the model with its encoders now, if it isn't: the reading needs it, and it isn't loaded twice.
    pub fn load_media(&mut self) -> Result<()> {
        self.model(true).map(drop)
    }

    /// The loaded model, or one loaded now: with its image and audio encoders when `media` needs them, else the text
    /// model alone. A text model makes way for one with the encoders, never the other way.
    fn model(&mut self, media: bool) -> Result<&mut Embedder> {
        if self.embedder.is_none() || (media && !self.media) {
            self.embedder = None;
            self.embedder = Some(Embedder::load(&ModelFiles::in_folder(&self.models)?, !media)?);
            self.media = media;
        }
        Ok(self.embedder.as_mut().expect("just loaded"))
    }
}

fn segment(piece: Piece, vector: Option<Vec<f32>>) -> NewSegment {
    NewSegment { segment: piece.segment, loc: piece.loc, loc_end: piece.loc_end, text: piece.text, vector }
}
