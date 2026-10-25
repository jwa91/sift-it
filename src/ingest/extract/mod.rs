//! Extracting: a file decoded into content, per container format and round: lines of text, a page's text, the text on
//! pictures, pictures and video frames, sound. What it decodes goes to Content by its type; nothing here decides how
//! it's cut or what the model is shown of it. Speech (extract/speech.rs) is an extractor too, with a lane of its own.

mod audio;
mod document;
mod image;
mod notebook;
pub(super) mod pdf;
pub mod speech;
pub mod text;
mod video;

use super::read::{Content, Ended, From};
use crate::config::Settings;
use crate::config::{extension_of, is_pdf};
use anyhow::{Result, bail};
use protocol::{Kind, Round};
use std::path::Path;

/// Reads an item of a file's work in `round` into `content`.
pub fn read(
    round: Round,
    path: &Path,
    kind: Kind,
    settings: &Settings,
    from: &From,
    content: &mut Content,
) -> Result<Ended> {
    match (round, kind) {
        (Round::Words, Kind::Code) if extension_of(path) == "ipynb" => notebook::read(path, content),
        (Round::Words, Kind::Plaintext | Kind::Structured | Kind::Code) => text::read(path, content),
        (Round::Words, Kind::Document) if is_pdf(path) => pdf::words(path, content),
        (Round::Words, Kind::Document) => document::read(path, content),
        (Round::Words, Kind::Image | Kind::Screenshot) => image::words(content),
        (Round::Words, Kind::Audio | Kind::Video) => audio::words(path),
        (Round::Ocr, Kind::Screenshot) => image::ocr(path, content),
        (Round::Ocr, Kind::Document) => pdf::ocr(path, from, content),
        (Round::Pictures, Kind::Image | Kind::Screenshot) => image::picture(path, kind, settings, content),
        (Round::Pictures, Kind::Document) => pdf::pictures(path, settings, from, content),
        (Round::Pictures, Kind::Video) => video::frames(path, from, content),
        (Round::Sound, Kind::Audio | Kind::Video) => audio::sound(path, from, content),
        _ => bail!("the {} round doesn't read {} files", round.as_str(), kind.as_str()),
    }
}
