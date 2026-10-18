//! Extracting: a file decoded into content, per container format: lines of text, a page's text, pictures with the
//! text on them, video frames, sound. What it decodes goes to Content by its type; nothing here decides how it's cut
//! or what the model is shown of it. Speech (extract/speech.rs) is an extractor too, a slow one, run on a lane of its own.

mod audio;
mod document;
mod image;
mod notebook;
mod pdf;
pub mod speech;
pub mod text;
mod video;

use super::Content;
use crate::config::Settings;
use crate::ingest::discover::crawl::is_pdf;
use anyhow::Result;
use protocol::Kind;
use std::path::Path;

/// Reads a file into `content`; returns what was learned about it (pages, duration, size) for the index.
pub fn read(path: &Path, kind: Kind, settings: &Settings, content: &mut Content) -> Result<serde_json::Value> {
    match kind {
        Kind::Code if path.extension().is_some_and(|e| e == "ipynb") => notebook::read(path, content),
        Kind::Plaintext | Kind::Structured | Kind::Code => text::read(path, content),
        Kind::Document if is_pdf(path) => pdf::read(path, content),
        Kind::Document => document::read(path, content),
        Kind::Image | Kind::Screenshot => image::read(path, kind, settings, content),
        Kind::Audio => audio::read(path, settings, content),
        Kind::Video => video::read(path, settings, content),
    }
}
