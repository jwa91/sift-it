//! Scheduling: the run that joins the rounds. It asks resources for a plan (which rounds run side by side), and hands
//! their items to the lanes that do them:
//!
//! - the words lane: a few readers (as many as the plan says) reading files' words.
//! - the media lane: as many again, decoding for the GPU's round: pictures and frames, or sound.
//! - the OCR lane: one thread, since Vision reads text in one picture at a time per process anyway.
//! - the speech lane (listen.rs).
//! - this thread: the GPU (embed.rs, the model's owner), the text round's batches, and the writer (write.rs).
//!
//! Each lane's thread has a channel of its own, and they're taken from in turn, so an item with many pieces can't
//! keep a short one waiting behind it. The queues are the index's: this thread hands out a few items at a time,
//! new files' first, and makes the plan again as what it depends on changes: the work waiting, the power, the memory
//! pressure. A round that leaves the plan finishes the items it has in hand, then its tool goes; the GPU's next round
//! loads its encoder then. Files that appear meanwhile (`changes`) are synced as new files: they go first in every
//! round they need, so a new screenshot is searchable in seconds even while a long video is read.
//!
//! A lane that hands over nothing for STUCK is stuck in a framework call that won't return, and no thread can be
//! stopped from outside: its file is set aside, and the run ends with `Stuck`, for a new process to go on.

use super::listen::Speech;
use super::progress::Progress;
use super::read::{Ended, From};
use super::write::{Items, Key};
use super::{Event, Indexer, Piece, discover, prepare};
use crate::config::{Config, Reading};
use crate::index::{DueText, Work};
use crate::resources::{Conditions, Plan, Pressure, Queue, Tool, Wait};
use anyhow::{Result, anyhow};
use protocol::{Kind, Round, Segment};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Passages the text round embeds in one model call, at most.
const BATCH: usize = 16;

/// The plan is made again at least this often (power and memory pressure change on their own).
const REPLAN: Duration = Duration::from_secs(1);

/// How far each round is goes out at most this often.
const PROGRESS: Duration = Duration::from_secs(1);

/// What the lanes hand over is taken for at most about this long before the text round's next batch.
const RECEIVING: Duration = Duration::from_millis(100);

/// A light lane's thread may run this many messages ahead of the run's thread, which takes as many at a time at most.
const LIGHT: usize = 256;

/// A lane that hands over nothing for this long is stuck. Vision's text recognition can be: right after it compiles
/// its models for the Neural Engine (once per binary, the first time it reads text), it sometimes waits forever, and
/// cancelling the request doesn't help. That compile is also the longest real wait: almost five minutes for one
/// picture on a Mac under memory pressure. Time asleep doesn't count: Instant stands still while the Mac sleeps.
const STUCK: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(600) };

/// What the plan follows as it changes: the power and the memory pressure, and the tools that aren't there (a model
/// not downloaded, a speech helper that can't start).
pub struct Facts {
    pub conditions: Conditions,
    pub missing: Vec<Tool>,
}

/// How a run ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Nothing is left to do.
    Done,
    /// The work left waits: for a charger, memory, a model.
    Waits(Wait),
    /// `stop` said so: what isn't finished stays queued.
    Stopped,
}

/// A lane got stuck (STUCK) on this file. It's set aside (tried again later), and the run ends: the thread can't be
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

/// What a lane is handed: a round's work for a file, where in it to start, and whether to stop at its next checkpoint.
struct Item {
    work: Work,
    from: From,
    stop: Arc<AtomicBool>,
}

enum Message {
    /// The first message about an item: when its reading started.
    Start(Key, Instant),
    Piece(Key, Piece),
    /// How far a long item has got.
    Checkpoint(Key, Ended),
    Done(Key, Result<Ended>),
}

/// Items waiting for a lane's threads, and how many may be at work at once.
#[derive(Default)]
struct Feed {
    items: VecDeque<Item>,
    allowed: usize,
    working: usize,
    over: bool,
}

/// The file a lane's thread is on, and since when it's been at it without handing over a piece.
type Busy = Arc<Mutex<Option<(Key, Instant)>>>;

/// Threads that read items, from a feed of their own.
struct Lane {
    feed: Arc<(Mutex<Feed>, Condvar)>,
    receivers: Vec<Receiver<Message>>,
    busy: Vec<Busy>,
    turn: usize,
    /// What it hands over needs nothing of the GPU (words, OCR): it's taken between any two pieces that do.
    light: bool,
    /// Items kept waiting for each thread that may work, so none waits for the run's thread to hand out the next.
    ahead: usize,
}

impl Lane {
    /// The words lane: items often done at once (a photo's words), so many wait.
    fn words(threads: usize, reading: Arc<Reading>, doorbell: &SyncSender<()>) -> Self {
        Self::start(threads, true, 16, reading, doorbell)
    }

    /// The media lane: what it hands over is for the GPU.
    fn media(threads: usize, reading: Arc<Reading>, doorbell: &SyncSender<()>) -> Self {
        Self::start(threads, false, 2, reading, doorbell)
    }

    /// The OCR lane: one thread, as Vision reads text in one picture at a time anyway.
    fn ocr(reading: Arc<Reading>, doorbell: &SyncSender<()>) -> Self {
        Self::start(1, true, 2, reading, doorbell)
    }

    fn start(threads: usize, light: bool, ahead: usize, reading: Arc<Reading>, doorbell: &SyncSender<()>) -> Self {
        let feed = Arc::new((Mutex::new(Feed { allowed: threads, ..Default::default() }), Condvar::new()));
        let mut lane = Lane { feed: feed.clone(), receivers: Vec::new(), busy: Vec::new(), turn: 0, light, ahead };
        for _ in 0..threads {
            // A light lane's thread may run well ahead (passages are small), a media lane's a few pieces (pictures and
            // windows of sound are large).
            let (sender, receiver) = sync_channel::<Message>(if light { LIGHT } else { 4 });
            let busy: Busy = Arc::new(Mutex::new(None));
            lane.receivers.push(receiver);
            lane.busy.push(busy.clone());
            let (feed, doorbell, reading) = (feed.clone(), doorbell.clone(), reading.clone());
            std::thread::spawn(move || read_items(&feed, &sender, &doorbell, &busy, &reading));
        }
        lane
    }

    /// Lets `allowed` of its threads work at once.
    fn allow(&self, allowed: usize) {
        self.feed.0.lock().unwrap().allowed = allowed;
        self.feed.1.notify_all();
    }

    /// How many more items it can take.
    fn room(&self) -> usize {
        let feed = self.feed.0.lock().unwrap();
        (self.ahead * feed.allowed).saturating_sub(feed.items.len())
    }

    /// Hands it an item: a new file's goes first.
    fn give(&self, item: Item) {
        let mut feed = self.feed.0.lock().unwrap();
        if item.work.new {
            feed.items.push_front(item);
        } else {
            feed.items.push_back(item);
        }
        self.feed.1.notify_one();
    }

    /// Takes back the items not started that `drop` says, and returns their keys.
    fn take_back(&self, drop: impl Fn(&Item) -> bool) -> Vec<Key> {
        let mut feed = self.feed.0.lock().unwrap();
        let mut taken = Vec::new();
        feed.items.retain(|item| {
            let dropped = drop(item);
            if dropped {
                taken.push((item.work.file.id, item.work.round));
            }
            !dropped
        });
        taken
    }

    /// One message from each thread that has one, in turn; from a light lane, all that are there, up to LIGHT a thread.
    fn receive(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        for offset in 0..self.receivers.len() {
            let at = (self.turn + offset) % self.receivers.len();
            let take = if self.light { LIGHT } else { 1 };
            messages.extend(self.receivers[at].try_iter().take(take));
        }
        self.turn += 1;
        messages
    }

    /// The item a thread has been on for STUCK without handing over anything, if one has.
    fn stuck(&self) -> Option<Key> {
        self.busy
            .iter()
            .find_map(|busy| busy.lock().unwrap().filter(|(_, since)| since.elapsed() >= STUCK))
            .map(|b| b.0)
    }
}

impl Drop for Lane {
    /// Nothing waits for its threads: each stops at its next piece (its channel is gone) or when it finds the feed
    /// over. One stuck in a reader never would.
    fn drop(&mut self) {
        self.feed.0.lock().unwrap().over = true;
        self.feed.1.notify_all();
    }
}

/// A lane's thread: the items of its feed, read with this run's values. An item of a file queued again meanwhile isn't
/// written (Writer::as_handed_out).
fn read_items(
    feed: &(Mutex<Feed>, Condvar),
    sender: &SyncSender<Message>,
    doorbell: &SyncSender<()>,
    busy: &Busy,
    reading: &Reading,
) {
    let send = |message: Message| -> Result<()> {
        sender.send(message).map_err(|_| anyhow!("indexing stopped"))?;
        let _ = doorbell.try_send(());
        Ok(())
    };
    let mark = |key: Option<Key>| *busy.lock().unwrap() = key.map(|key| (key, Instant::now()));
    loop {
        let item = {
            let mut waiting = feed.0.lock().unwrap();
            loop {
                if waiting.over {
                    return;
                }
                if waiting.working < waiting.allowed
                    && let Some(item) = waiting.items.pop_front()
                {
                    waiting.working += 1;
                    break item;
                }
                waiting = feed.1.wait(waiting).unwrap();
            }
        };
        let (work, key) = (&item.work, (item.work.file.id, item.work.round));
        let settings = reading.resolve(&work.file.path, work.file.kind);
        let started = send(Message::Start(key, Instant::now())).is_ok();
        let result = started.then(|| {
            mark(Some(key));
            // A reader that panics (a file no one expected) is an item that couldn't be read, not a lost run.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                objc2::rc::autoreleasepool(|_| {
                    super::read::read(
                        work.round,
                        &work.file.path,
                        work.file.kind,
                        &settings,
                        &item.from,
                        &mut |piece| {
                            // While a piece waits for the run to take it, the reader isn't the one stuck.
                            mark(None);
                            let sent = send(Message::Piece(key, piece));
                            mark(Some(key));
                            sent
                        },
                        &mut |at| {
                            mark(None);
                            let sent = send(Message::Checkpoint(key, at.clone()));
                            mark(Some(key));
                            sent.map(|_| !item.stop.load(Ordering::Relaxed))
                        },
                    )
                })
            }))
            .unwrap_or_else(|_| Err(anyhow!("the reader crashed on it")));
            mark(None);
            result
        });
        feed.0.lock().unwrap().working -= 1;
        if result.is_none_or(|result| send(Message::Done(key, result)).is_err()) {
            return;
        }
    }
}

/// The lane a round's items go to.
fn lane_of(round: Round) -> Option<usize> {
    match round {
        Round::Words => Some(WORDS),
        Round::Pictures | Round::Sound => Some(MEDIA),
        Round::Ocr => Some(OCR),
        Round::Text | Round::Speech => None,
    }
}

const WORDS: usize = 0;
const MEDIA: usize = 1;
const OCR: usize = 2;

impl Indexer {
    /// Indexes the queued work, round by round as the plan says (resources::Plan), until there's nothing left that can
    /// run: everything is done, or what's left waits (for a charger, memory, a model). `facts` says what the plan
    /// follows, as it changes. The paths `changes` brings in meanwhile are synced, and what they queue goes first.
    /// Stops between items when `stop` says so; what isn't finished stays queued. `progress` keeps how fast each round
    /// went while it ran, across runs.
    pub fn run(
        &mut self,
        config: &Config,
        facts: &mut dyn FnMut() -> Facts,
        progress: &mut Progress,
        on_event: &mut dyn FnMut(Event),
        stop: &dyn Fn() -> bool,
        changes: &mut dyn FnMut() -> Vec<PathBuf>,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let reading = Arc::new(config.reading.clone());
        let (doorbell, door) = sync_channel::<()>(1);
        let most_readers = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 3).clamp(2, 4);
        let mut lanes = [
            Lane::words(most_readers, reading.clone(), &doorbell),
            Lane::media(most_readers, reading.clone(), &doorbell),
            Lane::ocr(reading, &doorbell),
        ];
        let mut run = Run {
            plan: Plan::default(),
            planned: None,
            reported: None,
            in_hand: HashMap::new(),
            items: Items::default(),
            gpu: None,
            text_waits: false,
            speech: None,
            speech_down: false,
            missing: Vec::new(),
        };
        let outcome = (|| -> Result<Outcome> {
            loop {
                if stop() {
                    return Ok(Outcome::Stopped);
                }
                let changed = changes();
                if !changed.is_empty() {
                    on_event(discover::sync_paths(&self.index, config, &changed)?);
                    run.planned = None;
                }
                if run.planned.is_none_or(|at| at.elapsed() >= REPLAN) {
                    let facts = facts();
                    if facts.conditions.pressure == Pressure::Critical {
                        // Everything lets go at once: what was in hand is read again later.
                        return Ok(Outcome::Waits(Wait::Memory));
                    }
                    run.missing = facts.missing;
                    if run.speech_down {
                        run.missing.push(Tool::SpeechModel);
                    }
                    let queues: Vec<Queue> =
                        self.index.queues()?.into_iter().map(|(round, new, old)| Queue { round, new, old }).collect();
                    let plan = Plan::of(&config.indexing, facts.conditions, &queues, &run.missing);
                    if shape(&plan) != shape(&run.plan) && !plan.runs.is_empty() {
                        let rounds = plan.runs.iter().map(|running| running.round).collect();
                        on_event(Event::Plan { rounds, readers: plan.readers, waiting: plan.waiting() });
                    }
                    lanes[WORDS].allow(plan.readers);
                    lanes[MEDIA].allow(plan.readers);
                    progress.follow(&plan, Instant::now());
                    run.plan = plan;
                    run.planned = Some(Instant::now());
                }
                if let Some(key) = lanes.iter().find_map(Lane::stuck) {
                    let work = run.in_hand[&key].0.clone();
                    let error = format!("reading it got stuck: nothing came of it in {} minutes", STUCK.as_secs() / 60);
                    if self.index.fail(&work, &error, true)? {
                        on_event(Event::Failed {
                            round: work.round,
                            path: work.file.path.display().to_string(),
                            error,
                        });
                    }
                    return Err(Stuck(work.file.path).into());
                }
                self.switch_gpu(&mut run, &lanes[MEDIA]);
                let handed = self.hand_out(&mut run, &lanes)?;
                let listened = self.speech_lane(&mut run, &doorbell, progress, on_event)?;
                // What the media lane handed over, a piece from each of its threads in turn, for as long as there's
                // more and it takes less than a slice of the GPU's time, so the text round's batches go in between
                // pictures and sound. What the light lanes hand over is taken between any two of its pieces.
                let mut received = false;
                let receiving = Instant::now();
                let [words, media, ocr] = &mut lanes;
                // The text round comes before pictures and sound on the GPU, unless theirs is new files' work.
                let media_first = run.plan.runs.iter().position(|r| Some(r.round) == run.gpu)
                    < run.plan.runs.iter().position(|r| r.round == Round::Text);
                let media_waits = run.text_waits && !media_first;
                loop {
                    let mut more = self.light(&mut run, [&mut *words, &mut *ocr], progress, on_event)?;
                    for message in if media_waits { Vec::new() } else { media.receive() } {
                        more = true;
                        self.handle(message, &mut run, progress, on_event)?;
                        self.light(&mut run, [&mut *words, &mut *ocr], progress, on_event)?;
                    }
                    received |= more;
                    if !more || receiving.elapsed() >= RECEIVING {
                        break;
                    }
                }
                let embedded = match run.plan.runs(Round::Text) {
                    Some(text) => self.text_batch(text.new_only, progress, on_event)?,
                    None => 0,
                };
                // A whole batch: there's likely more waiting.
                run.text_waits = embedded == BATCH;
                let embedded = embedded > 0;
                if run.reported.is_none_or(|at: Instant| at.elapsed() >= PROGRESS) {
                    let rounds = progress.status(&self.index.rounds()?, &run.plan, Instant::now());
                    on_event(Event::Rounds { rounds });
                    run.reported = Some(Instant::now());
                }
                let busy = !run.in_hand.is_empty() || run.speech.as_ref().is_some_and(Speech::busy);
                if !(handed || listened || received || embedded || busy) {
                    if run.plan.runs.is_empty() {
                        return Ok(match run.plan.waits_for() {
                            Some(wait) => Outcome::Waits(wait),
                            None if self.index.queues()?.is_empty() => Outcome::Done,
                            None => Outcome::Waits(Wait::Turn),
                        });
                    }
                    // The plan may be out of date: what it runs has nothing to hand out.
                    run.planned = None;
                }
                if !(received || embedded) {
                    let wait = Duration::from_millis(if busy { 500 } else { 50 });
                    if let Err(RecvTimeoutError::Disconnected) = door.recv_timeout(wait) {
                        unreachable!("the run holds a doorbell");
                    }
                }
            }
        })();
        drop(lanes);
        // The pictures and sound in hand are dropped: their items are read again later. The encoders go, and so do
        // the rounds' clocks, until they run again.
        self.embed.release_encoders();
        progress.follow(&Plan::default(), Instant::now());
        let outcome = match run.speech.take() {
            // The speech lane stops after the stretch it's on, which is stored: the file goes on from there.
            Some(speech) => outcome.and_then(|outcome| self.finish_listening(speech, on_event).map(|_| outcome)),
            None => outcome,
        };
        if let Ok(rounds) = self.index.rounds() {
            on_event(Event::Rounds { rounds: progress.status(&rounds, &run.plan, Instant::now()) });
        }
        on_event(Event::Finished {
            files: run.items.files,
            failed: run.items.failed,
            seconds: started.elapsed().as_secs_f64(),
            stopped: matches!(outcome, Ok(Outcome::Stopped)),
        });
        outcome
    }

    /// The GPU's media round, as the plan says: once the one before has no items in hand, its encoder goes, and the
    /// next one's loads with its first piece.
    fn switch_gpu(&mut self, run: &mut Run, readers: &Lane) {
        let wanted =
            run.plan.runs.iter().map(|running| running.round).find(|r| matches!(r, Round::Pictures | Round::Sound));
        if run.gpu == wanted {
            return;
        }
        if let Some(gpu) = run.gpu {
            for key in readers.take_back(|item| item.work.round == gpu) {
                run.in_hand.remove(&key);
            }
            // What it has in hand stops at its next checkpoint.
            run.stop(|round| round == gpu);
            if run.in_hand.keys().any(|(_, round)| *round == gpu) {
                return;
            }
            self.embed.release_encoders();
        }
        run.gpu = wanted;
    }

    /// Hands the lanes the next items of the rounds the plan runs on them, new files' first. True when it handed out
    /// any.
    fn hand_out(&mut self, run: &mut Run, lanes: &[Lane; 3]) -> Result<bool> {
        let mut handed = false;
        // Items of rounds that left the plan come back, or stop at their next checkpoint.
        for lane in lanes {
            let plan = &run.plan;
            for key in lane.take_back(|item| plan.runs(item.work.round).is_none()) {
                run.in_hand.remove(&key);
            }
        }
        let plan = run.plan.clone();
        run.stop(|round| plan.runs(round).is_none());
        let runs: Vec<_> = run.plan.runs.clone();
        for running in runs {
            let Some(at) = lane_of(running.round) else { continue };
            if matches!(running.round, Round::Pictures | Round::Sound) && run.gpu != Some(running.round) {
                continue;
            }
            let lane = &lanes[at];
            let room = lane.room();
            if room == 0 {
                continue;
            }
            // Only one item of a file's round is in hand at a time: its items go in order.
            let mut next = self.index.next(running.round, running.new_only, room + run.in_hand.len())?;
            next.retain(|work| !run.in_hand.contains_key(&(work.file.id, work.round)));
            for work in next.into_iter().take(room) {
                let pages = if work.round == Round::Pictures && work.file.kind == Kind::Document {
                    self.index.due_pictures(work.file.id)?
                } else {
                    Vec::new()
                };
                let from = From { done: work.done, resume: work.resume.clone(), pages };
                let stop = Arc::new(AtomicBool::new(false));
                run.in_hand.insert((work.file.id, work.round), (work.clone(), stop.clone()));
                lane.give(Item { work, from, stop });
                handed = true;
            }
        }
        Ok(handed)
    }

    /// The speech lane, as the plan says: started for its first file, and stopped (its helper with it) when speech
    /// leaves the plan or its helper does. True when anything happened.
    fn speech_lane(
        &mut self,
        run: &mut Run,
        doorbell: &SyncSender<()>,
        progress: &mut Progress,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<bool> {
        if run.plan.runs(Round::Speech).is_none() {
            if let Some(lane) = run.speech.take() {
                self.finish_listening(lane, on_event)?;
                return Ok(true);
            }
            return Ok(false);
        }
        run.speech.get_or_insert_with(|| Speech::start(doorbell.clone()));
        let listened = self.listen_on(run, progress, on_event)?;
        if run.speech.as_ref().is_some_and(|lane| lane.over) {
            // The helper couldn't start, or it stopped: speech waits for it, and the rest goes on.
            run.speech_down = true;
            run.planned = None;
            if let Some(lane) = run.speech.take() {
                self.finish_listening(lane, on_event)?;
            }
            return Ok(true);
        }
        Ok(listened)
    }

    /// The speech lane, while it runs: what it heard is stored, and when it's free, it's handed its next file at once
    /// (a short recording is written down in a fraction of a second). True when anything happened.
    fn listen_on(&mut self, run: &mut Run, progress: &mut Progress, on_event: &mut dyn FnMut(Event)) -> Result<bool> {
        let Some(lane) = run.speech.as_mut() else { return Ok(false) };
        let seconds = self.heard(lane, on_event)?;
        progress.did(Round::Speech, seconds);
        let Some(speech) = run.plan.runs(Round::Speech).filter(|_| lane.free()) else { return Ok(seconds > 0.0) };
        let next = self.index.next(Round::Speech, speech.new_only, 3)?.into_iter().find(|work| !lane.has(work.file.id));
        let Some(work) = next else { return Ok(seconds > 0.0) };
        self.listen(lane, work, on_event)?;
        Ok(true)
    }

    /// What the light lanes handed over: words', OCR's, and the speech lane's stretches. Their threads mustn't wait on
    /// a picture's embedding to hand over a file's words, a screenshot's text or what's said: each of their many small
    /// messages would wait as long again. True when there was any.
    fn light(
        &mut self,
        run: &mut Run,
        lanes: [&mut Lane; 2],
        progress: &mut Progress,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<bool> {
        let mut any = false;
        for lane in lanes {
            for message in lane.receive() {
                any = true;
                self.handle(message, run, progress, on_event)?;
            }
        }
        any |= self.listen_on(run, progress, on_event)?;
        Ok(any)
    }

    fn handle(
        &mut self,
        message: Message,
        run: &mut Run,
        progress: &mut Progress,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        match message {
            Message::Start(key, started) => {
                if let Some((work, _)) = run.in_hand.get(&key) {
                    run.items.start(work.clone(), started);
                }
            }
            Message::Checkpoint(key, at) => {
                if let Some(units) = run.items.checkpoint(key, &at, &mut self.index)? {
                    progress.did(key.1, units);
                }
            }
            Message::Piece(key, piece) => {
                if !run.items.keeps(key) {
                    return Ok(());
                }
                let (segment, failed) = self.embed.one(piece)?;
                run.items.add(key, segment, failed);
                if key.1 == Round::Words {
                    run.items.write_part(key, &mut self.index)?;
                }
            }
            Message::Done(key, result) => {
                if run.in_hand.remove(&key).is_none() {
                    return Ok(());
                }
                if let Some(units) = run.items.end(key, result, &mut self.index, on_event)? {
                    progress.did(key.1, units);
                }
                // What it wrote may bring work to other rounds.
                run.planned = None;
            }
        }
        Ok(())
    }

    /// The text round: one batch of the passages waiting for their vectors. Returns how many there were.
    fn text_batch(
        &mut self,
        new_only: bool,
        progress: &mut Progress,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<usize> {
        let due = self.index.due_text(new_only, BATCH)?;
        if due.is_empty() {
            return Ok(0);
        }
        let mut titles: HashMap<i64, String> = HashMap::new();
        let prompts: Vec<String> = due
            .iter()
            .map(|passage| {
                let title = titles
                    .entry(passage.file.id)
                    .or_insert_with(|| prepare::title(&passage.file.path, passage.file.kind));
                // What's said in a recording is embedded with its title, when it has one.
                let title = match (&passage.title, passage.segment) {
                    (Some(tag), Segment::Speech) => tag.as_str(),
                    _ => title.as_str(),
                };
                prepare::prompt(title, passage.context.as_deref(), &passage.text)
            })
            .collect();
        match self.embed.texts(&prompts.iter().map(String::as_str).collect::<Vec<_>>())? {
            Some(vectors) if vectors.len() == due.len() => {
                let embedded: Vec<(&DueText, Vec<f32>)> = due.iter().zip(vectors).collect();
                self.index.write_text(&embedded)?;
                progress.did(Round::Text, due.len() as f64);
            }
            _ => {
                // It may well pass: these files' text is tried again later.
                let files: HashSet<i64> = due.iter().map(|passage| passage.file.id).collect();
                for file in files {
                    let file = due.iter().find(|passage| passage.file.id == file).expect("one of them").file.clone();
                    let path = file.path.display().to_string();
                    let work = Work { file, round: Round::Text, amount: 0.0, done: 0.0, resume: None, new: false };
                    let error = "its passages couldn't be embedded".to_string();
                    if self.index.fail(&work, &error, true)? {
                        on_event(Event::Failed { round: Round::Text, path, error });
                    }
                }
            }
        }
        Ok(due.len())
    }
}

/// What a plan loads and runs, for saying when it changes: the text round comes and goes with the passages waiting,
/// and shares the GPU's model with pictures and sound, so it doesn't count.
fn shape(plan: &Plan) -> (Vec<Round>, usize, Option<Wait>) {
    let rounds = plan.runs.iter().map(|running| running.round).filter(|round| *round != Round::Text).collect();
    (rounds, plan.readers, plan.waiting())
}

impl Run {
    /// Stops the items in hand of the rounds `which` says at their next checkpoint.
    fn stop(&self, which: impl Fn(Round) -> bool) {
        for ((_, round), (_, stop)) in &self.in_hand {
            if which(*round) {
                stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// A run's state.
struct Run {
    plan: Plan,
    /// When the plan was made: None to make it again now.
    planned: Option<Instant>,
    /// When progress last went out.
    reported: Option<Instant>,
    /// The items handed out and not finished, by file and round, with what stops them at their next checkpoint.
    in_hand: HashMap<Key, (Work, Arc<AtomicBool>)>,
    items: Items,
    /// The GPU's media round: the one whose items the readers get, and whose encoder is loaded or loads.
    gpu: Option<Round>,
    /// The text round's last batch was whole: more passages wait, and go before pictures and sound.
    text_waits: bool,
    speech: Option<Speech>,
    /// The speech helper couldn't start or stopped in this run: speech waits for it.
    speech_down: bool,
    missing: Vec<Tool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Depth;
    use crate::index::Writer;
    use crate::ingest::discover::crawl::Found;
    use crate::resources::Power;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn calm() -> Facts {
        Facts { conditions: Conditions { power: Power::Charger, pressure: Pressure::Normal }, missing: Vec::new() }
    }

    /// A lane that never hands over anything (a pipe no one writes to: opening it waits for a writer) gets its file
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
        let mut indexer = Indexer::new(index).unwrap();
        let mut failed = Vec::new();
        let mut events = |event| {
            if let Event::Failed { path, .. } = event {
                failed.push(path);
            }
        };
        let mut progress = Progress::default();
        let outcome = indexer.run(&config, &mut calm, &mut progress, &mut events, &|| false, &mut Vec::new);

        assert!(outcome.unwrap_err().is::<Stuck>());
        assert_eq!(failed, [stuck.display().to_string()]);
        let state = |path: &Path| indexer.index.work_state(path, Round::Words);
        assert_eq!(state(&stuck), "failed");
        assert_eq!(state(&fine), "done");
        // It's tried again later.
        assert_eq!(indexer.index.requeue_passing(0.0).unwrap(), 1);
        assert_eq!(state(&stuck), "pending");

        // A writer lets the reader go: it finds the run over.
        drop(std::fs::OpenOptions::new().write(true).open(&stuck).unwrap());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// When memory is critically short, a run stops at once and says so, with its work still queued; on a warning it
    /// goes on, one tool at a time.
    #[test]
    fn a_run_stops_when_memory_is_critically_short() {
        let folder = std::env::temp_dir().join(format!("sift-it-test-pressure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut config = Config::default();
        config.reading.all.depth = Some(Depth::Words);
        let index = Writer::open(&folder.join("index.sqlite")).unwrap();
        let path = folder.join("note.txt");
        std::fs::write(&path, "the ferry leaves at noon").unwrap();
        let settings = config.reading.resolve(&path, Kind::Plaintext);
        index
            .queue(
                &Found { path: path.clone(), kind: Kind::Plaintext, size: 24, modified: 1.0, settings }.new_file(),
                None,
                0.0,
            )
            .unwrap();
        let mut indexer = Indexer::new(index).unwrap();
        let mut progress = Progress::default();
        let pressed = |pressure| {
            move || Facts { conditions: Conditions { power: Power::Charger, pressure }, missing: Vec::new() }
        };
        let outcome = indexer.run(
            &config,
            &mut pressed(Pressure::Critical),
            &mut progress,
            &mut |_| {},
            &|| false,
            &mut Vec::new,
        );
        assert_eq!(outcome.unwrap(), Outcome::Waits(Wait::Memory));
        assert_eq!(indexer.index.work_state(&path, Round::Words), "pending");
        let outcome =
            indexer.run(&config, &mut pressed(Pressure::Warning), &mut progress, &mut |_| {}, &|| false, &mut Vec::new);
        assert_eq!(outcome.unwrap(), Outcome::Done);
        assert_eq!(indexer.index.work_state(&path, Round::Words), "done");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// Words need no model: a run at `words` depth reads every file's words and is done.
    #[test]
    fn a_run_reads_words_without_a_model() {
        let folder = std::env::temp_dir().join(format!("sift-it-test-words-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut config = Config::default();
        config.reading.all.depth = Some(Depth::Words);
        let index = Writer::open(&folder.join("index.sqlite")).unwrap();
        for n in 0..5 {
            let path = folder.join(format!("note {n}.txt"));
            std::fs::write(&path, format!("note number {n}: the ferry leaves at noon")).unwrap();
            let settings = config.reading.resolve(&path, Kind::Plaintext);
            let found = Found { path, kind: Kind::Plaintext, size: 40, modified: 1.0, settings };
            index.queue(&found.new_file(), None, 0.0).unwrap();
        }
        let mut indexer = Indexer::new(index).unwrap();
        let mut progress = Progress::default();
        let outcome = indexer.run(&config, &mut calm, &mut progress, &mut |_| {}, &|| false, &mut Vec::new);
        assert_eq!(outcome.unwrap(), Outcome::Done);
        assert_eq!(indexer.index.stats().unwrap().done, 5);
        let mut found = 0;
        indexer
            .index
            .word_matches(&["ferry".into()], false, |_, _| {
                found += 1;
                true
            })
            .unwrap();
        assert_eq!(found, 5);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
