//! Speech: what's said in a recording or a video's sound, written down by the speech model (adapters/speech.rs). Sound
//! goes to it in chunks of about three minutes, cut where it's quietest so no word is cut in two, and each chunk's
//! sentences, with their times, are handed over as soon as they're back: a file stopped partway goes on from there
//! later. A slow extractor, so it runs on a lane of its own (ingest/listen.rs).

use crate::adapters::speech::{Spoken, Transcriber};
use crate::ingest::segment::sound::{SILENCE, rms};
use anyhow::{Result, bail};
use embedder::AUDIO_RATE;
use std::path::Path;

/// Sound goes to the model in chunks of about this many samples…
const CHUNK: usize = 180 * AUDIO_RATE;
/// …cut at the quietest tenth of a second within this many samples of the mark. A silent chunk isn't sent at all.
const CUT_WINDOW: usize = 10 * AUDIO_RATE;

/// How writing down a file went.
pub enum Outcome {
    /// All of it, up to this second.
    Finished(f64),
    /// Stopped after a chunk (`on_chunk` said so); it goes on from where that chunk ended.
    Stopped,
}

/// Writes down what's said in a file from `from` seconds on, a chunk at a time: after each, `on_chunk(start, end,
/// sentences)` (seconds from the file's start) stores it and says whether to go on. After the last chunk the file is
/// finished, whatever `on_chunk` says: there's nothing to go on with.
pub fn transcribe(
    transcriber: &mut Transcriber,
    path: &Path,
    from: f64,
    on_chunk: &mut dyn FnMut(f64, f64, Vec<Spoken>) -> Result<bool>,
) -> Result<Outcome> {
    let mut buffer: Vec<f32> = Vec::with_capacity(CHUNK + CUT_WINDOW * 2);
    let mut start = from; // the second of buffer[0]
    let mut send = |chunk: &[f32], start: f64| -> Result<bool> {
        let end = start + chunk.len() as f64 / AUDIO_RATE as f64;
        let spoken = if rms(chunk) < SILENCE { Vec::new() } else { transcriber.run(chunk)? };
        let spoken = spoken
            .into_iter()
            .map(|s| Spoken { start: start + s.start, end: (start + s.end).min(end), text: s.text })
            .collect();
        on_chunk(start, end, spoken)
    };
    let mut stopped = false;
    let read = crate::adapters::macos::av::read_samples(path, from, &mut |samples| {
        buffer.extend_from_slice(samples);
        while buffer.len() >= CHUNK + CUT_WINDOW {
            let cut = quiet_point(&buffer, CHUNK);
            let chunk: Vec<f32> = buffer.drain(..cut).collect();
            let chunk_start = start;
            start += chunk.len() as f64 / AUDIO_RATE as f64;
            if !send(&chunk, chunk_start)? {
                // Stops reading the file: it goes on from here later.
                stopped = true;
                bail!("stopped");
            }
        }
        Ok(())
    });
    if stopped {
        return Ok(Outcome::Stopped);
    }
    read?;
    let end = start + buffer.len() as f64 / AUDIO_RATE as f64;
    if buffer.len() >= AUDIO_RATE / 2 {
        send(&buffer, start)?;
    }
    Ok(Outcome::Finished(end))
}

/// Where to cut near `mark`: the middle of the quietest tenth of a second within CUT_WINDOW of it.
fn quiet_point(samples: &[f32], mark: usize) -> usize {
    let frame = AUDIO_RATE / 10;
    let from = mark.saturating_sub(CUT_WINDOW);
    let to = (mark + CUT_WINDOW).min(samples.len());
    let mut best = (f32::MAX, mark);
    let mut at = from;
    while at + frame <= to {
        let level = rms(&samples[at..at + frame]);
        if level < best.0 {
            best = (level, at + frame / 2);
        }
        at += frame;
    }
    best.1.clamp(1, samples.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_falls_in_the_quiet() {
        let mut samples = vec![0.5f32; CHUNK + CUT_WINDOW];
        let quiet = CHUNK + AUDIO_RATE * 3;
        samples[quiet..quiet + AUDIO_RATE / 5].iter_mut().for_each(|s| *s = 0.0);
        let cut = quiet_point(&samples, CHUNK);
        assert!(cut >= quiet && cut <= quiet + AUDIO_RATE / 5, "{cut} vs {quiet}");
    }
}
