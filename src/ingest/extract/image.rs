//! Pictures and screenshots, through ImageIO: the picture decoded for what it shows, and on a screenshot the text
//! Vision reads on it. Their words are only their names: a photo isn't opened for them.

use crate::adapters::macos;
use crate::config::Settings;
use crate::ingest::read::{Content, Ended};
use anyhow::Result;
use protocol::Kind;
use serde_json::json;
use std::path::Path;

/// Pictures are decoded at most this large: plenty for any token budget, and a 48-megapixel photo decodes quickly.
const DECODE_SIDE: u32 = 2048;

/// The words round: the picture, to be embedded by the pictures round.
pub fn words(content: &mut Content) -> Result<Ended> {
    content.picture_later()?;
    Ok(Ended::whole(json!({}), 1.0))
}

/// The OCR round: the text Vision reads on a screenshot.
pub fn ocr(path: &Path, content: &mut Content) -> Result<Ended> {
    let picture = macos::open_picture(path, DECODE_SIDE)?;
    let text = macos::recognize_text(&picture.image)?;
    let long = content.read_off(&text)?;
    let info = json!({ "width": picture.width, "height": picture.height, "taken": picture.taken, "text": long });
    Ok(Ended::whole(info, 1.0))
}

/// The pictures round: what it shows. A screenshot is dense with small things: it takes the budget for text.
pub fn picture(path: &Path, kind: Kind, settings: &Settings, content: &mut Content) -> Result<Ended> {
    let picture = macos::open_picture(path, DECODE_SIDE)?;
    let budget = if kind == Kind::Screenshot { settings.text_tokens } else { settings.tokens };
    content.shown(None, &picture.image, budget)?;
    Ok(Ended::whole(json!({ "width": picture.width, "height": picture.height, "taken": picture.taken }), 1.0))
}
