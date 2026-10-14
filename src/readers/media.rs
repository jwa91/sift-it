//! Recordings and videos. Sound becomes 30-second windows (see av.rs), each a vector of what's heard in it, so a match
//! points to its moment. A video's frames (one per shot, and every 30 s within a long one) are pictures with their
//! time; its sound is read the same way as a recording's.

use super::{Payload, Piece, Sink};
use crate::av;
use crate::imaging;
use crate::mac;
use crate::store::Segment;
use anyhow::Result;
use std::path::Path;

/// Video frames are small things to embed: 70 tokens each.
pub const FRAME_BUDGET: u32 = 70;
/// Frames are taken at most this large (enough for 70 tokens, and quick to decode).
const FRAME_SIDE: u32 = 640;

pub fn read_audio(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let mut windows = 0usize;
    let seconds = av::read_audio(path, &mut |start, samples| {
        windows += 1;
        sound(start, samples, sink)
    })?;
    Ok(serde_json::json!({ "duration": seconds, "windows": windows }))
}

pub fn read_video(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let video = av::read_video(path, FRAME_SIDE, &mut |at, frame| {
        let pixels = imaging::fit(&mac::pixels(frame)?, FRAME_BUDGET)?;
        sink(Piece {
            segment: Segment::Frame,
            loc: Some(at),
            loc_end: None,
            text: None,
            payload: Payload::Image(pixels),
        })
    })?;
    let mut windows = 0usize;
    if av::has_audio(path) {
        av::read_audio(path, &mut |start, samples| {
            windows += 1;
            sound(start, samples, sink)
        })?;
    }
    Ok(serde_json::json!({
        "duration": video.duration,
        "width": video.width,
        "height": video.height,
        "frames": video.frames,
        "windows": windows,
    }))
}

fn sound(start: f64, samples: &[f32], sink: Sink) -> Result<()> {
    let end = start + samples.len() as f64 / crate::embedder::AUDIO_RATE as f64;
    sink(Piece {
        segment: Segment::Sound,
        loc: Some(start),
        loc_end: Some(end),
        text: None,
        payload: Payload::Audio(samples.to_vec()),
    })
}
