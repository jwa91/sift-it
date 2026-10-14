//! `sift-it serve`: the searcher as a process, for front ends (the Raycast extension is one). It keeps the index's vectors in memory and the
//! query encoder loaded, so a search while typing costs milliseconds, not a process start.
//!
//! One JSON request per line on stdin, one JSON reply per line on stdout carrying the request's `id`. Lines without an
//! `id` are events. Previews are made on a thread of their own, so a search never waits behind one: their replies can
//! come after replies to later requests.
//!
//!   {"id":1,"op":"search","q":"…","kinds":["pdf"],"limit":40,"prefix_last":true}
//!       → {"id":1,"hits":[…],"meaning":true,"ms":3.1}     meaning false: words only (the encoder is still loading)
//!   {"id":2,"op":"recent","limit":40}                    → {"id":2,"hits":[…]}: recently changed files
//!   {"id":3,"op":"preview","path":"…","kind":"pdf","loc":3,"size":512} → {"id":3,"image":"/…/preview.jpg"}
//!   {"id":5,"op":"preview","path":"…","kind":"waveform","loc":60,"loc_end":90,"size":720} → a recording's
//!       waveform (PNG) with the stretch from loc to loc_end seconds highlighted
//!   {"id":6,"op":"text","segment_id":12} or {"op":"text","path":"…"} → {"text":…}: a segment's whole text, or a
//!       file's (all its text segments, in order), for a preview
//!   {"id":4,"op":"status"}                               → {"id":4,"vectors":n,"encoder":"ready",…}
//!   {"id":5,"op":"quit"}
//!
//!   events: {"event":"ready","vectors":n}   {"event":"encoder","state":"ready"|"unavailable","error":…}
//!
//! It ends when its stdin closes (the front end went away).

use crate::config;
use crate::embedder::{Embedder, ModelFiles};
use crate::preview;
use crate::search::{Request, Searcher};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

enum Encoder {
    Loading,
    Ready(Embedder),
    Unavailable(String),
}

fn emit(out: &Mutex<std::io::Stdout>, value: &Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

pub fn serve(index: &Path, models: &Path) -> Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    std::thread::spawn(|| preview::prune(&config::previews_dir()));
    let mut searcher = Searcher::open(index)?;
    emit(&out, &json!({"event": "ready", "vectors": searcher.vector_count(), "index": index.display().to_string()}));

    let encoder = Arc::new(Mutex::new(Encoder::Loading));
    let loader = {
        let encoder = encoder.clone();
        let out = out.clone();
        let models = models.to_path_buf();
        std::thread::spawn(move || {
            let loaded = ModelFiles::in_folder(&models).and_then(|files| Embedder::load(&files, true));
            let event = match &loaded {
                Ok(_) => json!({"event": "encoder", "state": "ready"}),
                Err(error) => json!({"event": "encoder", "state": "unavailable", "error": format!("{error:#}")}),
            };
            *encoder.lock().unwrap() = match loaded {
                Ok(embedder) => Encoder::Ready(embedder),
                Err(error) => Encoder::Unavailable(format!("{error:#}")),
            };
            emit(&out, &event);
        })
    };

    // A video frame, a PDF page or a long recording's waveform can take a while (seconds, the first time): previews
    // are made one after another here, beside the searches.
    let (previews, requests) = std::sync::mpsc::channel::<(Value, Value)>();
    {
        let out = out.clone();
        std::thread::spawn(move || {
            for (id, request) in requests {
                let mut reply =
                    preview(&request).unwrap_or_else(|error| json!({"ok": false, "error": format!("{error:#}")}));
                reply["id"] = id;
                emit(&out, &reply);
            }
        });
    }

    let mut cache: HashMap<String, Vec<f32>> = HashMap::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        if op == "quit" {
            emit(&out, &json!({"id": id, "ok": true}));
            break;
        }
        if op == "preview" {
            let _ = previews.send((id, request));
            continue;
        }
        let reply = handle(op, &request, &mut searcher, &encoder, &mut cache);
        let mut reply = reply.unwrap_or_else(|error| json!({"ok": false, "error": format!("{error:#}")}));
        reply["id"] = id;
        emit(&out, &reply);
    }
    // The model goes before the process does (llama.cpp aborts if its Metal device is freed under a model): a front end
    // that closes while the model loads waits for the load to end, then the model is freed with the last reference.
    let _ = loader.join();
    Ok(())
}

fn handle(
    op: &str,
    request: &Value,
    searcher: &mut Searcher,
    encoder: &Mutex<Encoder>,
    cache: &mut HashMap<String, Vec<f32>>,
) -> Result<Value> {
    match op {
        "search" | "recent" => {
            let search: Request = serde_json::from_value(request.clone())?;
            if op == "recent" || search.q.trim().is_empty() {
                return Ok(serde_json::to_value(searcher.recent(&search)?)?);
            }
            let vector = query_vector(&search.prompt(), encoder, cache);
            Ok(serde_json::to_value(searcher.search(&search, vector.as_deref())?)?)
        }
        "text" => {
            // A segment's whole text, or all of a file's (a screenshot's recognized text, say), for a preview.
            let text = match (
                request.get("segment_id").and_then(Value::as_i64),
                request.get("path").and_then(Value::as_str),
            ) {
                (Some(id), _) => searcher.segment_text(id)?,
                (None, Some(path)) => searcher.file_text(path)?,
                _ => return Err(anyhow!("which segment or file?")),
            };
            Ok(json!({"text": text}))
        }
        "status" => {
            searcher.refresh()?;
            let state = match &*encoder.lock().unwrap() {
                Encoder::Loading => "loading".to_string(),
                Encoder::Ready(_) => "ready".to_string(),
                Encoder::Unavailable(error) => format!("unavailable: {error}"),
            };
            Ok(json!({"vectors": searcher.vector_count(), "encoder": state, "stats": searcher.store().stats()?}))
        }
        _ => Err(anyhow!("unknown op {op:?}")),
    }
}

/// A preview's picture: a thumbnail, a PDF page, a video frame or a waveform.
fn preview(request: &Value) -> Result<Value> {
    let path = request.get("path").and_then(Value::as_str).ok_or_else(|| anyhow!("which file?"))?;
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("image");
    let loc = request.get("loc").and_then(Value::as_f64);
    let loc_end = request.get("loc_end").and_then(Value::as_f64);
    let size = request.get("size").and_then(Value::as_u64).unwrap_or(640) as u32;
    let image = preview::render(Path::new(path), kind, loc, loc_end, size, &config::previews_dir())?;
    Ok(json!({"image": image.display().to_string()}))
}

/// The query's vector (from its prompt), or None while the encoder loads (words-only results then).
fn query_vector(prompt: &str, encoder: &Mutex<Encoder>, cache: &mut HashMap<String, Vec<f32>>) -> Option<Vec<f32>> {
    let key = prompt.to_string();
    if let Some(vector) = cache.get(&key) {
        return Some(vector.clone());
    }
    let mut encoder = encoder.lock().unwrap();
    let Encoder::Ready(embedder) = &mut *encoder else { return None };
    let vector = embedder.embed_text(&key).ok()?;
    if cache.len() > 256 {
        cache.clear();
    }
    cache.insert(key, vector.clone());
    Some(vector)
}
