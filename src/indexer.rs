//! Keeping the index in step with the chosen folders.
//!
//! `sync` crawls the folders and compares them with the index: new and changed files are queued, gone files leave.
//! `sync_paths` does the same for just the paths a file-system event named. `index_pending` then reads the queued files
//! on a few worker threads, which stream their pieces to this thread, the one that owns the model. Texts are embedded
//! in batches; each file is written in one transaction once all of it is in, so a search never sees half a file.
//!
//! While it runs, files that appear go to the front of the queue (`changes`): a new screenshot is searchable in a
//! second or two even while a long video is being read.

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
}

enum Message {
    /// The first message about a file: when its reading started.
    Start(i64, Instant),
    Piece(i64, Piece),
    Done(i64, Result<serde_json::Value>),
}

/// A file being put together from its pieces.
struct Assembly {
    row: FileRow,
    started: Instant,
    segments: Vec<NewSegment>,
    /// Pieces whose vector failed: they stay in the keyword index.
    failures: usize,
}

pub struct Indexer {
    pub store: Store,
    models: PathBuf,
    embedder: Option<Embedder>,
}

impl Indexer {
    /// An indexer for this store. Vectors from another pipeline (another model or llama.cpp) are dropped, and their
    /// files queued again: they'd never compare right with new queries.
    pub fn new(store: Store, models: PathBuf) -> Result<Self> {
        if store.adopt_vector_source(&crate::embedder::vector_source())? {
            eprintln!("the index was made by another model or pipeline: everything is indexed again");
        }
        Ok(Self { store, models, embedder: None })
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
        // Files where some piece couldn't be embedded: once in a while, try them again.
        changed += self.store.requeue_unembedded(6.0)?;
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
                Rescan::Outside => continue,
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
                    self.store.queue(file, None)?;
                    added += 1;
                }
                Some(row) if is_changed(row, file) => {
                    self.store.queue(file, Some(row.id))?;
                    changed += 1;
                }
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
        if self.embedder.is_none() {
            let files = ModelFiles::in_folder(&self.models)?;
            self.embedder = Some(Embedder::load(&files, false)?);
        }

        // The files handed out, by id (workers send only ids).
        let mut known: HashMap<i64, FileRow> = pending.iter().map(|row| (row.id, row.clone())).collect();
        let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
        let finished = Arc::new(AtomicBool::new(false));
        // Each worker has a small channel of its own, and this thread takes from them in turn: a file with hundreds
        // of pieces (a long recording) can't keep a short one waiting behind it. The doorbell wakes this thread.
        let (doorbell, door) = sync_channel::<()>(1);
        let workers = std::thread::available_parallelism().map(|n| (n.get() / 3).clamp(2, 4)).unwrap_or(2);
        let mut threads = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..workers {
            let (sender, receiver) = sync_channel::<Message>(4);
            receivers.push(receiver);
            let queue = queue.clone();
            let finished = finished.clone();
            let doorbell = doorbell.clone();
            threads.push(std::thread::spawn(move || {
                let send = |message: Message| -> Result<()> {
                    sender.send(message).map_err(|_| anyhow!("indexing stopped"))?;
                    let _ = doorbell.try_send(());
                    Ok(())
                };
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
                    // A reader that panics (a file no one expected) is a file that couldn't be read, not a lost run.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        objc2::rc::autoreleasepool(|_| {
                            readers::read(&row.path, row.kind, &mut |piece| send(Message::Piece(id, piece)))
                        })
                    }))
                    .unwrap_or_else(|_| Err(anyhow!("the reader crashed on it")));
                    if send(Message::Done(id, result)).is_err() {
                        break;
                    }
                }
            }));
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
                    self.embed_texts(&mut run);
                    match door.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => workers_gone = true,
                    }
                }
            }
        })();
        finished.store(true, Ordering::Relaxed);
        drop(receivers);
        for thread in threads {
            let _ = thread.join();
        }
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
            failures: 0,
        };
        match message {
            Message::Start(id, started) => {
                run.assemblies.insert(id, assembly(id, started));
            }
            Message::Piece(id, piece) => {
                run.assemblies.entry(id).or_insert_with(|| assembly(id, Instant::now()));
                if matches!(piece.payload, Payload::Text(_)) {
                    run.batch.push((id, piece));
                    if run.batch.len() >= 16 {
                        self.embed_texts(run);
                    }
                } else {
                    self.embed_texts(run);
                    let assembly = run.assemblies.get_mut(&id).expect("just made");
                    let segment = self.embed_piece(piece, assembly);
                    assembly.segments.push(segment);
                }
            }
            Message::Done(id, result) => {
                self.embed_texts(run);
                let assembly = run.assemblies.remove(&id).unwrap_or_else(|| assembly(id, Instant::now()));
                run.completed += 1;
                let path = assembly.row.path.display().to_string();
                match result {
                    Ok(info) => {
                        let vectors = assembly.segments.iter().filter(|s| s.vector.is_some()).count();
                        let info = if assembly.failures > 0 {
                            let mut info = info;
                            info["unembedded"] = serde_json::json!(assembly.failures);
                            info
                        } else {
                            info
                        };
                        // Not written when the file changed (or left) while it was read: it's queued again.
                        if self.store.finish_file(&assembly.row, &assembly.segments, &info)? {
                            run.indexed += 1;
                            on_event(Event::File {
                                path,
                                kind: assembly.row.kind,
                                segments: assembly.segments.len(),
                                vectors,
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

    /// Embeds the batched texts, one model call for up to 16 of them.
    fn embed_texts(&mut self, run: &mut Run) {
        if run.batch.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut run.batch);
        let texts: Vec<&str> = pieces
            .iter()
            .map(|(_, piece)| match &piece.payload {
                Payload::Text(text) => text.as_str(),
                _ => "",
            })
            .collect();
        let embedder = self.embedder.as_mut().expect("model loaded");
        let vectors = embedder.embed_texts(&texts);
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
    }

    fn embed_piece(&mut self, piece: Piece, assembly: &mut Assembly) -> NewSegment {
        let embedder = self.embedder.as_mut().expect("model loaded");
        let vector = match &piece.payload {
            Payload::None | Payload::Text(_) => None,
            Payload::Image(image) => embedder.embed_image(image).ok(),
            Payload::Audio(samples) => embedder.embed_audio(samples).ok(),
        };
        if vector.is_none() && !matches!(piece.payload, Payload::None) {
            assembly.failures += 1;
        }
        NewSegment { segment: piece.segment, loc: piece.loc, loc_end: piece.loc_end, text: piece.text, vector }
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

fn is_changed(row: &FileRow, found: &Found) -> bool {
    row.size != found.size || (row.modified - found.modified).abs() > 0.001 || row.kind != found.kind
}
