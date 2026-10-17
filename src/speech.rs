//! What's said in recordings and videos, written down: the second stage of indexing them. The first pass makes them
//! searchable by what they sound like (EmbeddingGemma's 30-second windows); this one, which runs only while the daemon
//! has nothing else to do, adds what's said, so a name or a word heard in a podcast is found at its moment.
//!
//! The speech model (Parakeet Ultra, 25 languages, Dutch and English among them) runs in a helper process,
//! `sift-it-transcribe`, beside this binary: transcribe.cpp brings its own ggml, and the helper gives back all of its
//! memory when there's nothing left to write down. Sound goes to it in chunks of about three minutes, cut where it's
//! quietest so no word is cut in two, and each chunk's sentences are stored as soon as they're back: a file stopped
//! partway goes on from there later.

use crate::config;
use crate::embedder::AUDIO_RATE;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The speech model: Moondream's Parakeet Ultra (a post-trained Parakeet TDT 0.6B v3), Handy's GGUF conversion at Q8_0.
pub const MODEL_FILE: &str = "parakeet-ultra-Q8_0.gguf";
pub const MODEL_REPOSITORY: &str = "handy-computer/parakeet-ultra-gguf";
pub const MODEL_REVISION: &str = "a1539be1b849ea7463b88343e5277b373d0c4ea9";
pub const MODEL_BYTES: u64 = 740_363_168;
pub const MODEL_SHA256: &str = "007a59761e9258779f189df396b50d12b83bb5b79e66df4b955c230d2f2a0a59";

/// What wrote the transcripts: stored with the index, so a change of model or engine writes them again.
pub fn source() -> &'static str {
    "parakeet-ultra-q8_0/transcribe.cpp-0.4.0/v1"
}

/// Sound goes to the model in chunks of about this many samples…
const CHUNK: usize = 180 * AUDIO_RATE;
/// …cut at the quietest tenth of a second within this many samples of the mark.
const CUT_WINDOW: usize = 10 * AUDIO_RATE;
/// A chunk quieter than this throughout (RMS, about −50 dBFS) isn't sent at all.
const SILENCE: f32 = 0.003;
/// Sentences shorter than this are joined to the next one (when it follows within MAX_GAP seconds), so a passage
/// holds enough to mean something.
const MIN_PASSAGE: usize = 160;
const MAX_GAP: f64 = 4.0;

/// The speech model's file: in the models folder (`sift-it model download`), or else Handy's copy of the same file,
/// so both apps share one download.
pub fn model_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SIFT_IT_SPEECH_MODEL") {
        return Some(PathBuf::from(path)).filter(|path| path.exists());
    }
    let ours = config::models_dir().join(MODEL_FILE);
    if ours.exists() {
        return Some(ours);
    }
    let handy = config::dirs_home().join("Library/Application Support/com.pais.handy/models").join(MODEL_FILE);
    handy.exists().then_some(handy)
}

/// The helper, beside this binary (the release and `make install` put it there; cargo builds it beside it too).
pub fn helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SIFT_IT_TRANSCRIBE") {
        return Some(PathBuf::from(path)).filter(|path| path.exists());
    }
    let binary = std::env::current_exe().ok()?.canonicalize().ok()?;
    let helper = binary.parent()?.join("sift-it-transcribe");
    helper.exists().then_some(helper)
}

/// Why speech can't be written down now, if it can't.
pub fn unavailable() -> Option<String> {
    if helper_path().is_none() {
        return Some("sift-it-transcribe isn't installed beside sift-it".into());
    }
    if model_path().is_none() {
        return Some(format!("the speech model isn't downloaded (sift-it model download): no {MODEL_FILE}"));
    }
    None
}

/// A sentence or a few, as the model heard it, with its time in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Spoken {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// The helper stopped (it crashed, or was killed): no fault of the file being written down, which is tried again with
/// a new helper.
#[derive(Debug)]
pub struct Gone;
impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the speech model's helper stopped")
    }
}
impl std::error::Error for Gone {}

/// The helper process, with the model loaded.
pub struct Transcriber {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Transcriber {
    /// Starts the helper, at a lower priority than everything else, and waits until its model has loaded.
    pub fn start() -> Result<Transcriber> {
        if let Some(why) = unavailable() {
            bail!(why);
        }
        let (helper, model) = (helper_path().expect("checked"), model_path().expect("checked"));
        let mut command = Command::new(&helper);
        command.arg(&model).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Searching and the first pass come first.
        unsafe {
            command.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(|| format!("can't start {}", helper.display()))?;
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut line = String::new();
        stdout.read_line(&mut line)?;
        let hello: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if hello["event"] != "ready" {
            let _ = child.kill();
            let _ = child.wait();
            let why = hello["error"].as_str().unwrap_or("it stopped before saying it was ready");
            bail!("the speech model didn't start: {why}");
        }
        Ok(Transcriber { child, stdin, stdout })
    }

    /// What's said in a stretch of 16 kHz mono sound, with times from its start.
    pub fn run(&mut self, samples: &[f32]) -> Result<Vec<Spoken>> {
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow::Error::new(Gone))?;
        let mut frame = Vec::with_capacity(4 + samples.len() * 4);
        frame.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        for sample in samples {
            frame.extend_from_slice(&sample.to_le_bytes());
        }
        if stdin.write_all(&frame).and_then(|_| stdin.flush()).is_err() {
            return Err(Gone.into());
        }
        let mut line = String::new();
        if self.stdout.read_line(&mut line).unwrap_or(0) == 0 {
            return Err(Gone.into());
        }
        let reply: serde_json::Value = serde_json::from_str(&line).context("the speech model said something odd")?;
        if let Some(error) = reply["error"].as_str() {
            bail!("the speech model failed: {error}");
        }
        Ok(reply["segments"]
            .as_array()
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| {
                        Some(Spoken {
                            start: segment["start"].as_f64()?,
                            end: segment["end"].as_f64()?,
                            text: segment["text"].as_str()?.trim().to_string(),
                        })
                    })
                    .filter(|spoken| !spoken.text.is_empty())
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl Drop for Transcriber {
    /// Closing its stdin ends the helper; it's waited for, so its memory is back when this returns.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// How writing down a file went.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// All of it, up to this second.
    Finished(f64),
    /// Stopped after a chunk (`on_chunk` said so); it goes on from where that chunk ended.
    Stopped,
}

/// Stops reading a file once a chunk is stored and `on_chunk` asks to stop.
#[derive(Debug)]
struct Stop;
impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Stop {}

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
    let read = crate::av::read_samples(path, from, &mut |samples| {
        buffer.extend_from_slice(samples);
        while buffer.len() >= CHUNK + CUT_WINDOW {
            let cut = quiet_point(&buffer, CHUNK);
            let chunk: Vec<f32> = buffer.drain(..cut).collect();
            let chunk_start = start;
            start += chunk.len() as f64 / AUDIO_RATE as f64;
            if !send(&chunk, chunk_start)? {
                return Err(Stop.into());
            }
        }
        Ok(())
    });
    match read {
        Ok(_) => {}
        Err(error) if error.downcast_ref::<Stop>().is_some() => return Ok(Outcome::Stopped),
        Err(error) => return Err(error),
    }
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

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Sentences joined into passages: short ones with what follows closely, so each passage holds enough to embed.
pub fn passages(spoken: Vec<Spoken>) -> Vec<Spoken> {
    let mut passages: Vec<Spoken> = Vec::new();
    for sentence in spoken {
        match passages.last_mut() {
            Some(last) if last.text.len() < MIN_PASSAGE && sentence.start - last.end <= MAX_GAP => {
                last.text.push(' ');
                last.text.push_str(&sentence.text);
                last.end = sentence.end;
            }
            _ => passages.push(sentence),
        }
    }
    passages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spoken(start: f64, end: f64, text: &str) -> Spoken {
        Spoken { start, end, text: text.into() }
    }

    #[test]
    fn short_sentences_join_what_follows_closely() {
        let joined = passages(vec![
            spoken(0.0, 2.0, "Hi."),
            spoken(2.5, 9.0, "Today we talk about Debian."),
            spoken(30.0, 32.0, "After a long pause."),
            spoken(32.5, 60.0, &"A long sentence about installing Ubuntu on a laptop. ".repeat(4)),
        ]);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].text, "Hi. Today we talk about Debian.");
        assert_eq!((joined[0].start, joined[0].end), (0.0, 9.0));
        assert_eq!(joined[1].start, 30.0);
    }

    #[test]
    fn a_cut_falls_in_the_quiet() {
        let mut samples = vec![0.5f32; CHUNK + CUT_WINDOW];
        let quiet = CHUNK + AUDIO_RATE * 3;
        samples[quiet..quiet + AUDIO_RATE / 5].iter_mut().for_each(|s| *s = 0.0);
        let cut = quiet_point(&samples, CHUNK);
        assert!(cut >= quiet && cut <= quiet + AUDIO_RATE / 5, "{cut} vs {quiet}");
    }
}
