//! Readers turn a file into pieces: what the model should see (text with its document prompt, a picture sized for
//! its token budget, 30 s of 16 kHz sound) and what the keyword index should hold. They stream their pieces to a
//! sink, so a long video never sits in memory whole.

pub mod code;
pub mod image;
pub mod media;
pub mod pdf;
pub mod text;

use crate::crawl::Kind;
use crate::imaging::Image;
use crate::store::Segment;
use anyhow::Result;
use std::path::Path;

pub enum Payload {
    /// Keyword index only (text that's mostly numbers: a vector of it means nothing).
    None,
    /// Text, already framed with its document prompt.
    Text(String),
    Image(Image),
    /// 16 kHz mono samples, at most 30 s.
    Audio(Vec<f32>),
}

pub struct Piece {
    pub segment: Segment,
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub text: Option<String>,
    pub payload: Payload,
}

pub type Sink<'a> = &'a mut dyn FnMut(Piece) -> Result<()>;

/// Goes up when the readers learn to read files they couldn't before: files that failed get another try.
/// 2: recordings AVFoundation gave up on partway are read past the spot.
pub const VERSION: u32 = 2;

/// Reads a file into pieces; returns what was learned about it (pages, duration, size) for the index.
pub fn read(path: &Path, kind: Kind, sink: Sink) -> Result<serde_json::Value> {
    match kind {
        Kind::Text => text::read_plain(path, sink),
        Kind::Code => code::read(path, sink),
        Kind::Doc => text::read_document(path, sink),
        Kind::Pdf => pdf::read(path, sink),
        Kind::Image | Kind::Screenshot => image::read(path, kind, sink),
        Kind::Audio => media::read_audio(path, sink),
        Kind::Video => media::read_video(path, sink),
    }
}

/// The title a document's passages are embedded with: its file name, without the extension.
pub fn title_of(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().replace(['_', '-'], " ")).unwrap_or_default()
}
