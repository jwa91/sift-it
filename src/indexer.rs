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
//! `transcribe_next` is the second stage, for recordings and videos the first pass is done with: what's said in them,
//! written down a chunk at a time (speech.rs), each chunk's passages embedded and stored as soon as they're back.

use crate::config::Config;
use crate::crawl::{self, Found, Kind, Rescan};
use crate::embedder::{Embedder, ModelFiles};
use crate::readers::{self, Payload, Piece};
use crate::store::{FileRow, NewSegment, Store};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
}

enum Message {
    /// The first message about a file: when its reading started.
    Start(i64, Instant),
    Piece(i64, Piece),
    Done(i64, Result<serde_json::Value>),
}

/// Segments a file keeps before they're written as a part (Store::write_part): about 2.5 MB of vectors.
const PART: usize = 256;

/// A reader that hands over nothing for this long is stuck. Vision's text recognition can be: right after it compiles
/// its models for the Neural Engine (once per binary, the first time it reads text), it sometimes waits forever, and
/// cancelling the request doesn't help. That compile is also the longest real wait: almost five minutes for one
/// picture on an 8 GB Mac under memory pressure, and a reader queued behind it waits too (Vision reads text in one
/// picture at a time). Time asleep doesn't count: Instant stands still while the Mac sleeps.
const STUCK: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(600) };

/// The file a worker's reader is on, and since when it's been at it without handing over a piece.
type Busy = Arc<Mutex<Option<(i64, Instant)>>>;

/// A reader got stuck (STUCK) on this file. It's set aside (Store::set_aside), and the run ends: the thread can't be
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
    pub store: Store,
    models: PathBuf,
    embedder: Option<Embedder>,
    /// The loaded model has the image and audio encoders (else it's the text model alone).
    media: bool,
}

impl Indexer {
    /// An indexer for this store. Vectors from another pipeline (another model or llama.cpp) are dropped, and their
    /// files queued again: they'd never compare right with new queries. Vector files no partition uses (what a crash
    /// left, or what was dropped) go.
    pub fn new(mut store: Store, models: PathBuf) -> Result<Self> {
        if store.adopt_vector_source(&crate::embedder::vector_source())? {
            eprintln!("the index was made by another model or pipeline: everything is indexed again");
        }
        if store.adopt_speech_source(crate::speech::source())? {
            eprintln!("what's said in recordings was written down by another speech model: it's written down again");
        }
        store.tidy_vectors()?;
        Ok(Self { store, models, embedder: None, media: false })
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
            self.store.files()?.into_iter().map(|row| (row.path.clone(), row)).collect();
        let found_paths: HashSet<&PathBuf> = crawl.found.iter().map(|f| &f.path).collect();
        let transaction = self.store.conn.unchecked_transaction()?;
        let (added, mut changed) = self.queue_found(&crawl.found, &existing)?;
        // Files where some piece couldn't be embedded, or reading got stuck: once in a while, try them again.
        changed += self.store.requeue_passing(6.0)?;
        let mut removed = 0;
        for (path, row) in &existing {
            if found_paths.contains(path) || crawl.unavailable.iter().any(|root| path.starts_with(root)) {
                continue;
            }
            self.store.remove_file(row.id)?;
            removed += 1;
        }
        transaction.commit()?;
        Ok(Event::Sync {
            files: crawl.found.len(),
            added,
            changed,
            removed,
            unavailable: crawl.unavailable.iter().map(|p| p.display().to_string()).collect(),
            pending: self.store.pending_count()?,
            ms: started.elapsed().as_millis() as u64,
        })
    }

    /// A sync of just these paths (files, or folders with everything in them).
    pub fn sync_paths(&mut self, config: &Config, paths: &[PathBuf]) -> Result<Event> {
        let started = Instant::now();
        let (mut files, mut added, mut changed, mut removed) = (0, 0, 0, 0);
        let transaction = self.store.conn.unchecked_transaction()?;
        for path in paths {
            let found = match crawl::rescan(config, path) {
                Rescan::Outside | Rescan::Unavailable => continue,
                Rescan::Skipped => Vec::new(),
                Rescan::Found(found) => found,
            };
            let existing: HashMap<PathBuf, FileRow> =
                self.store.files_under(path)?.into_iter().map(|row| (row.path.clone(), row)).collect();
            let (a, c) = self.queue_found(&found, &existing)?;
            let found_paths: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
            for (existing_path, row) in &existing {
                if !found_paths.contains(existing_path) {
                    self.store.remove_file(row.id)?;
                    removed += 1;
                }
            }
            files += found.len();
            added += a;
            changed += c;
        }
        transaction.commit()?;
        Ok(Event::Sync {
            files,
            added,
            changed,
            removed,
            unavailable: Vec::new(),
            pending: self.store.pending_count()?,
            ms: started.elapsed().as_millis() as u64,
        })
    }

    fn queue_found(&self, found: &[Found], existing: &HashMap<PathBuf, FileRow>) -> Result<(usize, usize)> {
        let (mut added, mut changed) = (0, 0);
        for file in found {
            match existing.get(&file.path) {
                None => {
                    self.store.queue(file, None, crate::estimate::cost(file))?;
                    added += 1;
                }
                Some(row) if is_changed(row, file) => {
                    self.store.queue(file, Some(row.id), crate::estimate::cost(file))?;
                    changed += 1;
                }
                Some(row) if row.speech != file.speech() => self.store.set_speech(row.id, file.speech())?,
                Some(_) => {}
            }
        }
        Ok((added, changed))
    }

    /// Indexes every queued file, and the files `changes` brings in meanwhile (paths that changed: they're synced,
    /// and what they queue goes first). Stops between pieces when `stop` says so; files not finished stay queued.
    pub fn index_pending(
        &mut self,
        config: &Config,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
        changes: &mut dyn FnMut() -> Vec<PathBuf>,
    ) -> Result<()> {
        let pending = self.store.pending(usize::MAX)?;
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
        let workers = std::thread::available_parallelism().map(|n| (n.get() / 3).clamp(2, 4)).unwrap_or(2);
        let mut receivers = Vec::new();
        let mut busy: Vec<Busy> = Vec::new();
        for _ in 0..workers {
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
                    // Read with this run's values. A file queued for other ones meanwhile isn't written (Store::write_file).
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
                        .store
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
                let stuck =
                    busy.iter().find_map(|busy| busy.lock().unwrap().filter(|(_, since)| since.elapsed() >= STUCK));
                if let Some((id, _)) = stuck {
                    let row = &known[&id];
                    let error = format!("reading it got stuck: nothing came of it in {} minutes", STUCK.as_secs() / 60);
                    // Parts written of it go. It's tried again later (Store::requeue_passing).
                    if self.store.set_aside(row, &error)? {
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
                        if self.store.finish_file(&assembly.row, &assembly.segments, &info, parts)? {
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
                        if self.store.fail_file(&assembly.row, &error)? {
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
        transcriber: &mut crate::speech::Transcriber,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let Some((row, from)) = self.store.transcripts_pending(1)?.into_iter().next() else { return Ok(false) };
        let path = row.path.display().to_string();
        let started = Instant::now();
        // The tags go in with the first stretch (they're read again when it starts over).
        let mut tags = (from == 0.0).then(|| crate::av::tags(&row.path));
        if !crate::av::has_audio(&row.path) {
            self.store.add_transcript(&row, from, &[], tags.as_ref(), "done")?;
            on_event(Event::Transcribed { path, passages: 0, seconds: 0.0, ms: 0 });
            return Ok(true);
        }
        let duration = crate::av::duration(&row.path);
        let title = crate::readers::title_of(&row.path);
        let title = tags.as_ref().and_then(|tags| tags["title"].as_str().map(str::to_string)).unwrap_or(title);
        let mut written = 0usize;
        let mut changed = false;
        let outcome = crate::speech::transcribe(transcriber, &row.path, from, &mut |start, end, spoken| {
            let passages = crate::speech::passages(spoken);
            let texts: Vec<String> =
                passages.iter().map(|p| crate::embedder::prompts::document(Some(&title), &p.text)).collect();
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
                    segment: crate::store::Segment::Speech,
                    loc: Some(p.start),
                    loc_end: Some(p.end),
                    text: Some(p.text),
                    vector: vectors.next(),
                })
                .collect();
            written += segments.len();
            if !self.store.add_transcript(&row, start, &segments, tags.take().as_ref(), &format!("at:{end:.2}"))? {
                // The file changed: it's being indexed again, and written down after that.
                changed = true;
                return Ok(false);
            }
            on_event(Event::Transcribing { path: path.clone(), at: end, duration });
            Ok(!stop())
        });
        match outcome {
            Ok(crate::speech::Outcome::Finished(end)) if !changed => {
                self.store.add_transcript(&row, end, &[], None, "done")?;
                on_event(Event::Transcribed {
                    path,
                    passages: written,
                    seconds: end - from,
                    ms: started.elapsed().as_millis() as u64,
                });
            }
            Ok(_) => {}
            // The helper stopped: the file isn't at fault, and goes on later with a new one.
            Err(error) if error.downcast_ref::<crate::speech::Gone>().is_some() => return Err(error),
            Err(error) => {
                let error = format!("{error:#}");
                self.store.add_transcript(&row, from, &[], None, &format!("failed: {error}"))?;
                on_event(Event::TranscribeFailed { path, error });
            }
        }
        Ok(true)
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
            if self.store.write_part(&assembly.row, &assembly.segments, first)? {
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
        let store = Store::open(&folder.join("index.sqlite")).unwrap();
        for path in [&stuck, &fine] {
            let settings = config.reading.resolve(path, Kind::Plaintext);
            let found = Found { path: path.clone(), kind: Kind::Plaintext, size: 1, modified: 1.0, settings };
            store.queue(&found, None, 0.0).unwrap();
        }
        let mut indexer = Indexer::new(store, folder.join("models")).unwrap();
        let mut failed = Vec::new();
        let mut events = |event| {
            if let Event::Failed { path, .. } = event {
                failed.push(path);
            }
        };
        let outcome = indexer.index_pending(&config, &mut events, &|| false, &mut Vec::new);

        assert!(outcome.unwrap_err().is::<Stuck>());
        assert_eq!(failed, [stuck.display().to_string()]);
        let state = |path: &Path| -> String {
            let path = path.to_string_lossy();
            indexer.store.conn.query_row("SELECT state FROM files WHERE path = ?1", [path], |row| row.get(0)).unwrap()
        };
        assert_eq!(state(&stuck), "failed");
        assert_eq!(state(&fine), "done");
        // It's tried again later.
        assert_eq!(indexer.store.requeue_passing(0.0).unwrap(), 1);
        assert_eq!(state(&stuck), "pending");

        // A writer lets the reader go: it finds the run over.
        drop(std::fs::OpenOptions::new().write(true).open(&stuck).unwrap());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
