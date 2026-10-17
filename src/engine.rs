//! The engine: one resident process that answers every search (`sift-it engine`, a LaunchAgent of its own beside the
//! indexer's). A crash in a reader can't take search down, and search doesn't wait for a charger. Clients reach it
//! through a Unix socket in the data folder: `sift-it serve` relays a front end's stdio to it, and `sift-it search` and
//! the finder talk to it directly (client.rs). Results carry a path, kind and place; opening them, and previews, are the
//! front end's.
//!
//! JSON lines: one request per line, one reply per line carrying the request's `id`, and events (lines without an
//! `id`) to every client.
//!
//!   {"id":1,"op":"search","q":"…","mode":"meaning","kinds":["document"],"limit":40,"prefix_last":true}
//!       → {"id":1,"hits":[…],"meaning":true,"ms":3.1}
//!       `mode` is words or meaning (config.json's `search.mode` when left out). While the model and the vectors load,
//!       a search by meaning is answered by words ("meaning":false), unless it says "wait":true.
//!   {"id":2,"op":"recent","limit":40}                    → {"id":2,"hits":[…]}: recently changed files
//!   {"id":3,"op":"text","segment_id":12} or {"op":"text","path":"…"} → {"text":…}: a segment's whole text, or a
//!       file's (its text segments in order)
//!   {"id":4,"op":"status"}                               → {"meaning":"ready","vectors":n,"scan":"exact-f32",
//!       "resident":bytes,"stats":{…}}: how many vectors are mapped, in which form, and how much of them is in memory
//!   {"id":5,"op":"unload"}                               → {"ok":true}: the model and the vectors go
//!   {"id":6,"op":"stop"}                                 → {"ok":true}, and the engine exits
//!
//!   events: {"event":"ready","version":…,"build":…,"meaning":…}: on connecting
//!           {"event":"meaning","state":"loading"|"ready"|"unavailable"|"unloaded","error":…}
//!           {"event":"config","error":…}: config.json can't be read (the ranking stays as it was)
//!
//! Nothing loads before it's needed: a search by words uses SQLite's full-text index alone. The first search by meaning
//! loads the text model and maps the vectors (vectors.rs), off the searching threads. What's loaded stays until `unload`
//! or the engine stopping. There are no idle timers.

use crate::config::{self, Config, Mode};
use crate::embedder::{Embedder, ModelFiles};
use crate::search::{Request, Searcher, Vectors};
use crate::store::Store;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::SystemTime;

pub fn socket_path() -> PathBuf {
    config::home().join("engine.sock")
}

/// This build: the version, and the binary it runs from (its path, size and time), so two builds of one version
/// differ too. A client of another build stops the engine and starts its own (client.rs). Taken once, when first
/// asked: the binary on disk can be replaced while this process runs.
pub fn build() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(binary_build)
}

fn binary_build() -> String {
    use sha2::{Digest, Sha256};
    let binary = std::env::current_exe().and_then(|path| path.canonicalize()).unwrap_or_default();
    let stamp = std::fs::metadata(&binary)
        .map(|metadata| {
            let time = metadata.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok());
            format!("{}:{}", metadata.len(), time.map_or(0, |t| t.as_nanos()))
        })
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{}\n{stamp}", binary.display()));
    let hash: String = digest[..6].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}+{hash}", env!("CARGO_PKG_VERSION"))
}

/// Whether search by meaning can be had: its model and vectors, loaded on first need.
enum Meaning {
    Unloaded,
    Loading,
    Ready(Embedder),
    Unavailable(String),
}

impl Meaning {
    fn name(&self) -> &'static str {
        match self {
            Meaning::Unloaded => "unloaded",
            Meaning::Loading => "loading",
            Meaning::Ready(_) => "ready",
            Meaning::Unavailable(_) => "unavailable",
        }
    }
}

struct MeaningState {
    meaning: Meaning,
    /// Counts unloads, so a load that an unload overtook isn't installed.
    generation: u64,
}

/// What the engine holds, and the answers to requests: the server below shares it between clients, and `sift-it eval`
/// uses it in its own process.
pub struct Core {
    index: PathBuf,
    models: PathBuf,
    searcher: Mutex<Option<Searcher>>,
    meaning: Mutex<MeaningState>,
    /// Signalled when a load ends.
    loaded: Condvar,
    /// Query vectors by prompt: typing back and forth embeds each prefix once.
    cache: Mutex<HashMap<String, Vec<f32>>>,
    config: Mutex<(Option<SystemTime>, Config)>,
    /// Events go to every client (a no-op in a process of its own).
    events: Box<dyn Fn(&Value) + Send + Sync>,
}

impl Core {
    pub fn new(index: &Path, models: &Path, events: Box<dyn Fn(&Value) + Send + Sync>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            index: index.to_path_buf(),
            models: models.to_path_buf(),
            searcher: Mutex::new(None),
            meaning: Mutex::new(MeaningState { meaning: Meaning::Unloaded, generation: 0 }),
            loaded: Condvar::new(),
            cache: Mutex::new(HashMap::new()),
            config: Mutex::new((config_modified(), Config::load()?)),
            events,
        }))
    }

    pub fn meaning_state(&self) -> &'static str {
        self.meaning.lock().unwrap().meaning.name()
    }

    /// Answers a request (anything but `stop`, which the server handles).
    pub fn handle(self: &Arc<Self>, op: &str, request: &Value) -> Result<Value> {
        match op {
            "search" | "recent" => {
                let search: Request = serde_json::from_value(request.clone())?;
                let wait = request.get("wait").and_then(Value::as_bool).unwrap_or(false);
                let response = if op == "recent" || search.q.trim().is_empty() {
                    self.with_searcher(|searcher| searcher.recent(&search))?
                } else {
                    self.search(&search, wait)?
                };
                Ok(serde_json::to_value(response)?)
            }
            "text" => {
                let text = match (
                    request.get("segment_id").and_then(Value::as_i64),
                    request.get("path").and_then(Value::as_str),
                ) {
                    (Some(id), _) => self.with_searcher(|searcher| searcher.segment_text(id))?,
                    (None, Some(path)) => self.with_searcher(|searcher| searcher.file_text(path))?,
                    _ => return Err(anyhow!("which segment or file?")),
                };
                Ok(json!({"text": text}))
            }
            "status" => {
                let (vectors, resident, stats) = self.with_searcher(|searcher| {
                    searcher.refresh()?;
                    Ok((searcher.vector_count(), searcher.vectors_resident(), searcher.store().stats()?))
                })?;
                let scan = self.config.lock().unwrap().1.search.vectors;
                let meaning = self.meaning.lock().unwrap();
                let error = match &meaning.meaning {
                    Meaning::Unavailable(error) => Some(error.clone()),
                    _ => None,
                };
                Ok(json!({
                    "meaning": meaning.meaning.name(),
                    "error": error,
                    "vectors": vectors,
                    "scan": scan,
                    "resident": resident,
                    "stats": stats,
                }))
            }
            "unload" => {
                self.unload();
                Ok(json!({"ok": true}))
            }
            "preview" => Err(anyhow!("previews are made by the front end (sift-it serve makes them)")),
            _ => Err(anyhow!("unknown op {op:?}")),
        }
    }

    /// A search, by meaning when it asks for it (or config.json does) and the model is there.
    pub fn search(self: &Arc<Self>, request: &Request, wait: bool) -> Result<crate::search::Response> {
        let mode = request.mode.unwrap_or(self.config.lock().unwrap().1.search.mode);
        // The searcher first: an index that isn't there yet is an error, with nothing loaded for it.
        self.with_searcher(|_| Ok(()))?;
        let vector = match mode {
            Mode::Meaning => self.query_vector(&request.prompt(), wait),
            Mode::Words => None,
        };
        self.with_searcher(|searcher| searcher.search(request, vector.as_deref()))
    }

    /// The searcher, opened on first need, ranking by config.json's values as they are now, and scanning the form of
    /// the vectors it says.
    fn with_searcher<T>(&self, work: impl FnOnce(&mut Searcher) -> Result<T>) -> Result<T> {
        let changed = self.reload_config();
        let config = || self.config.lock().unwrap().1.clone();
        let mut searcher = self.searcher.lock().unwrap();
        match searcher.as_mut() {
            Some(searcher) if changed => {
                let config = config();
                searcher.set_ranking(config.ranking);
                searcher.set_scan(config.search.vectors)?;
            }
            Some(_) => {}
            None => *searcher = Some(Searcher::open(&self.index, config().ranking)?),
        }
        work(searcher.as_mut().expect("just opened"))
    }

    /// Reads config.json again when it changed. True when it did, and could be read.
    fn reload_config(&self) -> bool {
        let mut current = self.config.lock().unwrap();
        let modified = config_modified();
        if modified == current.0 {
            return false;
        }
        current.0 = modified;
        match Config::load() {
            Ok(config) => {
                current.1 = config;
                true
            }
            Err(error) => {
                (self.events)(&json!({"event": "config", "error": format!("{error:#}")}));
                false
            }
        }
    }

    /// The query's vector, or None while the model loads (unless `wait`) or when it can't.
    fn query_vector(self: &Arc<Self>, prompt: &str, wait: bool) -> Option<Vec<f32>> {
        if let Some(vector) = self.cache.lock().unwrap().get(prompt) {
            return Some(vector.clone());
        }
        self.start_loading();
        let mut state = self.meaning.lock().unwrap();
        while wait && matches!(state.meaning, Meaning::Loading) {
            state = self.loaded.wait(state).unwrap();
        }
        let Meaning::Ready(embedder) = &mut state.meaning else { return None };
        let vector = embedder.embed_text(prompt).ok()?;
        drop(state);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(prompt.to_string(), vector.clone());
        Some(vector)
    }

    /// Starts loading the model and the vectors, unless they're loaded or loading.
    fn start_loading(self: &Arc<Self>) {
        let generation = {
            let mut state = self.meaning.lock().unwrap();
            if !matches!(state.meaning, Meaning::Unloaded) {
                return;
            }
            state.meaning = Meaning::Loading;
            state.generation
        };
        (self.events)(&json!({"event": "meaning", "state": "loading"}));
        let core = self.clone();
        std::thread::spawn(move || core.load(generation));
    }

    fn load(&self, generation: u64) {
        let scan = || self.config.lock().unwrap().1.search.vectors;
        let loaded = (|| -> Result<(Embedder, Vectors)> {
            let embedder = Embedder::load(&ModelFiles::in_folder(&self.models)?, true)?;
            let vectors = Vectors::load(&Store::open_readonly(&self.index)?, scan())?;
            Ok((embedder, vectors))
        })();
        let mut state = self.meaning.lock().unwrap();
        if state.generation != generation {
            // Unloaded meanwhile: what was loaded goes with this thread.
            return;
        }
        let event = match loaded {
            Ok((embedder, vectors)) => {
                // In the form config.json says now: it may have changed meanwhile.
                let installed = self.with_searcher(|searcher| {
                    searcher.set_vectors(vectors)?;
                    searcher.set_scan(scan())
                });
                match installed {
                    Ok(()) => {
                        state.meaning = Meaning::Ready(embedder);
                        json!({"event": "meaning", "state": "ready"})
                    }
                    Err(error) => {
                        state.meaning = Meaning::Unavailable(format!("{error:#}"));
                        json!({"event": "meaning", "state": "unavailable", "error": format!("{error:#}")})
                    }
                }
            }
            Err(error) => {
                state.meaning = Meaning::Unavailable(format!("{error:#}"));
                json!({"event": "meaning", "state": "unavailable", "error": format!("{error:#}")})
            }
        };
        drop(state);
        self.loaded.notify_all();
        (self.events)(&event);
    }

    /// Lets the model and the vectors go (a model that couldn't load is tried again on the next search by meaning).
    /// A load under way finishes first: llama.cpp can't be stopped halfway.
    pub fn unload(&self) {
        let mut state = self.meaning.lock().unwrap();
        while matches!(state.meaning, Meaning::Loading) {
            state = self.loaded.wait(state).unwrap();
        }
        state.generation += 1;
        state.meaning = Meaning::Unloaded;
        drop(state);
        if let Some(searcher) = self.searcher.lock().unwrap().as_mut() {
            searcher.unload();
        }
        self.cache.lock().unwrap().clear();
        (self.events)(&json!({"event": "meaning", "state": "unloaded"}));
    }
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(config::config_path()).and_then(|metadata| metadata.modified()).ok()
}

type Writer = Mutex<UnixStream>;

/// Writes a line in one go, so a reader never waits on half of one.
fn send(writer: &Writer, value: &Value) -> bool {
    writer.lock().unwrap().write_all(format!("{value}\n").as_bytes()).is_ok()
}

/// `sift-it engine`: answers clients on the socket until a `stop` request. Exits at once when another engine already
/// answers there.
pub fn run(index: &Path, models: &Path) -> Result<()> {
    build();
    let socket = socket_path();
    if UnixStream::connect(&socket).is_ok() {
        eprintln!("an engine already answers at {}", socket.display());
        return Ok(());
    }
    std::fs::create_dir_all(config::home())?;
    // A socket file left by an engine that didn't stop cleanly.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("can't listen at {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("engine {} answering at {}", build(), socket.display());

    let clients: Arc<Mutex<Vec<Weak<Writer>>>> = Arc::new(Mutex::new(Vec::new()));
    let events = {
        let clients = clients.clone();
        Box::new(move |event: &Value| {
            let mut clients = clients.lock().unwrap();
            clients.retain(|client| client.upgrade().is_some_and(|writer| send(&writer, event)));
        })
    };
    let core = Core::new(index, models, events)?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let core = core.clone();
        let clients = clients.clone();
        std::thread::spawn(move || {
            if let Err(error) = serve_client(&core, stream, &clients) {
                eprintln!("a client: {error:#}");
            }
        });
    }
    Ok(())
}

fn serve_client(core: &Arc<Core>, stream: UnixStream, clients: &Mutex<Vec<Weak<Writer>>>) -> Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    {
        // Ready goes first, and events only after it (they wait for the list): the client checks the build on it, and
        // takes the state of meaning from it, then follows the events.
        let mut clients = clients.lock().unwrap();
        let ready = json!({
            "event": "ready",
            "version": env!("CARGO_PKG_VERSION"),
            "build": build(),
            "index": core.index.display().to_string(),
            "meaning": core.meaning_state(),
        });
        if !send(&writer, &ready) {
            return Ok(());
        }
        clients.push(Arc::downgrade(&writer));
    }
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        if op == "stop" {
            stop(core, &writer, id);
        }
        let mut reply =
            core.handle(op, &request).unwrap_or_else(|error| json!({"ok": false, "error": format!("{error:#}")}));
        reply["id"] = id;
        if !send(&writer, &reply) {
            break;
        }
    }
    Ok(())
}

/// Stops the engine: the model goes before the process does (llama.cpp aborts when its Metal device is freed under a
/// model), and the socket with it.
fn stop(core: &Arc<Core>, writer: &Writer, id: Value) -> ! {
    core.unload();
    let _ = std::fs::remove_file(socket_path());
    send(writer, &json!({"id": id, "ok": true}));
    eprintln!("engine stopped");
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::crawl::{Found, Kind};
    use crate::store::{NewSegment, Segment};

    #[test]
    fn meaning_loads_on_first_need_and_words_never_wait_for_it() {
        let folder = std::env::temp_dir().join(format!("sift-it-engine-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let index = folder.join("index.sqlite");
        {
            let mut store = Store::open(&index).unwrap();
            let settings = Settings::BASE;
            let found =
                Found { path: "/a/pancakes.txt".into(), kind: Kind::Plaintext, size: 1, modified: 1.0, settings };
            store.queue(&found, None, 0.0).unwrap();
            let row = store.pending(1).unwrap().remove(0);
            let text = Some("pancakes with syrup".to_string());
            let passage = NewSegment { segment: Segment::Passage, loc: Some(1.0), loc_end: None, text, vector: None };
            store.finish_file(&row, &[passage], &json!({}), false).unwrap();
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let core = Core::new(
            &index,
            &folder.join("no-models"),
            Box::new(move |event| seen.lock().unwrap().push(event.clone())),
        )
        .unwrap();
        let request =
            |mode| Request { q: "pancakes".into(), mode: Some(mode), kinds: vec![], limit: 5, prefix_last: false };

        // By words: nothing loads.
        let response = core.search(&request(Mode::Words), true).unwrap();
        assert_eq!(response.hits[0].name, "pancakes.txt");
        assert_eq!(core.meaning_state(), "unloaded");

        // By meaning: the model loads (here it can't), and the search is answered by words meanwhile.
        let response = core.search(&request(Mode::Meaning), true).unwrap();
        assert!(!response.meaning);
        assert_eq!(response.hits[0].name, "pancakes.txt");
        assert_eq!(core.meaning_state(), "unavailable");
        let states: Vec<Value> = events.lock().unwrap().iter().map(|event| event["state"].clone()).collect();
        assert_eq!(states, [json!("loading"), json!("unavailable")]);

        // Unloading lets it be tried again.
        core.unload();
        assert_eq!(core.meaning_state(), "unloaded");
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
