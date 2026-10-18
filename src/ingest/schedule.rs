//! Scheduling the reading: the queued files are read cheapest first (discover.rs), on a few worker threads (resources::IngestPlan), which
//! stream their pieces to the thread that runs the stages: embedding (embed.rs, the model's owner) and writing
//! (write.rs). Each worker has a small channel of its own, and they're taken from in turn, so a file with hundreds of
//! pieces (a long recording) can't keep a short one waiting behind it.
//!
//! While it runs, files that appear go to the front of the queue (`changes`): a new screenshot is searchable in a
//! second or two even while a long video is being read. Beside it, when memory allows, what's said in recordings is
//! written down on a lane of its own (listen.rs).
//!
//! A reader that hands over nothing for STUCK is stuck in a framework call that won't return, and no thread can be
//! stopped from outside: its file is set aside, and the run ends with `Stuck`, for a new process to go on.

use super::listen::Speech;
use super::write::Files;
use super::{Event, Indexer, Payload, Piece, discover};
use crate::config::Config;
use crate::index::FileRow;
use crate::resources::IngestPlan;
use crate::speech::Transcriber;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

enum Message {
    /// The first message about a file: when its reading started.
    Start(i64, Instant),
    Piece(i64, Piece),
    Done(i64, Result<serde_json::Value>),
}

/// A reader that hands over nothing for this long is stuck. Vision's text recognition can be: right after it compiles
/// its models for the Neural Engine (once per binary, the first time it reads text), it sometimes waits forever, and
/// cancelling the request doesn't help. That compile is also the longest real wait: almost five minutes for one
/// picture on an 8 GB Mac under memory pressure, and a reader queued behind it waits too (Vision reads text in one
/// picture at a time). Time asleep doesn't count: Instant stands still while the Mac sleeps.
const STUCK: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(600) };

/// The file a worker's reader is on, and since when it's been at it without handing over a piece.
type Busy = Arc<Mutex<Option<(i64, Instant)>>>;

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

impl Indexer {
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
        for _ in 0..IngestPlan::of(&config.indexing).readers {
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
                            super::read(&row.path, row.kind, &settings, &mut |piece| {
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

        let mut files = Files::default();
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
                    on_event(discover::sync_paths(&self.index, config, &changed)?);
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
                if files.completed == known.len() {
                    return Ok(());
                }
                if let Some(speech) = &mut speech {
                    self.listen_beside(speech, on_event, stop)?;
                }
                let completed = files.completed;
                let stuck =
                    busy.iter().find_map(|busy| busy.lock().unwrap().filter(|(_, since)| since.elapsed() >= STUCK));
                if let Some((id, _)) = stuck {
                    let row = &known[&id];
                    let error = format!("reading it got stuck: nothing came of it in {} minutes", STUCK.as_secs() / 60);
                    // Parts written of it go. It's tried again later (Writer::requeue_passing).
                    if self.index.set_aside(row, &error)? {
                        files.failed += 1;
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
                            self.handle(message, &known, &mut files, on_event)?;
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {}
                    }
                }
                turn += 1;
                if let Some(speech) = speech.as_mut().filter(|_| files.completed > completed) {
                    // A recording read is one to write down.
                    speech.look();
                }
                if !received {
                    // Every worker gone and a whole round empty: nothing more can come.
                    if workers_gone {
                        return Ok(());
                    }
                    // Nothing waiting: embed what's batched rather than sit on it, then wait for the doorbell.
                    self.flush(&mut files)?;
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
        self.embed.forget();
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
            indexed: files.indexed,
            failed: files.failed,
            seconds: started.elapsed().as_secs_f64(),
            stopped,
        });
        outcome
    }

    fn handle(
        &mut self,
        message: Message,
        known: &HashMap<i64, FileRow>,
        files: &mut Files,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        match message {
            Message::Start(id, started) => files.start(&known[&id], started),
            Message::Piece(id, piece) => {
                if !files.wanted(&known[&id]) {
                    return Ok(());
                }
                if matches!(piece.payload, Payload::Text(_)) {
                    self.embed.queue(id, piece);
                    if self.embed.full() {
                        self.flush(files)?;
                    }
                } else {
                    self.flush(files)?;
                    let (segment, failed) = self.embed.one(piece)?;
                    files.add((id, segment, failed));
                }
                files.write_parts(&mut self.index)?;
            }
            Message::Done(id, result) => {
                self.flush(files)?;
                files.end(&known[&id], result, &mut self.index, on_event)?;
            }
        }
        Ok(())
    }

    /// Embeds the batched texts of files still wanted, and writes what that fills to a part.
    fn flush(&mut self, files: &mut Files) -> Result<()> {
        for embedded in self.embed.flush(|file| files.keeps(file))? {
            files.add(embedded);
        }
        files.write_parts(&mut self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Depth;
    use crate::crawl::Found;
    use crate::index::Writer;
    use protocol::Kind;
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
