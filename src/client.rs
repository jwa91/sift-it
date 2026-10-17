//! Talking to the engine (engine.rs) over its socket: `sift-it serve` relays through it, and `sift-it search`, the
//! finder and `sift-it status` ask it directly.
//!
//! Connecting starts the engine when it isn't running: through launchd when its LaunchAgent runs this binary for this
//! data folder, else as a process of its own. An engine of another build (an upgrade, or a new build while developing)
//! is stopped first, and this build's started in its place.

use crate::engine::{self, build, socket_path};
use crate::search::{Request, Response};
use crate::service;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long an engine has to start answering.
const START: Duration = Duration::from_secs(10);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// The engine's `ready` event: its build, and the state of search by meaning when this client connected.
    pub ready: Value,
    /// Events that came while waiting for a reply, oldest first.
    events: VecDeque<Value>,
    /// What's read of a line that hasn't ended yet (a wait for events can end in the middle of one).
    partial: String,
}

impl Client {
    /// Connects to the engine, starting it (or this build of it) when needed.
    pub fn connect() -> Result<Self> {
        let ours = build();
        let mut started = false;
        loop {
            match Self::open(if started { START } else { Duration::ZERO }) {
                // Once this build was started, whatever answers is kept: another client may have started its own
                // meanwhile, and taking turns stopping each other would never end.
                Ok(client) if client.ready["build"] == ours || started => return Ok(client),
                Ok(mut client) => {
                    // Another build: it goes, and this one takes its place.
                    let _ = client.send("stop", json!({}));
                    client.wait_closed();
                }
                Err(error) if started => {
                    let log = crate::config::home().join("engine.log");
                    return Err(error).context(format!("the engine didn't start (see {})", log.display()));
                }
                Err(_) => {}
            }
            start()?;
            started = true;
        }
    }

    /// The engine, when it's running (it isn't started).
    pub fn running() -> Option<Self> {
        let stream = engine::connect().ok()?;
        Self::from_stream(stream).ok()
    }

    /// Connects, waiting up to `patience` for the socket to answer.
    fn open(patience: Duration) -> Result<Self> {
        let deadline = Instant::now() + patience;
        loop {
            match engine::connect() {
                Ok(stream) => return Self::from_stream(stream),
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context(format!("can't reach the engine at {}", socket_path().display()));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    fn from_stream(stream: UnixStream) -> Result<Self> {
        let writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        reader.get_ref().set_read_timeout(Some(START))?;
        let mut line = String::new();
        reader.read_line(&mut line)?;
        reader.get_ref().set_read_timeout(None)?;
        let ready: Value = serde_json::from_str(&line).context("the engine didn't say it was ready")?;
        if ready["event"] != "ready" {
            bail!("the engine said {line}");
        }
        Ok(Self { reader, writer, next_id: 1, ready, events: VecDeque::new(), partial: String::new() })
    }

    /// The streams, for relaying (serve.rs).
    pub fn into_parts(self) -> (BufReader<UnixStream>, UnixStream, Value) {
        (self.reader, self.writer, self.ready)
    }

    fn send(&mut self, op: &str, mut params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        params["op"] = json!(op);
        params["id"] = json!(id);
        writeln!(self.writer, "{params}")?;
        Ok(id)
    }

    /// Sends a request and waits for its reply. Events that come meanwhile wait in `events`.
    pub fn request(&mut self, op: &str, params: Value) -> Result<Value> {
        let id = self.send(op, params)?;
        loop {
            let message = self.read()?.ok_or_else(|| anyhow!("the engine stopped"))?;
            if message["id"] == id {
                if message["ok"] == false {
                    bail!("{}", message["error"].as_str().unwrap_or("the engine failed"));
                }
                return Ok(message);
            }
            if message.get("event").is_some() {
                self.events.push_back(message);
            }
        }
    }

    /// The next message, None when the engine closed the connection.
    fn read(&mut self) -> Result<Option<Value>> {
        if self.reader.read_line(&mut self.partial)? == 0 {
            return Ok(None);
        }
        let line = std::mem::take(&mut self.partial);
        Ok(Some(serde_json::from_str(&line)?))
    }

    /// The next event: one that came with a reply, or one that comes within `timeout`.
    pub fn event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        self.reader.get_ref().set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        // A wait that ends partway through a line keeps that part for the next read.
        let message = self.read();
        self.reader.get_ref().set_read_timeout(None)?;
        match message {
            Ok(Some(message)) if message.get("event").is_some() => Ok(Some(message)),
            Ok(Some(_)) => Ok(None),
            Ok(None) => Err(anyhow!("the engine stopped")),
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
                }) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// A search (`wait`: by meaning even if the model has to load first). An empty query lists recent files.
    pub fn search(&mut self, request: &Request, wait: bool) -> Result<Response> {
        let mut params = serde_json::to_value(request)?;
        params["wait"] = json!(wait);
        let op = if request.q.trim().is_empty() { "recent" } else { "search" };
        Ok(serde_json::from_value(self.request(op, params)?)?)
    }

    pub fn segment_text(&mut self, segment_id: i64) -> Result<Option<String>> {
        Ok(serde_json::from_value(self.request("text", json!({"segment_id": segment_id}))?["text"].take())?)
    }

    pub fn file_text(&mut self, path: &str) -> Result<Option<String>> {
        Ok(serde_json::from_value(self.request("text", json!({"path": path}))?["text"].take())?)
    }

    /// Waits until the engine has closed the connection (it stopped), for at most a few seconds.
    fn wait_closed(&mut self) {
        let _ = self.reader.get_ref().set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        while self.reader.read_line(&mut line).is_ok_and(|read| read > 0) {
            line.clear();
        }
    }
}

/// Starts this build's engine: through launchd when its agent runs this binary for this data folder, else as a
/// process of its own that outlives this one.
fn start() -> Result<()> {
    if service::ENGINE.installed_as_this() && service::ENGINE.kickstart().is_ok() {
        return Ok(());
    }
    let binary = std::env::current_exe()?;
    std::fs::create_dir_all(crate::config::home())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::config::home().join("engine.log"))?;
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(binary);
    command.arg("engine").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // A session of its own: neither a Ctrl-C in the terminal that started it nor that terminal closing reaches it.
    // Safety: setsid is async-signal-safe, and the child calls nothing else before exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("can't start the engine")?;
    Ok(())
}
