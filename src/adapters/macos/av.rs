//! Recordings and videos through AVFoundation, which plays every format macOS can.
//!
//! Sound is decoded to 16 kHz mono, the same on every read (docs/audio-decoding.md), and video sampled every few
//! seconds. Which windows of sound and which frames become segments is the segmenters' to say (ingest/segment).
//!
//! It uses AVFoundation's synchronous calls (deprecated for the async ones): they block a worker thread, which is what
//! a reader wants.
#![allow(deprecated)]

use anyhow::{Result, anyhow, bail};
use embedder::AUDIO_RATE;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetImageGenerator, AVAssetReader, AVAssetReaderAudioMixOutput, AVAssetReaderOutput, AVAssetReaderStatus,
    AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::{CGFloat, CGSize};
use objc2_core_graphics::CGImage;
use objc2_core_media::{CMTime, CMTimeRange};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use std::path::Path;

mod resample;

/// Video is sampled at least this often, and at most this many times per file.
const FRAME_STEP: f64 = 2.0;
const MAX_SAMPLES: f64 = 900.0;

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

/// Linear PCM, 32-bit float, mono, at the file's own rate: AVFoundation decodes and mixes the channels down, and
/// resample.rs takes it to 16 kHz.
fn pcm_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys = [
        "AVFormatIDKey",
        "AVNumberOfChannelsKey",
        "AVLinearPCMBitDepthKey",
        "AVLinearPCMIsFloatKey",
        "AVLinearPCMIsBigEndianKey",
        "AVLinearPCMIsNonInterleaved",
    ]
    .map(NSString::from_str);
    let values = [
        NSNumber::new_u32(u32::from_be_bytes(*b"lpcm")),
        NSNumber::new_u32(1),
        NSNumber::new_u32(32),
        NSNumber::new_bool(true),
        NSNumber::new_bool(false),
        NSNumber::new_bool(false),
    ];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&AnyObject> = values.iter().map(|v| v.as_ref()).collect();
    NSDictionary::from_slices(&key_refs, &value_refs)
}

/// How a pass of the reader ended.
enum Pass {
    Finished,
    /// AVFoundation gave up partway (seen on long voice memos: its converter fails with -50 at one packet, though the
    /// file plays and ffmpeg decodes it whole).
    Failed(String),
}

/// One pass of the reader from `start_seconds` on, handing over the decoded samples.
fn read_pass(
    asset: &AVURLAsset,
    tracks: &NSArray<AVAssetTrack>,
    start_seconds: f64,
    on_samples: &mut dyn FnMut(&[f32]) -> Result<()>,
) -> Result<Pass> {
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(asset) }
        .map_err(|error| anyhow!("AVFoundation: {}", describe(&error)))?;
    // To the end of the tracks: the asset's estimated duration can leave out AAC's last partial buffer.
    let end = tracks.iter().map(|track| unsafe { track.timeRange().end().seconds() }).fold(0.0, f64::max);
    let seconds = (end - start_seconds).max(0.0);
    if seconds == 0.0 {
        return Ok(Pass::Finished);
    }
    let start = unsafe { CMTime::with_seconds(start_seconds, AUDIO_RATE as i32) };
    let duration = unsafe { CMTime::with_seconds(seconds, 1_000_000_000) };
    unsafe { reader.setTimeRange(CMTimeRange { start, duration }) };
    let settings = pcm_settings();
    // One track is read as it is; several are mixed down.
    let output: Retained<AVAssetReaderOutput> = if tracks.count() == 1 {
        let track = tracks.objectAtIndex(0);
        Retained::into_super(unsafe {
            AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&settings))
        })
    } else {
        Retained::into_super(unsafe {
            AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(tracks, Some(&settings))
        })
    };
    unsafe {
        output.setAlwaysCopiesSampleData(false);
        if !reader.canAddOutput(&output) {
            bail!("AVFoundation can't decode its sound");
        }
        reader.addOutput(&output);
        if !reader.startReading() {
            let reason = reader.error().map(|e| describe(&e)).unwrap_or_default();
            bail!("AVFoundation can't read it: {reason}");
        }
    }
    resample::read(&output, seconds, on_samples)?;
    if unsafe { reader.status() } == AVAssetReaderStatus::Failed {
        let reason = unsafe { reader.error() }.map(|e| describe(&e)).unwrap_or_default();
        return Ok(Pass::Failed(reason));
    }
    if unsafe { reader.status() } != AVAssetReaderStatus::Completed {
        bail!("AVFoundation stopped before the sound ended");
    }
    Ok(Pass::Finished)
}

/// Decodes the sound from `from` seconds on, handing it over as it comes (16 kHz mono). Returns the seconds of sound
/// read, counted from 0.
///
/// When AVFoundation gives up partway, reading starts again a second past where it stopped, and that second is
/// silence: the timeline stays true. Only a file it can't get past fails.
pub fn read_samples(path: &Path, from: f64, on_samples: &mut dyn FnMut(&[f32]) -> Result<()>) -> Result<f64> {
    const SKIP: usize = AUDIO_RATE;
    const MAX_RESUMES: usize = 50;
    let asset = asset(path);
    let audio = unsafe { AVMediaTypeAudio }.ok_or_else(|| anyhow!("no audio media type"))?;
    let tracks = unsafe { asset.tracksWithMediaType(audio) };
    if tracks.count() == 0 {
        bail!("no sound in it");
    }
    let first = (from.max(0.0) * AUDIO_RATE as f64) as usize;
    let mut total = first; // samples read (and silence put in for skipped stretches)
    let mut resumes = 0;
    loop {
        let pass = read_pass(&asset, &tracks, total as f64 / AUDIO_RATE as f64, &mut |samples| {
            total += samples.len();
            on_samples(samples)
        })?;
        match pass {
            Pass::Finished => break,
            Pass::Failed(reason) => {
                resumes += 1;
                if resumes > MAX_RESUMES {
                    bail!("AVFoundation stopped reading it {MAX_RESUMES} times; the last: {reason}");
                }
                // Past the spot it stumbled on, keeping the timeline: a second of silence stands in for it.
                total += SKIP;
                on_samples(&vec![0.0f32; SKIP])?;
            }
        }
    }
    Ok(total as f64 / AUDIO_RATE as f64)
}

/// The tags a recording or video carries (ID3, iTunes, QuickTime): its title, artist, album and the like, as
/// {"title": …, "artist": …}. Empty when it has none. Each is cut at 500 characters: they travel with every result in
/// its file's info, and a description tag can hold a whole text.
pub fn tags(path: &Path) -> serde_json::Value {
    const KEYS: [(&str, &str); 6] = [
        ("title", "title"),
        ("artist", "artist"),
        ("albumName", "album"),
        ("creator", "creator"),
        ("subject", "subject"),
        ("description", "description"),
    ];
    let mut tags = serde_json::Map::new();
    autoreleasepool(|_| {
        let items = unsafe { asset(path).commonMetadata() };
        for item in items.iter() {
            let (Some(key), Some(value)) = (unsafe { item.commonKey() }, unsafe { item.stringValue() }) else {
                continue;
            };
            let key = key.to_string();
            let value = value.to_string().trim().to_string();
            if let Some((_, name)) = KEYS.iter().find(|(common, _)| *common == key)
                && !value.is_empty()
                && !tags.contains_key(*name)
            {
                tags.insert(name.to_string(), serde_json::Value::String(value.chars().take(500).collect()));
            }
        }
    });
    serde_json::Value::Object(tags)
}

/// A recording's loudness in `columns` slices (peak of each, scaled so the loudest is 1), for a waveform.
pub fn envelope(path: &Path, columns: usize) -> Result<Vec<f32>> {
    let per_column = ((duration(path).max(0.1) * AUDIO_RATE as f64) / columns as f64).max(1.0);
    let mut peaks = vec![0.0f32; columns];
    let mut sample = 0usize;
    read_samples(path, 0.0, &mut |samples| {
        for value in samples {
            let column = ((sample as f64 / per_column) as usize).min(columns - 1);
            peaks[column] = peaks[column].max(value.abs());
            sample += 1;
        }
        Ok(())
    })?;
    // Loudness by ear: a square root lifts quiet speech so it shows next to loud passages.
    let loudest = peaks.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
    Ok(peaks.into_iter().map(|peak| (peak / loudest).sqrt()).collect())
}

/// An NSError with what "The operation could not be completed" leaves out: its domain, code, and underlying errors.
fn describe(error: &objc2_foundation::NSError) -> String {
    let mut text = format!("{} ({} {})", error.localizedDescription(), error.domain(), error.code());
    for underlying in error.underlyingErrors().iter() {
        text.push_str(&format!("; {}", describe(&underlying)));
    }
    text
}

pub struct VideoInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
}

fn generator(asset: &AVURLAsset, max_side: u32, tolerance: f64) -> Retained<AVAssetImageGenerator> {
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(asset) };
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
    let generator = generator(&asset(path), max_side, 0.0);
    let time = unsafe { CMTime::with_seconds(seconds.max(0.0), 600) };
    unsafe { generator.copyCGImageAtTime_actualTime_error(time, std::ptr::null_mut()) }
        .map_err(|error| anyhow!("AVFoundation: {}", error.localizedDescription()))
}

/// Walks the video and hands over a frame every few seconds: (time in seconds, frame).
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
    let generator = generator(&asset, max_side, step / 2.0);
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
            on_frame(at, &frame)
        })?;
        time += step;
    }
    Ok(VideoInfo { duration, width: size.width.abs() as u32, height: size.height.abs() as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(path: &Path, from: f64, slow: bool) -> (f64, Vec<u32>) {
        let mut bits = Vec::new();
        let seconds = read_samples(path, from, &mut |samples| {
            bits.extend(samples.iter().map(|sample| sample.to_bits()));
            if slow {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        })
        .unwrap();
        (seconds, bits)
    }

    #[test]
    fn compressed_sound_is_independent_of_reader_timing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio");
        for (name, frames) in [("priming.mp3", 123716), ("partial.m4a", 19760)] {
            let path = root.join(name);
            let expected = decode(&path, 0.0, false);
            assert_eq!(expected.1.len(), frames, "{name}");
            for run in 0..20 {
                let actual = decode(&path, 0.0, run % 2 == 0);
                assert_eq!(actual.0, expected.0, "{name}, run {run}: duration");
                assert_eq!(actual.1.len(), expected.1.len(), "{name}, run {run}: length");
                let difference = actual.1.iter().zip(&expected.1).position(|(a, b)| a != b);
                assert_eq!(difference, None, "{name}, run {run}: first different sample");
            }
            let (end, tail) = decode(&path, 1.0, false);
            assert_eq!(end, expected.0, "{name}, resumed duration");
            assert_eq!(tail.len() + AUDIO_RATE, frames, "{name}, resumed length");
        }
    }

    #[test]
    fn sound_consumer_errors_stop_decoding() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio/priming.mp3");
        let error = read_samples(&path, 0.0, &mut |_| bail!("consumer stopped")).unwrap_err();
        assert_eq!(error.to_string(), "consumer stopped");
    }
}
