//! Listening: what's said in recordings and videos the reading is done with, written down on a lane of its own. The
//! speech extractor (extract/speech.rs) hands over a stretch of a file at a time; its sentences are cut into passages
//! (segment/speech.rs), embedded and stored as soon as they're back, so a file stopped partway goes on from there. When
//! memory allows (resources::IngestPlan), the lane runs beside the reading, on a thread of its own that talks to the speech helper;
//! else `transcribe_next` writes down one file at a time once everything is read.

use super::extract::speech::{self, Outcome};
use super::{Event, Indexer, prepare, segment};
use crate::index::{FileRow, NewSegment};
use crate::speech::{Gone, Spoken, Transcriber};
use anyhow::Result;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Instant;

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
pub struct Speech {
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
    /// Look for a file to write down: one was just read.
    pub fn look(&mut self) {
        self.look = true;
    }

    /// The thread, with `helper` when there is one already (else it starts one for its first file).
    pub fn start(mut helper: Option<Transcriber>, doorbell: SyncSender<()>) -> Self {
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
                    speech::transcribe(transcriber, &row.path, from, &mut |start, end, spoken| {
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

impl Indexer {
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
        let outcome = speech::transcribe(transcriber, &path, listening.from, &mut |start, end, spoken| {
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
                title: title.unwrap_or_else(|| prepare::title(&row.path, row.kind)),
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
        let passages = segment::speech::passages(spoken);
        let texts: Vec<String> = passages.iter().map(|p| prepare::spoken(&listening.title, &p.text)).collect();
        let mut vectors = self.embed.texts(&texts.iter().map(String::as_str).collect::<Vec<_>>())?.into_iter();
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
    pub(super) fn listen_beside(
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
                        .embed
                        .load_media()
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
    pub(super) fn finish_listening(
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
}
