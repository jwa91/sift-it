//! `sift-it serve`: a front end's way to the engine (engine.rs), for the Raycast extension and the like. It relays JSON
//! lines between its stdin and stdout and the engine's socket, starting the engine when it isn't running (or is
//! another build), and makes previews itself: drawing a video frame or a PDF page is the work of the code that reads
//! files, which stays out of the engine.
//!
//! Requests, replies and events are the engine's (see engine.rs), and these:
//!
//!   {"id":3,"op":"preview","path":"…","kind":"pdf","loc":3,"size":512} → {"id":3,"image":"/…/preview.jpg"}
//!   {"id":5,"op":"preview","path":"…","kind":"waveform","loc":60,"loc_end":90,"size":720} → a recording's
//!       waveform (PNG) with the stretch from loc to loc_end seconds highlighted
//!   {"id":7,"op":"quit"}                                 → {"id":7,"ok":true} once earlier requests are answered,
//!       and it ends (the engine goes on)
//!
//! Previews are made on a thread of their own, so a search never waits behind one: their replies can come after
//! replies to later requests. When the engine stops (it crashed, or a client of another build replaced it), the
//! requests waiting on it fail, it's started again, and a new `ready` follows. It ends when its stdin closes.

use crate::client::Client;
use crate::config;
use crate::preview;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

fn emit(out: &Mutex<std::io::Stdout>, line: &str) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The way to the engine: its socket while connected, and the ids of the requests waiting on it. And how many previews
/// are being made.
#[derive(Default)]
struct Link {
    engine: Option<UnixStream>,
    waiting: HashSet<String>,
    previews: usize,
}

pub fn serve() -> Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    std::thread::spawn(|| preview::prune(&config::previews_dir()));
    let link = Arc::new((Mutex::new(Link::default()), Condvar::new()));
    {
        let out = out.clone();
        let link = link.clone();
        std::thread::spawn(move || relay_replies(&out, &link));
    }

    // A video frame, a PDF page or a long recording's waveform can take a while (seconds, the first time): previews
    // are made one after another here, beside the searches.
    let (previews, requests) = std::sync::mpsc::channel::<(Value, Value)>();
    {
        let out = out.clone();
        let link = link.clone();
        std::thread::spawn(move || {
            for (id, request) in requests {
                let mut reply =
                    preview(&request).unwrap_or_else(|error| json!({"ok": false, "error": format!("{error:#}")}));
                reply["id"] = id;
                emit(&out, &reply.to_string());
                link.0.lock().unwrap().previews -= 1;
                link.1.notify_all();
            }
        });
    }

    let (lock, changed) = &*link;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        match request.get("op").and_then(Value::as_str).unwrap_or("") {
            "quit" => {
                // Earlier requests are answered first (a reply, or a failure when the engine stops).
                let busy = |link: &mut Link| !link.waiting.is_empty() || link.previews > 0;
                drop(changed.wait_while(lock.lock().unwrap(), busy).unwrap());
                emit(&out, &json!({"id": id, "ok": true}).to_string());
                break;
            }
            "preview" => {
                lock.lock().unwrap().previews += 1;
                let _ = previews.send((id, request));
            }
            _ => {
                let mut link = changed.wait_while(lock.lock().unwrap(), |link| link.engine.is_none()).unwrap();
                link.waiting.insert(id.to_string());
                let sent = link.engine.as_mut().is_some_and(|engine| writeln!(engine, "{line}").is_ok());
                if !sent {
                    link.waiting.remove(&id.to_string());
                    emit(&out, &json!({"id": id, "ok": false, "error": "the engine stopped"}).to_string());
                }
            }
        }
    }
    Ok(())
}

/// Connects to the engine and passes on what it says, again after it stops. A failure to start it ends the process.
fn relay_replies(out: &Mutex<std::io::Stdout>, link: &(Mutex<Link>, Condvar)) {
    let (lock, changed) = link;
    loop {
        let client = match Client::connect() {
            Ok(client) => client,
            Err(error) => {
                eprintln!("sift-it: {error:#}");
                std::process::exit(1);
            }
        };
        let (reader, writer, ready) = client.into_parts();
        lock.lock().unwrap().engine = Some(writer);
        changed.notify_all();
        emit(out, &ready.to_string());
        for line in reader.lines() {
            let Ok(line) = line else { break };
            emit(out, &line);
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && let Some(id) = message.get("id")
            {
                lock.lock().unwrap().waiting.remove(&id.to_string());
                changed.notify_all();
            }
        }
        // The engine stopped: what waited on it fails, and a new one is started.
        let mut link = lock.lock().unwrap();
        link.engine = None;
        for id in link.waiting.drain() {
            let id: Value = serde_json::from_str(&id).unwrap_or(Value::Null);
            emit(out, &json!({"id": id, "ok": false, "error": "the engine stopped"}).to_string());
        }
        changed.notify_all();
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
