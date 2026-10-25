//! The engine: one resident process that answers every search (`sift-it engine`, a LaunchAgent of its own beside the
//! indexer's). A crash in a reader can't take search down, and search doesn't wait for a charger. Clients reach it
//! through a Unix socket in the data folder: `sift-it serve` relays a front end's stdio to it, and `sift-it search` and
//! the finder talk to it directly (clients/client.rs). Results carry a path, kind and place; opening them, and
//! previews, are the front end's.
//!
//! What clients and the engine say to each other is the protocol (crates/protocol, docs/protocol.md): JSON lines, one
//! request per line and one reply per line carrying the request's `id`, and events (lines without an `id`) to every
//! client. The engine is a client's one way in: it searches, and it passes on the indexer's progress (from status.json,
//! as the indexer writes it) and changes the settings and the folders (config.json, which the indexer follows).
//!
//! Nothing loads before it's needed: a search by words uses SQLite's full-text index alone. The first search by meaning
//! loads the text model and maps the vectors (index::Vectors), off the searching threads. What's loaded stays until
//! `unload` or the engine stopping. There are no idle timers.

use crate::config::{self, Config};
use crate::index::{Reader, Vectors};
use crate::models::Model;
use crate::query::{Query, Searcher};
use anyhow::{Context, Result, bail};
use embedder::{Embedder, Inputs};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{
    Done, Event, Failure, IndexerStatus, MeaningState, Mode, Op, Request, Results, Search, Settings, Status, Text,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::SystemTime;

/// The socket's file. It's in the data folder, so there's one engine per data folder.
pub fn socket_path() -> PathBuf {
    config::home().join("engine.sock")
}

/// Connects to the engine's socket.
pub fn connect() -> io::Result<UnixStream> {
    UnixStream::connect(socket_address(&config::home())?)
}

/// How long a socket's name can be: sockaddr_un's sun_path, its closing NUL included.
const SUN_PATH: usize = 104;

/// The name the socket is bound and reached by. A data folder can be too deep for a socket's name: then it's named
/// through a link to the data folder in this user's temporary folder, made when needed (and again after macOS clears
/// that folder). The socket stays in the data folder.
fn socket_address(home: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let socket = home.join("engine.sock");
    if socket.as_os_str().len() < SUN_PATH {
        return Ok(socket);
    }
    let home = std::path::absolute(home)?;
    let digest = Sha256::digest(home.as_os_str().as_bytes());
    let hash: String = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    let link = user_temp_dir()?.join(format!("sift-it-{hash}"));
    let made = match std::fs::read_link(&link) {
        Ok(target) if target == home => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::os::unix::fs::symlink(&home, &link),
        // Another data folder's link (their names are a hash), or something else in the way.
        _ => {
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&home, &link)
        }
    };
    match made {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            Err(io::Error::new(error.kind(), format!("can't link to the data folder from {}: {error}", link.display())))
        }
        // Made, or another client made it meanwhile.
        _ => Ok(link.join("engine.sock")),
    }
}

/// This user's temporary folder (/var/folders/…/T/), which only they can write to, whatever TMPDIR says.
fn user_temp_dir() -> io::Result<PathBuf> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // Safety: confstr writes at most buffer.len() bytes, a NUL included, and returns the length it needs.
    let needed = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buffer.as_mut_ptr().cast(), buffer.len()) };
    if needed == 0 || needed > buffer.len() {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(needed - 1);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

/// This build: the version, and the binary it runs from (its path, size and time), so two builds of one version differ
/// too. A client of another build stops the engine and starts its own (clients/client.rs). Taken once, when first
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
    fn state(&self) -> MeaningState {
        match self {
            Meaning::Unloaded => MeaningState::Unloaded,
            Meaning::Loading => MeaningState::Loading,
            Meaning::Ready(_) => MeaningState::Ready,
            Meaning::Unavailable(_) => MeaningState::Unavailable,
        }
    }

    /// Why search by meaning is unavailable, when it is.
    fn error(&self) -> Option<String> {
        match self {
            Meaning::Unavailable(error) => Some(error.clone()),
            _ => None,
        }
    }

    /// The event that says what it is now.
    fn event(&self) -> Event {
        Event::Meaning { state: self.state(), error: self.error() }
    }
}

struct Loaded {
    meaning: Meaning,
    /// Counts unloads, so a load that an unload overtook isn't installed.
    generation: u64,
}

/// What the engine holds, and the answers to requests: the server below shares it between clients, and `sift-it eval`
/// uses it in its own process.
pub struct Core {
    index: PathBuf,
    /// The text model, as the cache has it.
    model: &'static Model,
    searcher: Mutex<Option<Searcher>>,
    meaning: Mutex<Loaded>,
    /// Signalled when a load ends.
    loaded: Condvar,
    /// Query vectors by prompt: typing back and forth embeds each prefix once.
    cache: Mutex<HashMap<String, Vec<f32>>>,
    config: Mutex<(Option<SystemTime>, Config)>,
    /// Events go to every client (a no-op in a process of its own).
    events: Box<dyn Fn(&Event) + Send + Sync>,
}

impl Core {
    pub fn new(index: &Path, model: &'static Model, events: Box<dyn Fn(&Event) + Send + Sync>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            index: index.to_path_buf(),
            model,
            searcher: Mutex::new(None),
            meaning: Mutex::new(Loaded { meaning: Meaning::Unloaded, generation: 0 }),
            loaded: Condvar::new(),
            cache: Mutex::new(HashMap::new()),
            config: Mutex::new((config_modified(), Config::load()?)),
            events,
        }))
    }

    pub fn meaning_state(&self) -> MeaningState {
        self.meaning.lock().unwrap().meaning.state()
    }

    /// Answers a request (anything but `stop`, which the server handles) with its reply.
    pub fn handle(self: &Arc<Self>, op: &Op) -> Result<Value> {
        match op {
            Op::Search(search) if !search.q.trim().is_empty() => reply(self.search(search)?),
            Op::Search(search) | Op::Recent(search) => reply(self.with_searcher(|searcher| searcher.recent(search))?),
            Op::Text { segment_id: Some(id), .. } => {
                reply(Text { text: self.with_searcher(|searcher| searcher.index().segment_text(*id))? })
            }
            Op::Text { path: Some(path), .. } => {
                reply(Text { text: self.with_searcher(|searcher| searcher.index().file_text(path))? })
            }
            Op::Text { .. } => bail!("which segment or file?"),
            Op::Status => reply(self.status()?),
            Op::Config { key, value } => {
                let config = config::configure(key.as_deref(), value.as_deref())?;
                reply(Settings { config: config.get(key.as_deref())? })
            }
            Op::Folders { action, paths } => {
                let mut config = Config::load()?;
                if let Some(action) = action {
                    config.change_folders(*action, paths)?;
                    config.save()?;
                }
                reply(config.folders())
            }
            Op::Unload => {
                self.unload();
                reply(Done::OK)
            }
            Op::Stop => bail!("the server stops the engine"),
            Op::Preview(_) => bail!("previews are made by the front end (sift-it serve makes them)"),
            Op::Quit => bail!("quit ends sift-it serve, not the engine"),
        }
    }

    /// What the engine holds, and the indexer's progress.
    fn status(self: &Arc<Self>) -> Result<Status> {
        let (vectors, scan, resident, stats) = self.with_searcher(|searcher| {
            searcher.refresh()?;
            let stats = searcher.index().stats()?;
            Ok((searcher.vector_count(), searcher.plan().scan(), searcher.vectors_resident(), stats))
        })?;
        let meaning = self.meaning.lock().unwrap();
        Ok(Status {
            meaning: meaning.meaning.state(),
            error: meaning.meaning.error(),
            vectors,
            scan,
            resident,
            stats,
            indexer: indexer_status(),
        })
    }

    /// A search, by meaning when it asks for it (or config.json does) and the model is there.
    pub fn search(self: &Arc<Self>, request: &Search) -> Result<Results> {
        let mode = request.mode.unwrap_or(self.config.lock().unwrap().1.search.mode);
        // The searcher first: an index that isn't there yet is an error, with nothing loaded for it.
        self.with_searcher(|_| Ok(()))?;
        let vector = match mode {
            Mode::Meaning => self.query_vector(&Query::of(request).prompt(), request.wait),
            Mode::Words => None,
        };
        self.with_searcher(|searcher| searcher.search(request, vector.as_deref()))
    }

    /// `work` with the searcher and the request's query vector (once the model is loaded, None without one): for
    /// measuring search (`sift-it eval --compare`).
    pub fn with_query<T>(
        self: &Arc<Self>,
        request: &Search,
        work: impl FnOnce(&mut Searcher, Option<&[f32]>) -> Result<T>,
    ) -> Result<T> {
        self.with_searcher(|_| Ok(()))?;
        let vector = self.query_vector(&Query::of(request).prompt(), true);
        self.with_searcher(|searcher| work(searcher, vector.as_deref()))
    }

    /// The searcher, opened on first need, ranking and scanning the vectors as config.json says now.
    fn with_searcher<T>(&self, work: impl FnOnce(&mut Searcher) -> Result<T>) -> Result<T> {
        let changed = self.reload_config();
        let config = || self.config.lock().unwrap().1.clone();
        let mut searcher = self.searcher.lock().unwrap();
        match searcher.as_mut() {
            Some(searcher) if changed => {
                let config = config();
                searcher.set_ranking(config.ranking);
                searcher.set_search(config.search);
            }
            Some(_) => {}
            None => {
                let config = config();
                *searcher = Some(Searcher::open(&self.index, config.ranking, config.search)?)
            }
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
                (self.events)(&Event::Config { error: format!("{error:#}") });
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
        (self.events)(&Event::Meaning { state: MeaningState::Loading, error: None });
        let core = self.clone();
        std::thread::spawn(move || core.load(generation));
    }

    fn load(&self, generation: u64) {
        let loaded = (|| -> Result<(Embedder, Vectors)> {
            let embedder = Embedder::load(&self.model.require()?, Inputs::Queries)?;
            let vectors = Vectors::load(&Reader::open(&self.index)?)?;
            Ok((embedder, vectors))
        })();
        let mut state = self.meaning.lock().unwrap();
        if state.generation != generation {
            // Unloaded meanwhile: what was loaded goes with this thread.
            return;
        }
        let installed = loaded.and_then(|(embedder, vectors)| {
            self.with_searcher(|searcher| searcher.set_vectors(vectors))?;
            Ok(embedder)
        });
        state.meaning = match installed {
            Ok(embedder) => Meaning::Ready(embedder),
            Err(error) => Meaning::Unavailable(format!("{error:#}")),
        };
        let event = state.meaning.event();
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
        (self.events)(&Event::Meaning { state: MeaningState::Unloaded, error: None });
    }
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(config::config_path()).and_then(|metadata| metadata.modified()).ok()
}

type Writer = Mutex<UnixStream>;

/// A reply, as the JSON it goes out as.
fn reply(reply: impl Serialize) -> Result<Value> {
    Ok(serde_json::to_value(reply)?)
}

/// The indexer's progress as it last wrote it, if it ever ran for this data folder.
pub fn indexer_status() -> Option<IndexerStatus> {
    serde_json::from_slice(&std::fs::read(config::status_path()).ok()?).ok()
}

/// Writes a line in one go, so a reader never waits on half of one.
fn send(writer: &Writer, message: &impl Serialize) -> bool {
    let Ok(line) = serde_json::to_string(message) else { return false };
    writer.lock().unwrap().write_all(format!("{line}\n").as_bytes()).is_ok()
}

/// `sift-it engine`: answers clients on the socket until a `stop` request. Exits at once when another engine already
/// answers there.
pub fn run(index: &Path) -> Result<()> {
    build();
    let socket = socket_path();
    if connect().is_ok() {
        eprintln!("an engine already answers at {}", socket.display());
        return Ok(());
    }
    std::fs::create_dir_all(config::home())?;
    // A socket file left by an engine that didn't stop cleanly.
    let _ = std::fs::remove_file(&socket);
    let listener = socket_address(&config::home())
        .and_then(UnixListener::bind)
        .with_context(|| format!("can't listen at {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("engine {} answering at {}", build(), socket.display());

    let clients: Arc<Mutex<Vec<Weak<Writer>>>> = Arc::new(Mutex::new(Vec::new()));
    let broadcast: Arc<dyn Fn(&Event) + Send + Sync> = {
        let clients = clients.clone();
        Arc::new(move |event: &Event| {
            let mut clients = clients.lock().unwrap();
            clients.retain(|client| client.upgrade().is_some_and(|writer| send(&writer, event)));
        })
    };
    let _indexer = watch_indexer(broadcast.clone())?;
    let core = Core::new(index, &crate::models::TEXT, Box::new(move |event| broadcast(event)))?;
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

/// Passes the indexer's progress on to every client each time it writes it (status.json).
fn watch_indexer(broadcast: Arc<dyn Fn(&Event) + Send + Sync>) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let written = event
            .is_ok_and(|event| event.paths.iter().any(|path| path.file_name() == config::status_path().file_name()));
        if let Some(status) = indexer_status().filter(|_| written) {
            broadcast(&Event::Indexer(status));
        }
    })?;
    watcher.watch(&config::home(), RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

fn serve_client(core: &Arc<Core>, stream: UnixStream, clients: &Mutex<Vec<Weak<Writer>>>) -> Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    {
        // Ready goes first, and events only after it (they wait for the list): the client checks the build on it, and
        // takes the state of meaning from it, then follows the events.
        let mut clients = clients.lock().unwrap();
        let ready = Event::Ready {
            version: env!("CARGO_PKG_VERSION").into(),
            build: build().into(),
            index: core.index.display().to_string(),
            meaning: core.meaning_state(),
        };
        if !send(&writer, &ready) {
            return Ok(());
        }
        clients.push(Arc::downgrade(&writer));
    }
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        // The id first, so a request that doesn't read is answered as failed rather than not at all.
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let request = serde_json::from_value::<Request>(message).map_err(anyhow::Error::from);
        if let Ok(Request { op: Op::Stop, .. }) = request {
            stop(core, &writer, id);
        }
        let mut reply = request
            .and_then(|request| core.handle(&request.op))
            .unwrap_or_else(|error| serde_json::to_value(Failure::new(format!("{error:#}"))).unwrap_or_default());
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
    let mut done = serde_json::to_value(Done::OK).unwrap_or_default();
    done["id"] = id;
    send(writer, &done);
    eprintln!("engine stopped");
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Writer;
    use crate::index::testing::add;
    use protocol::Kind;

    #[test]
    fn a_deep_data_folder_is_reached_through_a_short_link() {
        let top = std::env::temp_dir().join(format!("sift-it-socket-{}", std::process::id()));
        let home = top.join("d".repeat(SUN_PATH));
        std::fs::create_dir_all(&home).unwrap();
        let address = socket_address(&home).unwrap();
        assert!(address.as_os_str().len() < SUN_PATH);
        let listener = UnixListener::bind(&address).unwrap();
        assert!(home.join("engine.sock").exists());
        UnixStream::connect(socket_address(&home).unwrap()).unwrap();

        // The link is made again after it's cleared, and the engine still answers through it.
        let link = address.parent().unwrap();
        std::fs::remove_file(link).unwrap();
        UnixStream::connect(socket_address(&home).unwrap()).unwrap();

        // A shallow one needs no link.
        assert_eq!(socket_address(Path::new("shallow")).unwrap(), Path::new("shallow/engine.sock"));
        drop(listener);
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn meaning_loads_on_first_need_and_words_never_wait_for_it() {
        let folder = std::env::temp_dir().join(format!("sift-it-engine-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let index = folder.join("index.sqlite");
        let passages = vec![("pancakes with syrup".to_string(), None)];
        add(&mut Writer::open(&index).unwrap(), "/a/pancakes.txt", Kind::Plaintext, passages);
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        // A model no cache has.
        static NONE: Model = Model {
            files: &[crate::models::File { path: "none.gguf", bytes: 1, blob: "none" }],
            ..crate::models::TEXT
        };
        let core = Core::new(&index, &NONE, Box::new(move |event| seen.lock().unwrap().push(event.clone()))).unwrap();
        let request =
            |mode| Search { q: "pancakes".into(), mode: Some(mode), limit: 5, wait: true, ..Default::default() };

        // By words: nothing loads.
        let response = core.search(&request(Mode::Words)).unwrap();
        assert_eq!(response.hits[0].name, "pancakes.txt");
        assert_eq!(core.meaning_state(), MeaningState::Unloaded);
        // The same through a request, as a client gets it.
        let reply = core.handle(&Op::Search(request(Mode::Words))).unwrap();
        let results: Results = serde_json::from_value(reply).unwrap();
        assert_eq!(results.hits[0].name, "pancakes.txt");
        assert!(core.handle(&Op::Text { segment_id: None, path: None }).is_err());

        // By meaning: the model loads (here it can't), and the search is answered by words meanwhile.
        let response = core.search(&request(Mode::Meaning)).unwrap();
        assert!(!response.meaning);
        assert_eq!(response.hits[0].name, "pancakes.txt");
        assert_eq!(core.meaning_state(), MeaningState::Unavailable);
        let states: Vec<MeaningState> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::Meaning { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(states, [MeaningState::Loading, MeaningState::Unavailable]);

        // Unloading lets it be tried again.
        core.unload();
        assert_eq!(core.meaning_state(), MeaningState::Unloaded);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
