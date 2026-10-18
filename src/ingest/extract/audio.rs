//! Recordings, through AVFoundation: their sound decoded to 16 kHz mono, and their tags (title, artist, album). At
//! `words` depth a recording is found by its name and tags only.

use crate::av;
use crate::config::Settings;
use crate::ingest::Content;
use anyhow::Result;
use serde_json::json;
use std::path::Path;

pub fn read(path: &Path, settings: &Settings, content: &mut Content) -> Result<serde_json::Value> {
    if !settings.embeds() {
        return Ok(json!({ "duration": av::duration(path), "tags": av::tags(path) }));
    }
    let seconds = av::read_samples(path, 0.0, &mut |samples| content.sound(samples))?;
    content.end_sound()?;
    Ok(json!({ "duration": seconds, "windows": content.windows, "tags": av::tags(path) }))
}
