//! Pictures and screenshots, through ImageIO: the picture decoded, and on a screenshot the text Vision reads on it. At
//! `words` depth a photo isn't opened at all.

use crate::adapters::macos;
use crate::config::Settings;
use crate::ingest::Content;
use anyhow::Result;
use protocol::Kind;
use std::path::Path;

/// Pictures are decoded at most this large: plenty for any token budget, and a 48-megapixel photo decodes quickly.
pub const DECODE_SIDE: u32 = 2048;

pub fn read(path: &Path, kind: Kind, settings: &Settings, content: &mut Content) -> Result<serde_json::Value> {
    if kind == Kind::Image && !settings.embeds() {
        return Ok(serde_json::json!({}));
    }
    let picture = macos::open_picture(path, DECODE_SIDE)?;
    let text = if kind == Kind::Screenshot { Some(macos::recognize_text(&picture.image)?) } else { None };
    let long_text = content.picture(&picture.image, text.as_deref())?;
    Ok(serde_json::json!({
        "width": picture.width,
        "height": picture.height,
        "taken": picture.taken,
        "text": long_text,
    }))
}
