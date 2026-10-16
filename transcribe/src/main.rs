//! `sift-it-transcribe <model.gguf>`: speech to text for sift-it's daemon, in a process of its own. transcribe.cpp
//! brings its own ggml, which can't share a binary with the one llama.cpp brings; a process of its own also gives
//! back all of its memory when the daemon has nothing more to transcribe and closes its stdin.
//!
//! It loads the model, says so on stdout, then reads chunks of sound from stdin until it closes: a little-endian u32
//! sample count, then that many little-endian f32 samples (16 kHz mono, -1 to 1). For each chunk it writes one JSON
//! line on stdout, with times in seconds from the chunk's start:
//!
//!   {"event":"ready","model":"parakeet","backend":"MTL0"}
//!   {"segments":[{"start":5.68,"end":28.24,"text":"This is Hacker Public Radio…"}],"language":"en"}
//!   {"error":"…"}                         (that chunk failed; the next one is read as usual)

use serde_json::json;
use std::io::{BufWriter, Read, Write};
use transcribe_cpp::{Model, RunOptions, TimestampKind};

fn main() {
    let Some(model_path) = std::env::args().nth(1) else {
        eprintln!("usage: sift-it-transcribe <model.gguf>");
        std::process::exit(2);
    };
    transcribe_cpp::disable_logging();
    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut say = |value: serde_json::Value| {
        let _ = writeln!(out, "{value}").and_then(|_| out.flush());
    };
    let model = match Model::load(&model_path) {
        Ok(model) => model,
        Err(error) => {
            say(json!({"error": format!("can't load {model_path}: {error}")}));
            std::process::exit(1);
        }
    };
    let mut session = match model.session() {
        Ok(session) => session,
        Err(error) => {
            say(json!({"error": format!("can't start a session: {error}")}));
            std::process::exit(1);
        }
    };
    say(json!({"event": "ready", "model": model.arch(), "backend": model.backend()}));

    let options = RunOptions { timestamps: TimestampKind::Segment, ..Default::default() };
    let mut input = std::io::stdin().lock();
    let mut header = [0u8; 4];
    let mut bytes = Vec::new();
    let mut samples: Vec<f32> = Vec::new();
    while input.read_exact(&mut header).is_ok() {
        let count = u32::from_le_bytes(header) as usize;
        bytes.resize(count * 4, 0);
        if input.read_exact(&mut bytes).is_err() {
            break;
        }
        samples.clear();
        samples.extend(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
        if samples.is_empty() {
            say(json!({"segments": []}));
            continue;
        }
        match session.run(&samples, &options) {
            Ok(transcript) => {
                let segments: Vec<serde_json::Value> = transcript
                    .segments
                    .iter()
                    .filter(|segment| !segment.text.trim().is_empty())
                    .map(|segment| {
                        json!({
                            "start": segment.t0_ms as f64 / 1000.0,
                            "end": segment.t1_ms as f64 / 1000.0,
                            "text": segment.text.trim(),
                        })
                    })
                    .collect();
                say(json!({"segments": segments, "language": transcript.language}));
            }
            Err(error) => say(json!({"error": error.to_string()})),
        }
    }
}
