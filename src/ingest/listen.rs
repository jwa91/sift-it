//! The speech lane: the speech round, on a thread of its own that talks to the speech helper. The speech extractor
//! (extract/speech.rs) hands over a stretch of a file at a time; its words are cut into passages (segment/speech.rs)
//! and stored as soon as they're back, for the keyword index at once and for the text round to embed, so a file stopped
//! partway goes on from there. The thread doesn't wait for the run to store them: it goes on with the next stretch,
//! and the next file waits beside it, so the Neural Engine never waits on the GPU. The run stops a file by naming it
//! (`halt`): it changed meanwhile, or the lane is stopping.

use super::extract::speech::{self, Outcome};
use super::{Event, Indexer, segment};
use crate::adapters::speech::{Gone, Spoken, Transcriber};
use crate::index::{Done, NewSegment, Work};
use anyhow::Result;
use protocol::Round;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Instant;

/// Stretches the lane's thread may be ahead of the run, and files waiting beside the one it's on.
const AHEAD: usize = 4;

/// `halt` for every file: the lane stops after the stretch it's on.
const EVERY: i64 = i64::MIN;

/// A recording or video whose speech is being written down.
pub(super) struct Listening {
    pub work: Work,
    started: Instant,
    passages: usize,
    /// It changed meanwhile: it's read again, and written down after that.
    changed: bool,
}

/// The thread that writes down speech, through the speech helper, and the files it's on and has waiting.
pub struct Speech {
    /// Files to write down (by id), from where they got to.
    files: SyncSender<(i64, PathBuf, f64)>,
    /// What it heard: a stretch at a time, then how the file ended.
    heard: Receiver<Heard>,
    /// The file it's to stop after the stretch it's on (by id), or EVERY.
    halt: Arc<AtomicI64>,
    /// It gives the helper back when it's done, unless the helper stopped.
    thread: JoinHandle<Option<Transcriber>>,
    /// The file it's on, then the one waiting.
    listening: VecDeque<Listening>,
    /// It stopped: nothing more goes to it in this run.
    pub over: bool,
}

enum Heard {
    /// The words heard from `start` to `end` seconds.
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
    /// The thread. It starts the helper for its first file.
    pub fn start(doorbell: SyncSender<()>) -> Self {
        let (files, to_do) = sync_channel::<(i64, PathBuf, f64)>(1);
        let (sender, heard) = sync_channel::<Heard>(AHEAD);
        let halt = Arc::new(AtomicI64::new(0));
        let halted = halt.clone();
        let thread = std::thread::spawn(move || {
            let mut helper: Option<Transcriber> = None;
            let tell = |heard: Heard| {
                let told = sender.send(heard).is_ok();
                let _ = doorbell.try_send(());
                told
            };
            while let Ok((id, path, from)) = to_do.recv() {
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
                    speech::transcribe(transcriber, &path, from, &mut |start, end, spoken| {
                        let stop = halted.load(Ordering::Relaxed);
                        Ok(tell(Heard::Stretch { start, end, spoken }) && stop != id && stop != EVERY)
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
        Self { files, heard, halt, thread, listening: VecDeque::new(), over: false }
    }

    /// Whether it takes another file: one waits beside the one it's on.
    pub fn free(&self) -> bool {
        self.listening.len() < 2 && !self.over
    }

    /// Whether it's on a file.
    pub fn busy(&self) -> bool {
        !self.listening.is_empty()
    }

    /// Whether it's on the speech of this file, or has it waiting.
    pub fn has(&self, file: i64) -> bool {
        self.listening.iter().any(|listening| listening.work.file.id == file)
    }

    /// The files it's on and has waiting.
    pub fn files(&self) -> impl Iterator<Item = &Work> {
        self.listening.iter().map(|listening| &listening.work)
    }
}

impl Indexer {
    /// Gives the speech lane a file to write down, from where it got to. One without sound is done at once.
    pub(super) fn listen(&mut self, speech: &mut Speech, work: Work, on_event: &mut dyn FnMut(Event)) -> Result<()> {
        if !crate::adapters::macos::av::has_audio(&work.file.path) {
            let done = Done { done: work.amount, finished: true, ..Default::default() };
            self.index.write_item(&work, &[], &done)?;
            return Ok(());
        }
        on_event(Event::Transcribing {
            path: work.file.path.display().to_string(),
            at: work.done,
            duration: work.amount,
        });
        if speech.files.send((work.file.id, work.file.path.clone(), work.done)).is_ok() {
            speech.listening.push_back(Listening { work, started: Instant::now(), passages: 0, changed: false });
        } else {
            speech.over = true;
        }
        Ok(())
    }

    /// Stores a stretch of what's said in a file: its passages, for the keyword index and the text round. False when
    /// the file changed meanwhile.
    fn store_heard(
        &mut self,
        listening: &mut Listening,
        end: f64,
        spoken: Vec<Spoken>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<bool> {
        let segments: Vec<NewSegment> = segment::speech::passages(spoken)
            .into_iter()
            .map(|p| NewSegment {
                segment: protocol::Segment::Speech,
                loc: Some(p.start),
                loc_end: Some(p.end),
                text: Some(p.text),
                due: Some(Round::Text),
                ..Default::default()
            })
            .collect();
        listening.passages += segments.len();
        let done = Done { done: end, ..Default::default() };
        if !self.index.write_item(&listening.work, &segments, &done)? {
            listening.changed = true;
            return Ok(false);
        }
        listening.work.done = end;
        let path = listening.work.file.path.display().to_string();
        on_event(Event::Transcribing { path, at: end, duration: listening.work.amount });
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
        let path = listening.work.file.path.display().to_string();
        let kind = listening.work.file.kind;
        match outcome {
            Ok(Outcome::Finished(end)) if !listening.changed => {
                let done = Done { done: end, finished: true, ..Default::default() };
                if self.index.write_item(&listening.work, &[], &done)? {
                    on_event(Event::File {
                        round: Round::Speech,
                        path,
                        kind,
                        segments: listening.passages,
                        vectors: 0,
                        ms: listening.started.elapsed().as_millis() as u64,
                    });
                }
            }
            Ok(_) => {}
            Err(error) if error.is::<Gone>() => return Err(error),
            Err(error) => {
                let error = format!("{error:#}");
                if self.index.fail(&listening.work, &error, false)? {
                    on_event(Event::Failed { round: Round::Speech, path, error });
                }
            }
        }
        Ok(())
    }

    /// Stores what the speech lane heard. Returns how many seconds of sound it wrote down.
    pub(super) fn heard(&mut self, speech: &mut Speech, on_event: &mut dyn FnMut(Event)) -> Result<f64> {
        let mut seconds = 0.0;
        while let Ok(heard) = speech.heard.try_recv() {
            match heard {
                Heard::Stretch { start, end, spoken } => {
                    let listening = speech.listening.front_mut().expect("a stretch of the file it's on");
                    if !self.store_heard(listening, end, spoken, on_event)? {
                        // It changed: the thread stops it after the stretch it's on.
                        speech.halt.store(listening.work.file.id, Ordering::Relaxed);
                    }
                    seconds += end - start;
                }
                Heard::Ended(outcome) => {
                    let listening = speech.listening.pop_front().expect("the file it's on");
                    // The thread is done with it: if it was told to stop it, a new read of it goes on.
                    let id = listening.work.file.id;
                    let _ = speech.halt.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
                    if let Err(error) = self.end_listening(listening, outcome, on_event) {
                        if !error.is::<Gone>() {
                            return Err(error);
                        }
                        speech.over = true;
                        on_event(Event::SpeechStopped { error: format!("{error:#}") });
                    }
                }
                Heard::Unavailable(error) => {
                    speech.listening.clear();
                    speech.over = true;
                    on_event(Event::SpeechStopped { error });
                }
            }
        }
        Ok(seconds)
    }

    /// Stops the speech lane once the stretches it has are stored, and lets the helper go (its memory with it). A file
    /// that was waiting goes on from where it was, later.
    pub(super) fn finish_listening(&mut self, speech: Speech, on_event: &mut dyn FnMut(Event)) -> Result<()> {
        let Speech { files, heard, halt, thread, mut listening, .. } = speech;
        halt.store(EVERY, Ordering::Relaxed);
        drop(files);
        let finished = (|| -> Result<()> {
            while let Some(current) = listening.front_mut() {
                match heard.recv() {
                    Ok(Heard::Stretch { end, spoken, .. }) => {
                        self.store_heard(current, end, spoken, on_event)?;
                    }
                    Ok(Heard::Ended(outcome)) => {
                        let ended = listening.pop_front().expect("the file it's on");
                        if let Err(error) = self.end_listening(ended, outcome, on_event)
                            && !error.is::<Gone>()
                        {
                            return Err(error);
                        }
                    }
                    Ok(Heard::Unavailable(_)) | Err(_) => listening.clear(),
                }
            }
            Ok(())
        })();
        // Without its channels, a thread still waiting stops at once.
        drop(heard);
        drop(thread.join());
        finished
    }
}
