//! Readers turn a file into pieces: what the model should see (text with its document prompt, a picture sized for
//! its token budget, 30 s of 16 kHz sound) and what the keyword index should hold. They stream their pieces to a
//! sink, so a long video never sits in memory whole.

pub mod code;
pub mod image;
pub mod media;
pub mod pdf;
pub mod text;

use crate::config::{Depth, Settings};
use crate::crawl::{Kind, is_pdf};
use crate::imaging::Image;
use crate::store::Segment;
use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

pub enum Payload {
    /// Keyword index only.
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

/// Goes up when a kind's reader changes in a way that matters for what's indexed, or learns to read files it
/// couldn't: the files it read (or failed to) are read again, and the index keeps what it has meanwhile.
pub fn version(kind: Kind) -> u32 {
    match kind {
        Kind::Plaintext | Kind::Document | Kind::Image | Kind::Screenshot | Kind::Audio | Kind::Video | Kind::Code => 1,
    }
}

/// What decides how a file is read: its reader's version and the reading values it uses at its depth. When it
/// changes, the file is read again. Whether what's said in it is written down isn't in it: that comes after reading.
pub fn fingerprint(path: &Path, kind: Kind, settings: &Settings) -> String {
    let words = match kind {
        Kind::Plaintext | Kind::Code | Kind::Document | Kind::Screenshot => {
            json!([settings.passage_chars, settings.max_passage_chars])
        }
        Kind::Image | Kind::Audio | Kind::Video => json!(null),
    };
    let meaning = settings.embeds().then(|| match kind {
        Kind::Plaintext | Kind::Code | Kind::Audio => json!(null),
        Kind::Document if is_pdf(path) => json!(settings.text_tokens),
        Kind::Document => json!(null),
        Kind::Image => json!(settings.tokens),
        Kind::Screenshot => json!(settings.text_tokens),
        Kind::Video => json!([settings.tokens, settings.shot_change, settings.keep_every_s, settings.sound]),
    });
    let digest = Sha256::digest(format!("{}/{}/{}", kind.as_str(), version(kind), json!([words, meaning])));
    digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What's said in a file of this kind is written down.
pub fn transcribes(kind: Kind, settings: &Settings) -> bool {
    settings.depth == Depth::Speech && (kind == Kind::Audio || (kind == Kind::Video && settings.sound))
}

/// The model's input for a text, at a depth that embeds; else the text goes into the keyword index only. Text that's
/// mostly numbers is never embedded: a vector of it means nothing.
pub fn text_payload(title: &str, text: &str, settings: &Settings) -> Payload {
    if settings.embeds() && !text::mostly_numbers(text) {
        Payload::Text(crate::embedder::prompts::document(Some(title), text))
    } else {
        Payload::None
    }
}

/// Reads a file into pieces; returns what was learned about it (pages, duration, size) for the index.
pub fn read(path: &Path, kind: Kind, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    match kind {
        Kind::Plaintext => text::read_plain(path, settings, sink),
        Kind::Code => code::read(path, settings, sink),
        Kind::Document if is_pdf(path) => pdf::read(path, settings, sink),
        Kind::Document => text::read_document(path, settings, sink),
        Kind::Image | Kind::Screenshot => image::read(path, kind, settings, sink),
        Kind::Audio => media::read_audio(path, settings, sink),
        Kind::Video => media::read_video(path, settings, sink),
    }
}

/// The title a document's passages are embedded with: its file name, without the extension.
pub fn title_of(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().replace(['_', '-'], " ")).unwrap_or_default()
}
