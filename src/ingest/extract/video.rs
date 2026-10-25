//! Videos, through AVFoundation. Their words are their names and tags. For the pictures round, frames sampled every
//! few seconds, each with a thumbnail to tell shots apart, five minutes of the video an item; their sound is read as a
//! recording's (audio.rs).

use crate::adapters::macos;
use crate::adapters::macos::av;
use crate::ingest::read::{Content, Ended, From};
use crate::ingest::rounds::{FRAME_STEP, SLICE};
use anyhow::Result;
use serde_json::json;
use std::path::Path;

/// Frames are taken at most this large (enough for 70 tokens, and quick to decode).
const FRAME_SIDE: u32 = 640;

/// The pictures round: the frames from where the last item stopped (`from.done` frames looked at), for SLICE seconds.
pub fn frames(path: &Path, from: &From, content: &mut Content) -> Result<Ended> {
    let start = from.done * FRAME_STEP;
    let until = start + SLICE;
    let video = av::read_video(path, FRAME_SIDE, FRAME_STEP, (start, until), &mut |at, frame| {
        content.frame(at, frame, &macos::signature(frame)?)
    })?;
    let done = (video.duration.min(until) / FRAME_STEP).ceil();
    if until < video.duration {
        return Ok(Ended { done, resume: Some(content.resume()), ..Ended::default() });
    }
    let info = json!({ "width": video.width, "height": video.height, "frames": content.frames });
    Ok(Ended::whole(info, done))
}
