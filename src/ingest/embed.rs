//! Embedding: the model's only owner. It loads the text model when something first needs a vector, and its image and
//! audio encoders (the larger half, and memory of their own) when a picture or sound does. The encoders go when the
//! reading is over, the text model when indexing has been idle a while. Texts are embedded in batches, one model call
//! for up to BATCH of them.

use super::{Payload, Piece};
use crate::index::NewSegment;
use crate::models;
use anyhow::Result;
use embedder::{Embedder, Inputs};

/// Texts embedded in one model call, at most.
const BATCH: usize = 16;

/// A piece embedded, for its file: as a segment (without a vector when it couldn't have one), and whether embedding
/// it failed (it stays in the keyword index).
pub type Embedded = (i64, NewSegment, bool);

#[derive(Default)]
pub struct Embed {
    embedder: Option<Embedder>,
    /// Texts waiting for a model call, with their file.
    batch: Vec<(i64, Piece)>,
}

impl Embed {
    /// Frees the model until it's needed again.
    pub fn release(&mut self) {
        self.embedder = None;
    }

    /// Frees the image and audio encoders until a picture or sound needs them again.
    pub fn release_encoders(&mut self) {
        if let Some(embedder) = &mut self.embedder {
            embedder.release_encoders();
        }
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
        let vectors = self.model()?.embed_texts(&texts);
        let failed = vectors.is_err();
        let mut vectors = vectors.unwrap_or_default().into_iter();
        Ok(pieces.into_iter().map(|(file, piece)| (file, segment(piece, vectors.next()), failed)).collect())
    }

    /// A picture or sound embedded now, or a piece without a vector (words alone). True with it when it couldn't be
    /// embedded.
    pub fn one(&mut self, piece: Piece) -> Result<(NewSegment, bool)> {
        let vector = match &piece.payload {
            Payload::None | Payload::Text(_) => Ok(None),
            Payload::Image(image) => self.encoders()?.embed_image(image).map(Some),
            Payload::Audio(samples) => self.encoders()?.embed_audio(samples).map(Some),
        };
        let failed = vector.is_err();
        Ok((segment(piece, vector.ok().flatten()), failed))
    }

    /// Texts embedded together: their vectors, or none when the call failed.
    pub fn texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.model()?.embed_texts(texts).unwrap_or_default())
    }

    /// The text model, loaded now if it isn't.
    fn model(&mut self) -> Result<&mut Embedder> {
        if self.embedder.is_none() {
            self.embedder = Some(Embedder::load(&models::TEXT.require()?, Inputs::Documents)?);
        }
        Ok(self.embedder.as_mut().expect("just loaded"))
    }

    /// The text model with its image and audio encoders, loaded now if they aren't.
    fn encoders(&mut self) -> Result<&mut Embedder> {
        let embedder = self.model()?;
        if !embedder.has_encoders() {
            embedder.load_encoders(&models::MEDIA.require()?)?;
        }
        Ok(embedder)
    }
}

fn segment(piece: Piece, vector: Option<Vec<f32>>) -> NewSegment {
    NewSegment { segment: piece.segment, loc: piece.loc, loc_end: piece.loc_end, text: piece.text, vector }
}
