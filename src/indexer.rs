//! Keeping the index in step with the chosen folders.
//!
//! `sync` crawls the folders and compares them with the index: new and changed files are queued, each with an estimate
//! of its work, gone files leave. `sync_paths` does the same for just the paths a file-system event named.
//! `index_pending` then reads the queued files, cheapest first, on a few worker threads, which stream their pieces to
//! this thread, the one that owns the model (loaded when a piece first needs a vector). Texts are embedded in batches.
//! A file is written once all of it is in, or, for a big one, in parts of PART segments as they come: memory doesn't
//! grow with a file, and a search finds what's in.
//!
//! While it runs, files that appear go to the front of the queue (`changes`): a new screenshot is searchable in a
//! second or two even while a long video is being read.
//!
//! A reader that hands over nothing for STUCK is stuck in a framework call that won't return, and no thread can be
//! stopped from outside: its file is set aside, and the run ends with `Stuck`, for a new process to go on.
//!
//! The second stage, for recordings and videos the first pass is done with, is what's said in them, written down a
//! chunk at a time (speech.rs), each chunk's passages embedded and stored as soon as they're back. When memory allows
//! (Plan), it runs beside the reading, on a thread of its own that talks to the speech helper; else `transcribe_next`
//! does it once everything is read.

use crate::config::{self, Config};
use crate::crawl::{self, Found, Rescan};
use crate::embedder::{Embedder, ModelFiles};
use crate::index::{FileRow, NewSegment, Writer};
use crate::readers::{self, Payload, Piece};
use crate::speech::{Gone, Outcome, Spoken, Transcriber};
use anyhow::{Result, anyhow};
use protocol::Kind;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What `indexing.memory_mb` affords: how many files are read at once, and whether what's said in recordings and videos
/// is written down beside the reading or after it. Neither changes what's written.
#[derive(Debug, PartialEq)]
pub struct Plan {
    /// The budget, in bytes.
    pub memory: u64,
    pub readers: usize,
    pub beside: bool,
}

/// What the parts of indexing hold at their peak, measured on an 8 GB Mac: the embedding model with its image and
/// audio encoders (0.92 GB), the speech helper with a three-minute stretch of sound (1.0 GB), and a reader with what
/// it has decoded.
const MODEL: u64 = 1 << 30;
const SPEECH: u64 = 1 << 30;
const READER: u64 = 128 << 20;

/// The share of the Mac's memory indexing takes with `indexing.memory_mb` at auto: 2 GB of 8, where the models take
/// turns, and 6 GB of 24, where they run side by side.
const MEMORY_SHARE: f64 = 0.25;

impl Plan {
    pub fn of(indexing: &config::Indexing) -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self::within(indexing.memory_mb.bytes(MEMORY_SHARE), cores)
    }

    /// The plan for `budget` bytes on a Mac with `cores` cores. Reading takes at most a third of them (two to four),
    /// so the Mac stays quick; the budget can make that fewer, down to one. Speech is written down beside the reading
    /// when both models fit with two readers.
    fn within(budget: u64, cores: usize) -> Self {
        let beside = budget >= MODEL + SPEECH + 2 * READER;
        let left = budget.saturating_sub(MODEL + if beside { SPEECH } else { 0 });
        Self { memory: budget, readers: ((left / READER) as usize).clamp(1, (cores / 3).clamp(2, 4)), beside }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Sync {
        files: usize,
        added: usize,
        changed: usize,
        removed: usize,
        unavailable: Vec<String>,
        pending: usize,
        ms: u64,
    },
    Started {
        pending: usize,
    },
    /// Files that appeared while indexing joined the queue.
    Queued {
        count: usize,
    },
    File {
        path: String,
        kind: Kind,
        segments: usize,
        vectors: usize,
        ms: u64,
    },
    Failed {
        path: String,
        error: String,
    },
    Finished {
        indexed: usize,
        failed: usize,
        seconds: f64,
        stopped: bool,
    },
    /// A stretch of a recording or video was written down: what's said up to `at` seconds of `duration`.
    Transcribing {
        path: String,
        at: f64,
        duration: f64,
    },
    /// A recording or video was written down to its end.
    Transcribed {
        path: String,
        passages: usize,
        seconds: f64,
        ms: u64,
    },
    TranscribeFailed {
        path: String,
        error: String,
    },
    /// Writing down speech beside the reading stopped: the speech helper couldn't start, or it stopped. It goes on
    /// once everything is read.
    SpeechStopped {
        error: String,
    },
}

enum Message {
    /// The first message about a file: when its reading started.
    Start(i64, Instant),
    Piece(i64, Piece),
    Done(i64, Result<serde_json::Value>),
}

/// Segments a file keeps before they're written as a part (Writer::write_part): about 2.5 MB of vectors.
const PART: usize = 256;

/// A reader that hands over nothing for this long is stuck. Vision's text recognition can be: right after it compiles
/// its models for the Neural Engine (once per binary, the first time it reads text), it sometimes waits forever, and
/// cancelling the request doesn't help. That compile is also the longest real wait: almost five minutes for one
/// picture on an 8 GB Mac under memory pressure, and a reader queued behind it waits too (Vision reads text in one
/// picture at a time). Time asleep doesn't count: Instant stands still while the Mac sleeps.
const STUCK: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(600) };

/// The file a worker's reader is on, and since when it's been at it without handing over a piece.
type Busy = Arc<Mutex<Option<(i64, Instant)>>>;

/// A recording or video whose speech is being written down.
struct Listening {
    row: FileRow,
    /// Where this goes on from, in seconds.
    from: f64,
    started: Instant,
    title: String,
    /// Its tags, stored with the first stretch.
    tags: Option<serde_json::Value>,
    duration: f64,
    passages: usize,
    /// It changed meanwhile: it's read again, and written down after that.
    changed: bool,
}

/// The thread that writes down speech beside the reading, through the speech helper, and the file it's on.
struct Speech {
    /// Files to write down, from where they got to.
    files: SyncSender<(FileRow, f64)>,
    /// What it heard: a stretch at a time, then how the file ended.
    heard: Receiver<Heard>,
    /// Whether it goes on after a stretch: false stops the file there.
    go: SyncSender<bool>,
    /// It gives the helper back when it's done, unless the helper stopped.
    thread: JoinHandle<Option<Transcriber>>,
    listening: Option<Listening>,
    /// Look for a file to write down: at the start, and when a file is read or one is written down.
    look: bool,
    /// It stopped: nothing more goes to it in this run.
    over: bool,
}

enum Heard {
    /// The sentences heard from `start` to `end` seconds.
    Stretch {
        start: f64,
        end: f64,
        spoken: Vec<Spoken>,
    },
    Ended(Result<Outcome>),
    /// The helper couldn't start.
    Unavailable(String),
}

impl Speech {
    /// The thread, with `helper` when there is one already (else it starts one for its first file).
    fn start(mut helper: Option<Transcriber>, doorbell: SyncSender<()>) -> Self {
        let (files, to_do) = sync_channel::<(FileRow, f64)>(1);
        let (sender, heard) = sync_channel::<Heard>(1);
        let (go, go_on) = sync_channel::<bool>(1);
        let thread = std::thread::spawn(move || {
            let tell = |heard: Heard| {
                let told = sender.send(heard).is_ok();
                let _ = doorbell.try_send(());
                told
            };
            while let Ok((row, from)) = to_do.recv() {
                if helper.is_none() {
                    match Transcriber::start() {
                        Ok(started) => helper = Some(started),
                        Err(error) => {
                            tell(Heard::Unavailable(format!("{error:#}")));
                            return None;
                        }
                    }
                }
                let transcriber = helper.as_mut().expect("started");
                // AVFoundation's objects go when the file is done, as in a reader.
                let outcome = objc2::rc::autoreleasepool(|_| {
                    crate::speech::transcribe(transcriber, &row.path, from, &mut |start, end, spoken| {
                        Ok(tell(Heard::Stretch { start, end, spoken }) && go_on.recv().unwrap_or(false))
                    })
                });
                let gone = outcome.as_ref().is_err_and(|error| error.is::<Gone>());
                if gone {
                    helper = None;
                }
                if !tell(Heard::Ended(outcome)) || gone {
                    break;
                }
            }
            helper
        });
        Self { files, heard, go, thread, listening: None, look: true, over: false }
    }
}

/// A reader got stuck (STUCK) on this file. It's set aside (Writer::set_aside), and the run ends: the thread can't be
/// stopped, and what it's stuck in stays stuck for this process (Vision reads no more text in it). A new process goes
/// on with the rest.
#[derive(Debug)]
pub struct Stuck(pub PathBuf);

impl std::fmt::Display for Stuck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reading {} got stuck: nothing came of it in {} minutes", self.0.display(), STUCK.as_secs() / 60)
    }
}

impl std::error::Error for Stuck {}

/// A file being put together from its pieces.
struct Assembly {
    row: FileRow,
    started: Instant,
    /// The segments not written yet.
    segments: Vec<NewSegment>,
    /// Segments written as parts, and how many of them have a vector.
    written: usize,
    vectors: usize,
    /// Pieces whose vector failed: they stay in the keyword index.
    failures: usize,
    /// The file changed (or left) while it was read: what's left of it isn't embedded or written.
    stale: bool,
}

pub struct Indexer {
    pub index: Writer,
    models: PathBuf,
    embedder: Option<Embedder>,
    /// The loaded model has the image and audio encoders (else it's the text model alone).
    media: bool,
}

impl Indexer {
    /// An indexer writing to `index`. Vectors from another pipeline (another model or llama.cpp) are dropped, and their
    /// files queued again: they'd never compare right with new queries. Vector files no partition uses (what a crash
    /// left, or what was dropped) go.
    pub fn new(mut index: Writer, models: PathBuf) -> Result<Self> {
        if index.adopt_vector_source(&crate::embedder::vector_source())? {
            eprintln!("the index was made by another model or pipeline: everything is indexed again");
        }
        if index.adopt_speech_source(crate::speech::source())? {
            eprintln!("what's said in recordings was written down by another speech model: it's written down again");
        }
        index.tidy_vectors()?;
        Ok(Self { index, models, embedder: None, media: false })
    }

    /// Frees the model (about 1 GB) until it's needed again.
    pub fn release_model(&mut self) {
        self.embedder = None;
    }

    pub fn model_loaded(&self) -> bool {
        self.embedder.is_some()
    }

    pub fn sync(&mut self, config: &Config) -> Result<Event> {
        let started = Instant::now();
        let crawl = crawl::crawl(config);
        let existing: HashMap<PathBuf, FileRow> =
            self.index.files()?.into_iter().map(|row| (row.path.clone(), row)).collect();
        let found_paths: HashSet<&PathBuf> = crawl.found.iter().map(|f| &f.path).collect();
        let (added, changed, removed) = self.index.batch(|| {
            let (added, mut changed) = self.queue_found(&crawl.found, &existing)?;
            // Files where some piece couldn't be embedded, or reading got stuck: once in a while, try them again.
            changed += self.index.requeue_passing(6.0)?;
            let mut removed = 0;
            for (path, row) in &existing {
                if found_paths.contains(path) || crawl.unavailable.iter().any(|root| path.starts_with(root)) {
                    continue;
                }
                self.index.remove_file(row.id)?;
                removed += 1;
            }
            Ok((added, changed, removed))
        })?;
        Ok(Event::Sync {
            files: crawl.found.len(),
            added,
            changed,
            removed,
            unavailable: crawl.unavailable.iter().map(|p| p.display().to_string()).collect(),
            pending: self.index.pending_count()?,
            ms: started.elapsed().as_millis() as u64,
        })
    }

    /// A sync of just these paths (files, or folders with everything in them).
    pub fn sync_paths(&mut self, config: &Config, paths: &[PathBuf]) -> Result<Event> {
        let started = Instant::now();
        let (mut files, mut added, mut changed, mut removed) = (0, 0, 0, 0);
        self.index.batch(|| {
            for path in paths {
                let found = match crawl::rescan(config, path) {
                    Rescan::Outside | Rescan::Unavailable => continue,
                    Rescan::Skipped => Vec::new(),
                    Rescan::Found(found) => found,
                };
                let existing: HashMap<PathBuf, FileRow> =
                    self.index.files_under(path)?.into_iter().map(|row| (row.path.clone(), row)).collect();
                let (a, c) = self.queue_found(&found, &existing)?;
                let found_paths: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
                for (existing_path, row) in &existing {
                    if !found_paths.contains(existing_path) {
                        self.index.remove_file(row.id)?;
                        removed += 1;
                    }
                }
                files += found.len();
                added += a;
                changed += c;
            }
            Ok(())
        })?;
        Ok(Event::Sync {
            files,
            added,
            changed,
            removed,
            unavailable: Vec::new(),
            pending: self.index.pending_count()?,
            ms: started.elapsed().as_millis() as u64,
        })
    }

    fn queue_found(&self, found: &[Found], existing: &HashMap<PathBuf, FileRow>) -> Result<(usize, usize)> {
        let (mut added, mut changed) = (0, 0);
        for file in found {
            match existing.get(&file.path) {
                None => {
                    self.index.queue(&file.new_file(), None, crate::estimate::cost(file))?;
                    added += 1;
                }
                Some(row) if is_changed(row, file) => {
                    self.index.queue(&file.new_file(), Some(row.id), crate::estimate::cost(file))?;
                    changed += 1;
                }
                Some(row) if row.speech != file.speech() => self.index.set_speech(row.id, file.speech())?,
                Some(_) => {}
            }
        }
        Ok((added, changed))
    }

    /// Indexes every queued file, and the files `changes` brings in meanwhile (paths that changed: they're synced,
    /// and what they queue goes first). Stops between pieces when `stop` says so; files not finished stay queued.
    /// With `helper`, what's said in recordings and videos is written down beside the reading, with the helper in it
    /// or one it starts, which it leaves there.
    pub fn index_pending(
        &mut self,
        config: &Config,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
        changes: &mut dyn FnMut() -> Vec<PathBuf>,
        helper: Option<&mut Option<Transcriber>>,
    ) -> Result<()> {
        let pending = self.index.pending(usize::MAX)?;
        if pending.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        on_event(Event::Started { pending: pending.len() });

        // The files handed out, by id (workers send only ids).
        let mut known: HashMap<i64, FileRow> = pending.iter().map(|row| (row.id, row.clone())).collect();
        let reading = Arc::new(config.reading.clone());
        let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
        let finished = Arc::new(AtomicBool::new(false));
        // Each worker has a small channel of its own, and this thread takes from them in turn: a file with hundreds
        // of pieces (a long recording) can't keep a short one waiting behind it. The doorbell wakes this thread.
        let (doorbell, door) = sync_channel::<()>(1);
        let mut receivers = Vec::new();
        let mut busy: Vec<Busy> = Vec::new();
        for _ in 0..Plan::of(&config.indexing).readers {
            let (sender, receiver) = sync_channel::<Message>(4);
            receivers.push(receiver);
            let worker_busy = Arc::new(Mutex::new(None));
            busy.push(worker_busy.clone());
            let queue = queue.clone();
            let finished = finished.clone();
            let doorbell = doorbell.clone();
            let reading = reading.clone();
            std::thread::spawn(move || {
                let send = |message: Message| -> Result<()> {
                    sender.send(message).map_err(|_| anyhow!("indexing stopped"))?;
                    let _ = doorbell.try_send(());
                    Ok(())
                };
                let mark = |id: Option<i64>| *worker_busy.lock().unwrap() = id.map(|id| (id, Instant::now()));
                while !finished.load(Ordering::Relaxed) {
                    let Some(row) = queue.lock().unwrap().pop_front() else {
                        // More may come (a file that appeared): wait until the run is over.
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    };
                    let id = row.id;
                    if send(Message::Start(id, Instant::now())).is_err() {
                        break;
                    }
                    // Read with this run's values. A file queued for other ones meanwhile isn't written (Writer::write_file).
                    let settings = reading.resolve(&row.path, row.kind);
                    mark(Some(id));
                    // A reader that panics (a file no one expected) is a file that couldn't be read, not a lost run.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        objc2::rc::autoreleasepool(|_| {
                            readers::read(&row.path, row.kind, &settings, &mut |piece| {
                                // While a piece waits for this thread to take it, the reader isn't the one stuck.
                                mark(None);
                                let sent = send(Message::Piece(id, piece));
                                mark(Some(id));
                                sent
                            })
                        })
                    }))
                    .unwrap_or_else(|_| Err(anyhow!("the reader crashed on it")));
                    mark(None);
                    if send(Message::Done(id, result)).is_err() {
                        break;
                    }
                }
            });
        }
        let (helper, mut speech) = match helper {
            Some(helper) => {
                let speech = Speech::start(helper.take(), doorbell.clone());
                (Some(helper), Some(speech))
            }
            None => (None, None),
        };
        drop(doorbell);

        let mut run = Run { assemblies: HashMap::new(), batch: Vec::new(), indexed: 0, failed: 0, completed: 0 };
        let mut stopped = false;
        let mut turn = 0usize;
        let mut workers_gone = false;
        let outcome = (|| -> Result<()> {
            loop {
                if stop() {
                    stopped = true;
                    return Ok(());
                }
                let changed = changes();
                if !changed.is_empty() {
                    on_event(self.sync_paths(config, &changed)?);
                    let fresh: Vec<FileRow> = self
                        .index
                        .pending(usize::MAX)?
                        .into_iter()
                        .filter(|row| !known.contains_key(&row.id))
                        .collect();
                    if !fresh.is_empty() {
                        on_event(Event::Queued { count: fresh.len() });
                        let mut queue = queue.lock().unwrap();
                        for row in fresh.into_iter().rev() {
                            known.insert(row.id, row.clone());
                            queue.push_front(row);
                        }
                    }
                }
                if run.completed == known.len() {
                    return Ok(());
                }
                if let Some(speech) = &mut speech {
                    self.listen_beside(speech, on_event, stop)?;
                }
                let completed = run.completed;
                let stuck =
                    busy.iter().find_map(|busy| busy.lock().unwrap().filter(|(_, since)| since.elapsed() >= STUCK));
                if let Some((id, _)) = stuck {
                    let row = &known[&id];
                    let error = format!("reading it got stuck: nothing came of it in {} minutes", STUCK.as_secs() / 60);
                    // Parts written of it go. It's tried again later (Writer::requeue_passing).
                    if self.index.set_aside(row, &error)? {
                        run.failed += 1;
                        on_event(Event::Failed { path: row.path.display().to_string(), error });
                    }
                    return Err(Stuck(row.path.clone()).into());
                }
                // One message from each worker that has one, in turn.
                let mut received = false;
                for offset in 0..receivers.len() {
                    let index = (turn + offset) % receivers.len();
                    match receivers[index].try_recv() {
                        Ok(message) => {
                            received = true;
                            self.handle(message, &known, &mut run, on_event)?;
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {}
                    }
                }
                turn += 1;
                if let Some(speech) = speech.as_mut().filter(|_| run.completed > completed) {
                    // A recording read is one to write down.
                    speech.look = true;
                }
                if !received {
                    // Every worker gone and a whole round empty: nothing more can come.
                    if workers_gone {
                        return Ok(());
                    }
                    // Nothing waiting: embed what's batched rather than sit on it, then wait for the doorbell.
                    self.embed_texts(&mut run)?;
                    match door.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => workers_gone = true,
                    }
                }
            }
        })();
        // Nothing waits for the workers: each stops at its next piece (its channel is gone) or when it finds the run
        // over. One stuck in a reader never would.
        finished.store(true, Ordering::Relaxed);
        drop(receivers);
        // The speech thread stops after the stretch it's on, which is stored: no work is lost, and the file goes on
        // from there.
        let outcome = match (speech, helper) {
            (Some(speech), Some(helper)) => {
                let finished = self.finish_listening(speech, helper, on_event);
                outcome.and(finished)
            }
            _ => outcome,
        };
        on_event(Event::Finished {
            indexed: run.indexed,
            failed: run.failed,
            seconds: started.elapsed().as_secs_f64(),
            stopped,
        });
        outcome
    }

    fn handle(
        &mut self,
        message: Message,
        known: &HashMap<i64, FileRow>,
        run: &mut Run,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        let assembly = |id: i64, started: Instant| Assembly {
            row: known[&id].clone(),
            started,
            segments: Vec::new(),
            written: 0,
            vectors: 0,
            failures: 0,
            stale: false,
        };
        match message {
            Message::Start(id, started) => {
                run.assemblies.insert(id, assembly(id, started));
            }
            Message::Piece(id, piece) => {
                if run.assemblies.entry(id).or_insert_with(|| assembly(id, Instant::now())).stale {
                    return Ok(());
                }
                if matches!(piece.payload, Payload::Text(_)) {
                    run.batch.push((id, piece));
                    if run.batch.len() >= 16 {
                        self.embed_texts(run)?;
                    }
                } else {
                    self.embed_texts(run)?;
                    let (segment, failed) = self.embed_piece(piece)?;
                    let assembly = run.assemblies.get_mut(&id).expect("just made");
                    assembly.failures += failed as usize;
                    assembly.segments.push(segment);
                }
                self.write_parts(run)?;
            }
            Message::Done(id, result) => {
                self.embed_texts(run)?;
                let assembly = run.assemblies.remove(&id).unwrap_or_else(|| assembly(id, Instant::now()));
                run.completed += 1;
                if assembly.stale {
                    return Ok(());
                }
                let path = assembly.row.path.display().to_string();
                match result {
                    Ok(info) => {
                        let info = if assembly.failures > 0 {
                            let mut info = info;
                            info["unembedded"] = serde_json::json!(assembly.failures);
                            info
                        } else {
                            info
                        };
                        let parts = assembly.written > 0;
                        // Not written when the file changed (or left) while it was read: it's queued again.
                        if self.index.finish_file(&assembly.row, &assembly.segments, &info, parts)? {
                            run.indexed += 1;
                            on_event(Event::File {
                                path,
                                kind: assembly.row.kind,
                                segments: assembly.written + assembly.segments.len(),
                                vectors: assembly.vectors
                                    + assembly.segments.iter().filter(|s| s.vector.is_some()).count(),
                                ms: assembly.started.elapsed().as_millis() as u64,
                            });
                        }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if self.index.fail_file(&assembly.row, &error)? {
                            run.failed += 1;
                            on_event(Event::Failed { path, error });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes down what's said in the next recording or video that's waiting for it, a chunk at a time, until it's
    /// done or `stop` says so between chunks (it goes on from there next time). False when there's none waiting.
    pub fn transcribe_next(
        &mut self,
        transcriber: &mut Transcriber,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let Some(mut listening) = self.next_listening(on_event)? else { return Ok(false) };
        let path = listening.row.path.clone();
        let outcome = crate::speech::transcribe(transcriber, &path, listening.from, &mut |start, end, spoken| {
            Ok(self.store_heard(&mut listening, start, end, spoken, on_event)? && !stop())
        });
        self.end_listening(listening, outcome, on_event)?;
        Ok(true)
    }

    /// The next recording or video waiting for what's said in it to be written down, from where it got to (one
    /// without sound is done at once), or None.
    fn next_listening(&mut self, on_event: &mut dyn FnMut(Event)) -> Result<Option<Listening>> {
        while let Some((row, from)) = self.index.transcripts_pending(1)?.into_iter().next() {
            // The tags go in with the first stretch (they're read again when it starts over).
            let tags = (from == 0.0).then(|| crate::av::tags(&row.path));
            if !crate::av::has_audio(&row.path) {
                self.index.add_transcript(&row, from, &[], tags.as_ref(), "done")?;
                let path = row.path.display().to_string();
                on_event(Event::Transcribed { path, passages: 0, seconds: 0.0, ms: 0 });
                continue;
            }
            let title = tags.as_ref().and_then(|tags| tags["title"].as_str().map(str::to_string));
            return Ok(Some(Listening {
                title: title.unwrap_or_else(|| crate::readers::title_of(&row.path)),
                duration: crate::av::duration(&row.path),
                row,
                from,
                started: Instant::now(),
                tags,
                passages: 0,
                changed: false,
            }));
        }
        Ok(None)
    }

    /// Stores a stretch of what's said in a file, its passages embedded. False when the file changed meanwhile.
    fn store_heard(
        &mut self,
        listening: &mut Listening,
        start: f64,
        end: f64,
        spoken: Vec<Spoken>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<bool> {
        let passages = crate::speech::passages(spoken);
        let texts: Vec<String> =
            passages.iter().map(|p| crate::embedder::prompts::document(Some(&listening.title), &p.text)).collect();
        let vectors = if texts.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            self.text_model()?.embed_texts(&refs).unwrap_or_default()
        };
        let mut vectors = vectors.into_iter();
        let segments: Vec<NewSegment> = passages
            .into_iter()
            .map(|p| NewSegment {
                segment: protocol::Segment::Speech,
                loc: Some(p.start),
                loc_end: Some(p.end),
                text: Some(p.text),
                vector: vectors.next(),
            })
            .collect();
        listening.passages += segments.len();
        let (row, tags) = (&listening.row, listening.tags.take());
        if !self.index.add_transcript(row, start, &segments, tags.as_ref(), &format!("at:{end:.2}"))? {
            listening.changed = true;
            return Ok(false);
        }
        on_event(Event::Transcribing { path: row.path.display().to_string(), at: end, duration: listening.duration });
        Ok(true)
    }

    /// Ends writing down a file: to its end, stopped partway (it goes on from there next time), or failed. An error
    /// when the helper stopped: the file isn't at fault, and goes on later with a new helper.
    fn end_listening(
        &mut self,
        listening: Listening,
        outcome: Result<Outcome>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        let path = listening.row.path.display().to_string();
        match outcome {
            Ok(Outcome::Finished(end)) if !listening.changed => {
                self.index.add_transcript(&listening.row, end, &[], None, "done")?;
                on_event(Event::Transcribed {
                    path,
                    passages: listening.passages,
                    seconds: end - listening.from,
                    ms: listening.started.elapsed().as_millis() as u64,
                });
            }
            Ok(_) => {}
            Err(error) if error.is::<Gone>() => return Err(error),
            Err(error) => {
                let error = format!("{error:#}");
                self.index.add_transcript(&listening.row, listening.from, &[], None, &format!("failed: {error}"))?;
                on_event(Event::TranscribeFailed { path, error });
            }
        }
        Ok(())
    }

    /// Beside the reading: stores what the speech thread heard, and hands it its next file when it's free.
    fn listen_beside(
        &mut self,
        speech: &mut Speech,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
    ) -> Result<()> {
        while let Ok(heard) = speech.heard.try_recv() {
            match heard {
                Heard::Stretch { start, end, spoken } => {
                    let listening = speech.listening.as_mut().expect("a stretch of the file it's on");
                    // The reading needs the model with its encoders: loaded now, it isn't loaded twice.
                    let stored = self
                        .media_model()
                        .map(drop)
                        .and_then(|()| self.store_heard(listening, start, end, spoken, on_event));
                    // The thread waits for this whatever happened.
                    let _ = speech.go.send(matches!(stored, Ok(true)) && !stop());
                    stored?;
                }
                Heard::Ended(outcome) => {
                    let listening = speech.listening.take().expect("the file it's on");
                    if let Err(error) = self.end_listening(listening, outcome, on_event) {
                        if !error.is::<Gone>() {
                            return Err(error);
                        }
                        speech.over = true;
                        on_event(Event::SpeechStopped { error: format!("{error:#}") });
                    }
                    speech.look = true;
                }
                Heard::Unavailable(error) => {
                    speech.listening = None;
                    speech.over = true;
                    on_event(Event::SpeechStopped { error });
                }
            }
        }
        if speech.listening.is_none() && speech.look && !speech.over {
            speech.look = false;
            if let Some(listening) = self.next_listening(on_event)? {
                if speech.files.send((listening.row.clone(), listening.from)).is_ok() {
                    speech.listening = Some(listening);
                } else {
                    speech.over = true;
                }
            }
        }
        Ok(())
    }

    /// Stops the speech thread once the stretch it's on is stored, and puts the helper back in `helper`.
    fn finish_listening(
        &mut self,
        speech: Speech,
        helper: &mut Option<Transcriber>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        let Speech { files, heard, go, thread, mut listening, .. } = speech;
        drop(files);
        let finished = (|| -> Result<()> {
            while let Some(current) = listening.as_mut() {
                match heard.recv() {
                    Ok(Heard::Stretch { start, end, spoken }) => {
                        let stored = self.store_heard(current, start, end, spoken, on_event);
                        let _ = go.send(false);
                        stored?;
                    }
                    Ok(Heard::Ended(outcome)) => {
                        let ended = listening.take().expect("the file it's on");
                        if let Err(error) = self.end_listening(ended, outcome, on_event)
                            && !error.is::<Gone>()
                        {
                            return Err(error);
                        }
                    }
                    Ok(Heard::Unavailable(_)) | Err(_) => listening = None,
                }
            }
            Ok(())
        })();
        // Without its channels, a thread still waiting stops at once.
        drop((heard, go));
        *helper = thread.join().ok().flatten();
        finished
    }

    /// A model for text: the loaded one, or else the text model alone (a third of the memory, quick to load).
    fn text_model(&mut self) -> Result<&mut Embedder> {
        if self.embedder.is_none() {
            let files = ModelFiles::in_folder(&self.models)?;
            self.embedder = Some(Embedder::load(&files, true)?);
            self.media = false;
        }
        Ok(self.embedder.as_mut().expect("just loaded"))
    }

    /// The model with its image and audio encoders: the loaded one, or loaded now.
    fn media_model(&mut self) -> Result<&mut Embedder> {
        if self.embedder.is_none() || !self.media {
            self.embedder = None;
            let files = ModelFiles::in_folder(&self.models)?;
            self.embedder = Some(Embedder::load(&files, false)?);
            self.media = true;
        }
        Ok(self.embedder.as_mut().expect("just loaded"))
    }

    /// Writes the segments of files that have PART of them waiting, as a part.
    fn write_parts(&mut self, run: &mut Run) -> Result<()> {
        for assembly in run.assemblies.values_mut().filter(|a| !a.stale && a.segments.len() >= PART) {
            let first = assembly.written == 0;
            if self.index.write_part(&assembly.row, &assembly.segments, first)? {
                assembly.written += assembly.segments.len();
                assembly.vectors += assembly.segments.iter().filter(|s| s.vector.is_some()).count();
            } else {
                // It changed while it was read: it's queued again, and read again from the start.
                assembly.stale = true;
            }
            assembly.segments.clear();
        }
        Ok(())
    }

    /// Embeds the batched texts, one model call for up to 16 of them.
    fn embed_texts(&mut self, run: &mut Run) -> Result<()> {
        run.batch.retain(|(id, _)| run.assemblies.get(id).is_some_and(|assembly| !assembly.stale));
        if run.batch.is_empty() {
            return Ok(());
        }
        let pieces = std::mem::take(&mut run.batch);
        let texts: Vec<&str> = pieces
            .iter()
            .map(|(_, piece)| match &piece.payload {
                Payload::Text(text) => text.as_str(),
                _ => "",
            })
            .collect();
        let vectors = self.media_model()?.embed_texts(&texts);
        let failed = vectors.is_err();
        let mut vectors = vectors.unwrap_or_default().into_iter();
        for (id, piece) in pieces {
            let Some(assembly) = run.assemblies.get_mut(&id) else { continue };
            if failed {
                assembly.failures += 1;
            }
            assembly.segments.push(NewSegment {
                segment: piece.segment,
                loc: piece.loc,
                loc_end: piece.loc_end,
                text: piece.text,
                vector: vectors.next(),
            });
        }
        self.write_parts(run)
    }

    /// A picture or sound embedded (the model loads for the first one), or a piece without a vector (words alone).
    /// True with it when it couldn't be embedded: it stays in the keyword index.
    fn embed_piece(&mut self, piece: Piece) -> Result<(NewSegment, bool)> {
        let vector = match &piece.payload {
            Payload::None | Payload::Text(_) => Ok(None),
            Payload::Image(image) => self.media_model()?.embed_image(image).map(Some),
            Payload::Audio(samples) => self.media_model()?.embed_audio(samples).map(Some),
        };
        let failed = vector.is_err();
        let vector = vector.ok().flatten();
        Ok((
            NewSegment { segment: piece.segment, loc: piece.loc, loc_end: piece.loc_end, text: piece.text, vector },
            failed,
        ))
    }
}

/// The state of one `index_pending` run.
struct Run {
    assemblies: HashMap<i64, Assembly>,
    batch: Vec<(i64, Piece)>,
    indexed: usize,
    failed: usize,
    completed: usize,
}

/// Changed, or to be read another way.
fn is_changed(row: &FileRow, found: &Found) -> bool {
    row.size != found.size
        || (row.modified - found.modified).abs() > 0.001
        || row.kind != found.kind
        || row.reading != found.reading()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Depth;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[test]
    fn memory_decides_the_readers_and_whether_speech_goes_beside_the_reading() {
        let gb = 1u64 << 30;
        let plan = |budget, cores| {
            let plan = Plan::within(budget, cores);
            (plan.readers, plan.beside)
        };
        // A quarter of 8 GB: the models take turns, and the cores decide the readers.
        assert_eq!(plan(2 * gb, 8), (2, false));
        // A quarter of 24 GB, on ten cores: both models, three readers.
        assert_eq!(plan(6 * gb, 10), (3, true));
        // Little memory: one reader at a time.
        assert_eq!(plan(gb + READER, 8), (1, false));
        assert_eq!(plan(0, 8), (1, false));
    }

    /// A reader that never hands over anything (a pipe no one writes to: opening it waits for a writer) gets its file
    /// set aside and ends the run, and the other files are read meanwhile.
    #[test]
    fn a_stuck_reader_sets_its_file_aside_and_ends_the_run() {
        let folder = std::env::temp_dir().join(format!("sift-it-test-stuck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let (stuck, fine) = (folder.join("stuck.txt"), folder.join("fine.txt"));
        std::fs::write(&fine, "a few words to find").unwrap();
        let pipe = std::ffi::CString::new(stuck.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);

        let mut config = Config::default();
        config.reading.all.depth = Some(Depth::Words);
        let index = Writer::open(&folder.join("index.sqlite")).unwrap();
        for path in [&stuck, &fine] {
            let settings = config.reading.resolve(path, Kind::Plaintext);
            let found = Found { path: path.clone(), kind: Kind::Plaintext, size: 1, modified: 1.0, settings };
            index.queue(&found.new_file(), None, 0.0).unwrap();
        }
        let mut indexer = Indexer::new(index, folder.join("models")).unwrap();
        let mut failed = Vec::new();
        let mut events = |event| {
            if let Event::Failed { path, .. } = event {
                failed.push(path);
            }
        };
        let outcome = indexer.index_pending(&config, &mut events, &|| false, &mut Vec::new, None);

        assert!(outcome.unwrap_err().is::<Stuck>());
        assert_eq!(failed, [stuck.display().to_string()]);
        let state = |path: &Path| indexer.index.file_state(path);
        assert_eq!(state(&stuck), "failed");
        assert_eq!(state(&fine), "done");
        // It's tried again later.
        assert_eq!(indexer.index.requeue_passing(0.0).unwrap(), 1);
        assert_eq!(state(&stuck), "pending");

        // A writer lets the reader go: it finds the run over.
        drop(std::fs::OpenOptions::new().write(true).open(&stuck).unwrap());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
