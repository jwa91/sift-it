//! Videos, through AVFoundation: frames sampled every few seconds, each with a thumbnail to tell shots apart, and
//! their sound as a recording's. At `words` depth a video is found by its name and tags only.

use crate::av;
use crate::config::Settings;
use crate::ingest::Content;
use crate::mac;
use anyhow::Result;
use serde_json::json;
use std::path::Path;

/// Frames are taken at most this large (enough for 70 tokens, and quick to decode).
const FRAME_SIDE: u32 = 640;

pub fn read(path: &Path, settings: &Settings, content: &mut Content) -> Result<serde_json::Value> {
    if !settings.embeds() {
        return Ok(json!({ "duration": av::duration(path), "tags": av::tags(path) }));
    }
    let video = av::read_video(path, FRAME_SIDE, &mut |at, frame| content.frame(at, frame, &mac::signature(frame)?))?;
    if settings.sound && av::has_audio(path) {
        av::read_samples(path, 0.0, &mut |samples| content.sound(samples))?;
        content.end_sound()?;
    }
    Ok(json!({
        "duration": video.duration,
        "width": video.width,
        "height": video.height,
        "frames": content.frames,
        "windows": content.windows,
        "tags": av::tags(path),
    }))
}
