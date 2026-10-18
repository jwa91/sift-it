//! `sift-it serve`: a front end's way to the engine (services/engine.rs), for the Raycast extension and the like. It
//! relays JSON lines between its stdin and stdout and the engine's socket, starting the engine when it isn't running
//! (or is another build), and makes previews itself: drawing a video frame or a PDF page is the work of the code that
//! reads files, which stays out of the engine.
//!
//! Requests, replies and events are the protocol's (crates/protocol, docs/protocol.md). Two of them are this relay's
//! own: `preview` (a thumbnail, a PDF page, a video frame or a waveform) and `quit`.
//!
//! Previews are made on a thread of their own, so a search never waits behind one: their replies can come after
//! replies to later requests. When the engine stops (it crashed, or a client of another build replaced it), the
//! requests waiting on it fail, it's started again, and a new `ready` follows. It ends when its stdin closes.

use crate::clients::Client;
use crate::clients::preview;
use crate::config;
use anyhow::Result;
use protocol::{Done, Failure, Op, Picture, Preview, Request};
use serde::Serialize;
use serde_json::Value;
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

/// A reply to the request with `id`.
fn answer(out: &Mutex<std::io::Stdout>, id: &Value, reply: impl Serialize) {
    let mut reply = serde_json::to_value(reply).unwrap_or_default();
    reply["id"] = id.clone();
    emit(out, &reply.to_string());
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
    let (previews, requests) = std::sync::mpsc::channel::<(Value, Preview)>();
    {
        let out = out.clone();
        let link = link.clone();
        std::thread::spawn(move || {
            for (id, request) in requests {
                match preview(&request) {
                    Ok(picture) => answer(&out, &id, picture),
                    Err(error) => answer(&out, &id, Failure::new(format!("{error:#}"))),
                }
                link.0.lock().unwrap().previews -= 1;
                link.1.notify_all();
            }
        });
    }

    let (lock, changed) = &*link;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        // What this relay doesn't answer itself goes to the engine as it came, which says when it doesn't read.
        match serde_json::from_value::<Request>(message).map(|request| request.op) {
            Ok(Op::Quit) => {
                // Earlier requests are answered first (a reply, or a failure when the engine stops).
                let busy = |link: &mut Link| !link.waiting.is_empty() || link.previews > 0;
                drop(changed.wait_while(lock.lock().unwrap(), busy).unwrap());
                answer(&out, &id, Done::OK);
                break;
            }
            Ok(Op::Preview(preview)) => {
                lock.lock().unwrap().previews += 1;
                let _ = previews.send((id, preview));
            }
            _ => {
                let mut link = changed.wait_while(lock.lock().unwrap(), |link| link.engine.is_none()).unwrap();
                link.waiting.insert(id.to_string());
                let sent = link.engine.as_mut().is_some_and(|engine| writeln!(engine, "{line}").is_ok());
                if !sent {
                    link.waiting.remove(&id.to_string());
                    answer(&out, &id, Failure::new("the engine stopped"));
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
        emit(out, &ready);
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
            answer(out, &id, Failure::new("the engine stopped"));
        }
        changed.notify_all();
    }
}

/// A preview's picture: a thumbnail, a PDF page, a video frame or a waveform.
fn preview(request: &Preview) -> Result<Picture> {
    let Preview { path, kind, loc, loc_end, size } = request;
    let image = preview::render(Path::new(path), kind, *loc, *loc_end, *size, &config::previews_dir())?;
    Ok(Picture { image: image.display().to_string() })
}
