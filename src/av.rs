//! Recordings and videos through AVFoundation, which plays every format macOS can.
//!
//! Sound is decoded to 16 kHz mono and handed over in 30-second windows that overlap by 10 seconds (the model listens
//! to 30 seconds at a time; the overlap keeps a sentence at a window's edge whole in one of them). Silent windows are
//! skipped. Video is sampled every few seconds, and a frame is kept when the picture changes (a new shot) or every
//! 30 seconds of the same one.
//!
//! It uses AVFoundation's synchronous calls (deprecated for the async ones): they block a worker thread, which is what
//! a reader wants.
#![allow(deprecated)]

use crate::embedder::{AUDIO_RATE, AUDIO_WINDOW};
use anyhow::{Result, anyhow, bail};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetImageGenerator, AVAssetReader, AVAssetReaderAudioMixOutput, AVAssetReaderStatus, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::{CGFloat, CGSize};
use objc2_core_graphics::CGImage;
use objc2_core_media::CMTime;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::path::Path;
use std::ptr::NonNull;

/// Windows start this far apart: 30 s windows overlapping by 10 s.
pub const AUDIO_HOP: usize = 20 * AUDIO_RATE;
/// A window quieter than this (RMS, full scale 1.0, about −50 dBFS) is silence.
const SILENCE: f32 = 0.003;
/// Video is sampled at least this often, and at most this many times per file.
const FRAME_STEP: f64 = 2.0;
const MAX_SAMPLES: f64 = 900.0;
/// A sampled frame this different from the last kept one (mean difference of a 16 × 9 grey thumbnail, 0–255)
/// starts a new shot.
const SHOT_CHANGE: f64 = 14.0;
/// Within one shot, a frame is kept at least this often.
const KEEP_EVERY: f64 = 30.0;

fn asset(path: &Path) -> Retained<AVURLAsset> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) }
}

/// The file's length in seconds (0 when it can't tell).
pub fn duration(path: &Path) -> f64 {
    let seconds = unsafe { asset(path).duration().seconds() };
    if seconds.is_finite() { seconds.max(0.0) } else { 0.0 }
}

pub fn has_audio(path: &Path) -> bool {
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return false };
    unsafe { asset(path).tracksWithMediaType(audio).count() > 0 }
}

/// Decodes the sound and hands over its windows: (start in seconds, samples). Returns the seconds of sound read.
pub fn read_audio(path: &Path, on_window: &mut dyn FnMut(f64, &[f32]) -> Result<()>) -> Result<f64> {
    let asset = asset(path);
    let audio = unsafe { AVMediaTypeAudio }.ok_or_else(|| anyhow!("no audio media type"))?;
    let tracks = unsafe { asset.tracksWithMediaType(audio) };
    if tracks.count() == 0 {
        bail!("no sound in it");
    }
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
        .map_err(|error| anyhow!("AVFoundation: {}", error.localizedDescription()))?;
    // Linear PCM, 32-bit float, mono, 16 kHz: AVFoundation mixes the tracks down and resamples.
    let keys = [
        "AVFormatIDKey",
        "AVSampleRateKey",
        "AVNumberOfChannelsKey",
        "AVLinearPCMBitDepthKey",
        "AVLinearPCMIsFloatKey",
        "AVLinearPCMIsBigEndianKey",
        "AVLinearPCMIsNonInterleaved",
    ]
    .map(NSString::from_str);
    let values = [
        NSNumber::new_u32(u32::from_be_bytes(*b"lpcm")),
        NSNumber::new_f64(AUDIO_RATE as f64),
        NSNumber::new_u32(1),
        NSNumber::new_u32(32),
        NSNumber::new_bool(true),
        NSNumber::new_bool(false),
        NSNumber::new_bool(false),
    ];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&AnyObject> = values.iter().map(|v| v.as_ref()).collect();
    let settings = NSDictionary::from_slices(&key_refs, &value_refs);
    let output = unsafe {
        AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(&tracks, Some(&settings))
    };
    unsafe {
        output.setAlwaysCopiesSampleData(false);
        if !reader.canAddOutput(&output) {
            bail!("AVFoundation can't decode its sound");
        }
        reader.addOutput(&output);
        if !reader.startReading() {
            bail!("AVFoundation can't read it");
        }
    }

    let mut buffer: Vec<f32> = Vec::with_capacity(AUDIO_WINDOW * 2);
    let mut buffer_start = 0usize; // the sample number of buffer[0]
    let mut total = 0usize;
    let mut emitted = false;
    let mut emit = |start: usize, samples: &[f32], emitted: &mut bool| -> Result<()> {
        *emitted = true;
        if rms(samples) < SILENCE {
            return Ok(());
        }
        on_window(start as f64 / AUDIO_RATE as f64, samples)
    };
    loop {
        let more = autoreleasepool(|_| -> Result<bool> {
            let Some(sample) = (unsafe { output.copyNextSampleBuffer() }) else { return Ok(false) };
            let Some(block) = (unsafe { sample.data_buffer() }) else { return Ok(true) };
            let bytes = unsafe { block.data_length() };
            let count = bytes / 4;
            let old = buffer.len();
            buffer.resize(old + count, 0.0);
            let destination = NonNull::new(buffer[old..].as_mut_ptr()).unwrap().cast();
            let status = unsafe { block.copy_data_bytes(0, count * 4, destination) };
            if status != 0 {
                buffer.truncate(old);
                bail!("couldn't copy decoded sound ({status})");
            }
            total += count;
            Ok(true)
        })?;
        while buffer.len() >= AUDIO_WINDOW {
            emit(buffer_start, &buffer[..AUDIO_WINDOW], &mut emitted)?;
            buffer.drain(..AUDIO_HOP);
            buffer_start += AUDIO_HOP;
        }
        if !more {
            break;
        }
    }
    if unsafe { reader.status() } == AVAssetReaderStatus::Failed {
        let reason = unsafe { reader.error() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
        bail!("AVFoundation stopped reading it: {reason}");
    }
    // The rest: on its own if nothing came before; else only when it holds a few seconds the last window didn't.
    let uncovered = buffer.len().saturating_sub(AUDIO_WINDOW - AUDIO_HOP);
    if (!emitted && buffer.len() >= AUDIO_RATE) || (emitted && uncovered >= 3 * AUDIO_RATE) {
        emit(buffer_start, &buffer, &mut emitted)?;
    }
    Ok(total as f64 / AUDIO_RATE as f64)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

pub struct VideoInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
}

fn generator(path: &Path, max_side: u32, tolerance: f64) -> Retained<AVAssetImageGenerator> {
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset(path)) };
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize::new(max_side as CGFloat, max_side as CGFloat));
        let tolerance = CMTime::with_seconds(tolerance, 600);
        generator.setRequestedTimeToleranceBefore(tolerance);
        generator.setRequestedTimeToleranceAfter(tolerance);
    }
    generator
}

/// One frame, at most `max_side` pixels on the long side (for previews).
pub fn frame_at(path: &Path, seconds: f64, max_side: u32) -> Result<Retained<CGImage>> {
    let generator = generator(path, max_side, 0.0);
    let time = unsafe { CMTime::with_seconds(seconds.max(0.0), 600) };
    unsafe { generator.copyCGImageAtTime_actualTime_error(time, std::ptr::null_mut()) }
        .map_err(|error| anyhow!("AVFoundation: {}", error.localizedDescription()))
}

/// Walks the video and hands over the frames worth keeping: (time in seconds, frame).
pub fn read_video(
    path: &Path,
    max_side: u32,
    on_frame: &mut dyn FnMut(f64, &CGImage) -> Result<()>,
) -> Result<VideoInfo> {
    let asset = asset(path);
    let video = unsafe { AVMediaTypeVideo }.ok_or_else(|| anyhow!("no video media type"))?;
    let tracks = unsafe { asset.tracksWithMediaType(video) };
    let Some(track) = tracks.firstObject() else { bail!("no picture in it") };
    let size = unsafe { track.naturalSize() };
    let duration = unsafe { asset.duration().seconds() };
    if !duration.is_finite() || duration <= 0.0 {
        bail!("it has no length");
    }
    let step = FRAME_STEP.max(duration / MAX_SAMPLES);
    let generator = generator(path, max_side, step / 2.0);
    let mut last_kept: Option<(f64, Vec<u8>)> = None;
    let mut frames = 0usize;
    let mut time = 0.0f64;
    while time < duration {
        autoreleasepool(|_| -> Result<()> {
            let mut actual = unsafe { CMTime::with_seconds(time, 600) };
            let requested = actual;
            let Ok(frame) = (unsafe { generator.copyCGImageAtTime_actualTime_error(requested, &mut actual) }) else {
                return Ok(());
            };
            let at = unsafe { actual.seconds() };
            let at = if at.is_finite() { at } else { time };
            let signature = crate::mac::signature(&frame)?;
            let keep = match &last_kept {
                None => true,
                Some((kept_at, kept)) => at - kept_at >= KEEP_EVERY || difference(kept, &signature) >= SHOT_CHANGE,
            };
            if keep {
                on_frame(at, &frame)?;
                frames += 1;
                last_kept = Some((at, signature));
            }
            Ok(())
        })?;
        time += step;
    }
    Ok(VideoInfo { duration, width: size.width.abs() as u32, height: size.height.abs() as u32, frames })
}

fn difference(a: &[u8], b: &[u8]) -> f64 {
    let sum: u64 = a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum();
    sum as f64 / a.len().max(1) as f64
}
