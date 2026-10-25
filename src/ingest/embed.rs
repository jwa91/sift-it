//! Embedding: the model's only owner. It loads the text model when something first needs a vector, the vision encoder
//! when a picture or a video frame does, and the audio encoder when sound does (each memory of its own). An encoder
//! goes when its round leaves the plan, the text model when indexing has been idle a while. Texts are embedded in
//! batches, one model call for a batch of the text round.

use super::{Payload, Piece};
use crate::index::NewSegment;
use crate::models;
use anyhow::Result;
use embedder::{Embedder, Encoder, Inputs};
use protocol::Round;

#[derive(Default)]
pub struct Embed {
    embedder: Option<Embedder>,
}

impl Embed {
    /// Frees the model until it's needed again.
    pub fn release(&mut self) {
        self.embedder = None;
    }

    /// Frees the encoders until a picture or sound needs one again.
    pub fn release_encoders(&mut self) {
        if let Some(embedder) = &mut self.embedder {
            embedder.release_encoders();
        }
    }

    pub fn loaded(&self) -> bool {
        self.embedder.is_some()
    }

    /// Texts embedded together: their vectors, or None when the call failed. An error when the model can't load.
    pub fn texts(&mut self, texts: &[&str]) -> Result<Option<Vec<Vec<f32>>>> {
        if texts.is_empty() {
            return Ok(Some(Vec::new()));
        }
        Ok(self.model()?.embed_texts(texts).ok())
    }

    /// A piece as a segment: a picture or sound embedded now; text, or a picture the pictures round embeds, marked as
    /// waiting for its round; words alone without a vector. True with it when it couldn't be embedded.
    pub fn one(&mut self, piece: Piece) -> Result<(NewSegment, bool)> {
        let Piece { segment, loc, loc_end, text, payload } = piece;
        let mut new = NewSegment { segment, loc, loc_end, text, ..Default::default() };
        let vector = match payload {
            Payload::None => Ok(None),
            Payload::Text(context) => {
                new.context = (!context.is_empty()).then_some(context);
                new.due = Some(Round::Text);
                Ok(None)
            }
            Payload::Later => {
                new.due = Some(Round::Pictures);
                Ok(None)
            }
            Payload::Image(image) => self.encoder(Encoder::Vision)?.embed_image(&image).map(Some),
            Payload::Audio(samples) => self.encoder(Encoder::Audio)?.embed_audio(&samples).map(Some),
        };
        let failed = vector.is_err();
        new.vector = vector.ok().flatten();
        Ok((new, failed))
    }

    /// The text model, loaded now if it isn't.
    fn model(&mut self) -> Result<&mut Embedder> {
        if self.embedder.is_none() {
            self.embedder = Some(Embedder::load(&models::TEXT.require()?, Inputs::Documents)?);
        }
        Ok(self.embedder.as_mut().expect("just loaded"))
    }

    /// The text model with an encoder, loaded now if it isn't.
    fn encoder(&mut self, encoder: Encoder) -> Result<&mut Embedder> {
        let embedder = self.model()?;
        if !embedder.has_encoder(encoder) {
            embedder.load_encoder(encoder, &models::MEDIA.require()?)?;
        }
        Ok(embedder)
    }
}
