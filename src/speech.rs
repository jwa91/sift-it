//! The speech model, in a helper process: what writes down what's said in recordings and videos (the speech extractor,
//! ingest/extract/speech.rs, sends it sound).
//!
//! The model (Parakeet Ultra, 25 languages, Dutch and English among them) runs in `sift-it-transcribe`, beside this
//! binary: transcribe.cpp brings its own ggml, and the helper gives back all of its memory when there's nothing left to
//! write down.

use crate::config;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
