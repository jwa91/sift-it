//! Pictures and screenshots. A picture is one vector of what it shows; a screenshot also has the text on it read
//! (Vision), which becomes passages of its own and goes into the keyword index.

use super::text::{mostly_numbers, passages};
use super::{Payload, Piece, Sink, title_of};
use crate::crawl::Kind;
use crate::embedder::prompts;
use crate::imaging;
use crate::mac;
use crate::store::Segment;
use anyhow::Result;
use std::path::Path;

/// Pictures are decoded at most this large: plenty for any token budget, and a 48-megapixel photo decodes quickly.
pub const DECODE_SIDE: u32 = 2048;
/// Token budgets: a photo reads well at 140; a screenshot is dense with small things and gets 280.
pub const PHOTO_BUDGET: u32 = 140;
pub const SCREENSHOT_BUDGET: u32 = 280;
/// Text read from a picture becomes passages of its own from this many characters on.
pub const MIN_TEXT: usize = 40;

pub fn read(path: &Path, kind: Kind, sink: Sink) -> Result<serde_json::Value> {
    let picture = mac::open_picture(path, DECODE_SIDE)?;
    let pixels = mac::pixels(&picture.image)?;
    let budget = if kind == Kind::Screenshot { SCREENSHOT_BUDGET } else { PHOTO_BUDGET };
    let fitted = imaging::fit(&pixels, budget)?;
    let text = if kind == Kind::Screenshot { mac::recognize_text(&picture.image)? } else { String::new() };
    let long_text = text.chars().count() >= MIN_TEXT;
    sink(Piece {
        segment: Segment::Picture,
        loc: None,
        loc_end: None,
        // Short text (a title, a button) only goes into the keyword index, with the picture.
        text: (!long_text && !text.trim().is_empty()).then(|| text.clone()),
        payload: Payload::Image(fitted),
    })?;
    if long_text {
        emit_text(path, &text, None, sink)?;
    }
    Ok(serde_json::json!({
        "width": picture.width,
        "height": picture.height,
        "taken": picture.taken,
        "text": long_text,
    }))
}

/// Text read from a picture or a scanned page, as passages with vectors.
pub fn emit_text(path: &Path, text: &str, loc: Option<f64>, sink: Sink) -> Result<()> {
    let title = title_of(path);
    for passage in passages(text) {
        let payload = if mostly_numbers(&passage.text) {
            Payload::None
        } else {
            Payload::Text(prompts::document(Some(&title), &passage.text))
        };
        sink(Piece { segment: Segment::Ocr, loc, loc_end: None, text: Some(passage.text), payload })?;
    }
    Ok(())
}
