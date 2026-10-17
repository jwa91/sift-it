//! Pictures and screenshots. A picture is one vector of what it shows, seen with `tokens` (a screenshot, dense with
//! small things, with `text_tokens`); a screenshot also has the text on it read (Vision), which becomes passages of
//! its own and goes into the keyword index. At `words` depth only the text is read: a photo isn't opened at all.

use super::text::passages;
use super::{Payload, Piece, Sink, text_payload, title_of};
use crate::config::Settings;
use crate::crawl::Kind;
use crate::imaging;
use crate::mac;
use crate::store::Segment;
use anyhow::Result;
use std::path::Path;

/// Pictures are decoded at most this large: plenty for any token budget, and a 48-megapixel photo decodes quickly.
pub const DECODE_SIDE: u32 = 2048;
/// Text read from a picture becomes passages of its own from this many characters on.
pub const MIN_TEXT: usize = 40;

pub fn read(path: &Path, kind: Kind, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    if kind == Kind::Image && !settings.embeds() {
        return Ok(serde_json::json!({}));
    }
    let picture = mac::open_picture(path, DECODE_SIDE)?;
    let text = if kind == Kind::Screenshot { mac::recognize_text(&picture.image)? } else { String::new() };
    let long_text = text.chars().count() >= MIN_TEXT;
    let info = serde_json::json!({
        "width": picture.width,
        "height": picture.height,
        "taken": picture.taken,
        "text": long_text,
    });
    if !settings.embeds() {
        emit_text(path, &text, None, settings, sink)?;
        return Ok(info);
    }
    let budget = if kind == Kind::Screenshot { settings.text_tokens } else { settings.tokens };
    let fitted = imaging::fit(&mac::pixels(&picture.image)?, budget)?;
    sink(Piece {
        segment: Segment::Picture,
        loc: None,
        loc_end: None,
        // Short text (a title, a button) only goes into the keyword index, with the picture.
        text: (!long_text && !text.trim().is_empty()).then(|| text.clone()),
        payload: Payload::Image(fitted),
    })?;
    if long_text {
        emit_text(path, &text, None, settings, sink)?;
    }
    Ok(info)
}

/// Text read from a picture or a scanned page, as passages (embedded, at a depth that embeds).
pub fn emit_text(path: &Path, text: &str, loc: Option<f64>, settings: &Settings, sink: Sink) -> Result<()> {
    let title = title_of(path);
    for passage in passages(text, settings) {
        let payload = text_payload(&title, &passage.text, settings);
        sink(Piece { segment: Segment::Ocr, loc, loc_end: None, text: Some(passage.text), payload })?;
    }
    Ok(())
}
