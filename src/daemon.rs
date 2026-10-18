//! `sift-it daemon`: the background indexer. It keeps the index in step with the chosen folders: a full sync when it
//! starts (and every `indexing.full_sync_min`, in case an event was missed), then the paths macOS reports as changed
//! (FSEvents), a moment after they go quiet. It reads what's new, newest and cheapest first, and lets the model go
//! after `indexing.release_model_s` with nothing to do. A change to config.json counts at once, with a full sync.
//!
//! It writes down what's said in recordings and videos (ingest/listen.rs), a chunk at a time: beside the reading when
//! `indexing.memory_mb` allows both models (ingest::Plan), else once everything is read. Anything that changes in the
//! folders comes first, and the writing down goes on where it stopped.
//!
//! With nothing left to do, it writes the next generation of a vector partition that's due one (index/vectors.rs).
//!
//! It says what it's doing in status.json (for `sift-it status` and front ends), and listens to signals:
//!   SIGUSR1  look for changes now (a full sync)
//!   SIGUSR2  pause or resume
//!   SIGTERM  stop (what isn't finished stays queued)
//!
//! One runs at a time (daemon.lock). With `indexing.on_battery` off it waits for a charger; in Low Power Mode it always
//! waits. When a reader gets stuck (ingest::Stuck), it exits, and launchd starts a new one.

use crate::config::{self, Config};
use crate::index::{self, Writer};
use crate::ingest::{Event, Indexer, Plan, Stuck};
use crate::power;
use crate::speech::Transcriber;
use anyhow::{Context, Result, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{IndexerState, IndexerStatus};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime};

/// While waiting (battery, Low Power Mode), conditions are checked this often.
const WAIT_CHECK: Duration = Duration::from_secs(10);
/// A speech helper that couldn't start is tried again after this long.
const SPEECH_RETRY: Duration = Duration::from_secs(300);
/// Writing the vectors' next generation, when it failed (a full disk), is tried again after this long.
const COMPACT_RETRY: Duration = Duration::from_secs(3600);

static RESYNC: AtomicBool = AtomicBool::new(false);
static TOGGLE_PAUSE: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    match signal {
        libc::SIGUSR1 => RESYNC.store(true, Ordering::SeqCst),
        libc::SIGUSR2 => TOGGLE_PAUSE.store(true, Ordering::SeqCst),
        _ => SHUTDOWN.store(true, Ordering::SeqCst),
    }
}

fn install_signal_handlers() {
    for signal in [libc::SIGUSR1, libc::SIGUSR2, libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
}

/// Says what the daemon does in status.json, which the engine passes on (protocol::IndexerStatus).
struct Reporter {
    status: IndexerStatus,
    last_write: Instant,
}

impl Reporter {
    fn set(&mut self, state: IndexerState, detail: Option<String>) {
        if self.status.state != state || self.status.detail != detail {
            self.status.state = state;
            self.status.detail = detail;
            self.write(true);
        }
    }

    fn event(&mut self, event: &Event) {
        match event {
            Event::Sync { added, changed, removed, .. } if added + changed + removed > 0 => {
                log(&format!("sync: {added} new, {changed} changed, {removed} gone"));
            }
            Event::Sync { .. } => {}
            Event::Started { pending } => {
                self.status.done = 0;
                self.status.total = *pending;
                self.set(IndexerState::Indexing, None);
            }
            Event::Queued { count } => self.status.total += count,
            Event::File { path, .. } => {
                self.status.done += 1;
                self.status.current = Some(path.clone());
            }
            Event::Failed { path, error } => {
                self.status.done += 1;
                self.status.last_error = Some(format!("{}: {error}", file_name(path)));
                log(&format!("can't read {path}: {error}"));
            }
            Event::Finished { indexed, failed, seconds, stopped } => {
                log(&format!(
                    "indexed {indexed} files ({failed} unreadable) in {seconds:.1} s{}",
                    if *stopped { ", stopped early" } else { "" }
                ));
            }
            // Beside the reading, the reading's progress is what's shown.
            Event::Transcribing { path, at, duration } if self.status.state == IndexerState::Transcribing => {
                self.status.current = Some(path.clone());
                self.status.detail = Some(format!("{} of {}", clock(*at), clock(*duration)));
            }
            Event::Transcribing { .. } => {}
            Event::Transcribed { path, passages, seconds, ms } => {
                self.status.speech_done += 1;
                if self.status.state == IndexerState::Transcribing {
                    self.status.done = self.status.speech_done;
                }
                log(&format!(
                    "wrote down {}: {passages} passages, {seconds:.0} s of sound in {ms} ms",
                    file_name(path)
                ));
            }
            Event::TranscribeFailed { path, error } => {
                self.status.speech_done += 1;
                if self.status.state == IndexerState::Transcribing {
                    self.status.done = self.status.speech_done;
                }
                self.status.last_error = Some(format!("{}: {error}", file_name(path)));
                log(&format!("can't write down what's said in {path}: {error}"));
            }
            Event::SpeechStopped { error } => {
                log(&format!("writing down speech beside the reading stopped, it goes on after it: {error}"))
            }
        }
        self.write(false);
    }

    /// At most once a second, unless `now`.
    fn write(&mut self, now: bool) {
        if !now && self.last_write.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_write = Instant::now();
        self.status.updated = index::now();
        if let Ok(json) = serde_json::to_vec(&self.status) {
            let _ = config::write_atomically(&config::status_path(), &json);
        }
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// "12:34", or "1:02:03" past the hour.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn log(line: &str) {
    let now = chrono_like(SystemTime::now());
    eprintln!("{now} {line}");
}

/// "2026-10-14 12:34:56" in UTC, without a date library.
fn chrono_like(time: SystemTime) -> String {
    let seconds = time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Holds daemon.lock for as long as it lives: one indexer at a time.
struct Lock(#[allow(dead_code)] File);

fn lock() -> Result<Lock> {
    std::fs::create_dir_all(config::home())?;
    let file = File::options().create(true).truncate(false).write(true).open(lock_path())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another sift-it daemon is running (pid {})", running_pid().map(|p| p.to_string()).unwrap_or("?".into()));
    }
    std::fs::write(lock_path(), std::process::id().to_string())?;
    Ok(Lock(file))
}

fn lock_path() -> PathBuf {
    config::home().join("daemon.lock")
}

/// The running daemon's pid, if one holds the lock.
pub fn running_pid() -> Option<i32> {
    let file = File::options().write(true).open(lock_path()).ok()?;
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if free {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    std::fs::read_to_string(lock_path()).ok()?.trim().parse().ok()
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn watch(
    config: &Config,
    events: std::sync::mpsc::Sender<notify::Result<notify::Event>>,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(events)?;
    for folder in &config.sources.paths {
        if let Err(error) = watcher.watch(folder, RecursiveMode::Recursive) {
            log(&format!("can't watch {}: {error}", folder.display()));
        }
    }
    Ok(watcher)
}

/// Changed paths gathered from FSEvents, each handed over once it has been quiet a moment (a download still being
/// written doesn't hold back a screenshot that just landed).
struct Changes {
    receiver: Receiver<notify::Result<notify::Event>>,
    /// Each changed path, with when it last changed.
    paths: HashMap<PathBuf, Instant>,
    /// The chosen folders: what's below them decides what's worth a look.
    roots: Vec<PathBuf>,
    /// How long a path must be quiet before it's handed over.
    quiet: Duration,
    rescan: bool,
}

impl Changes {
    fn drain(&mut self) {
        while let Ok(event) = self.receiver.try_recv() {
            self.add(event);
        }
    }

    fn add(&mut self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event) if event.need_rescan() => self.rescan = true,
            Ok(event) => {
                for path in event.paths {
                    if self.worth_a_look(&path) {
                        self.paths.insert(path, Instant::now());
                    }
                }
            }
            Err(error) => {
                log(&format!("watch error: {error}"));
                self.rescan = true;
            }
        }
    }

    /// Below its chosen folder, a path in a hidden folder or a Library is never about an indexed file. (Only below:
    /// iCloud Drive and Dropbox live in ~/Library themselves.)
    fn worth_a_look(&self, path: &Path) -> bool {
        let Some(root) = self.roots.iter().filter(|root| path.starts_with(root)).max_by_key(|r| r.as_os_str().len())
        else {
            return false;
        };
        !path.strip_prefix(root).unwrap_or(path).components().any(|c| {
            let c = c.as_os_str().to_string_lossy();
            c.starts_with('.') || c == "Library"
        })
    }

    /// The paths that have settled, folded so a folder covers what's in it. The rest wait.
    fn take(&mut self) -> Vec<PathBuf> {
        self.drain();
        let mut settled: Vec<PathBuf> =
            self.paths.iter().filter(|(_, changed)| changed.elapsed() >= self.quiet).map(|(p, _)| p.clone()).collect();
        if settled.is_empty() {
            return Vec::new();
        }
        for path in &settled {
            self.paths.remove(path);
        }
        settled.sort();
        let mut folded: Vec<PathBuf> = Vec::new();
        for path in settled {
            if !folded.last().is_some_and(|last| path.starts_with(last)) {
                folded.push(path);
            }
        }
        folded
    }
}

/// Why indexing should wait now, if it should.
fn reason_to_wait(config: &Config) -> Option<String> {
    if power::low_power_mode() {
        return Some("Waiting: Low Power Mode is on".into());
    }
    if !config.indexing.on_battery && power::on_battery() {
        return Some("Waiting for a charger".into());
    }
    None
}

pub fn run() -> Result<()> {
    let _lock = lock()?;
    install_signal_handlers();
    let config_path = config::config_path();
    let mut config = Config::load()?;
    let mut config_time = modified(&config_path);
    trim_log(config.log.max_mb << 20);
    log(&format!("sift-it daemon started (pid {})", std::process::id()));
    let mut reporter = Reporter {
        status: IndexerStatus { pid: std::process::id(), ..Default::default() },
        last_write: Instant::now(),
    };
    reporter.write(true);

    let index = Writer::open(&config::index_path()).context("can't open the index")?;
    let mut indexer = Indexer::new(index, config::models_dir())?;
    let (sender, receiver) = channel();
    let mut watcher = watch(&config, sender.clone())?;
    let mut changes = Changes {
        receiver,
        paths: HashMap::new(),
        roots: config.sources.paths.clone(),
        quiet: Duration::from_millis(config.indexing.quiet_ms),
        rescan: true,
    };
    let mut last_full = Instant::now();
    let mut idle_since = Instant::now();
    let mut paused = false;
    let mut waiting_reason: Option<(String, Instant)> = None;
    // The speech model's helper, while there's speech to write down; why it can't run, when it can't.
    let mut transcriber: Option<Transcriber> = None;
    let mut speech_problem: Option<String> = None;
    // When the helper last failed to start: it's tried again a while later, not on every pass.
    let mut speech_failed_at: Option<Instant> = None;
    // When writing a partition's next generation last failed: the same.
    let mut compact_failed_at: Option<Instant> = None;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        // The config changed: watch the folders it names now, and sync it all (what's read, and how, may differ).
        if modified(&config_path) != config_time {
            config_time = modified(&config_path);
            match Config::load() {
                Ok(new) => {
                    config = new;
                    waiting_reason = None;
                    changes.roots = config.sources.paths.clone();
                    changes.quiet = Duration::from_millis(config.indexing.quiet_ms);
                    drop(watcher);
                    watcher = watch(&config, sender.clone())?;
                    changes.rescan = true;
                    log("the config changed");
                }
                Err(error) => log(&format!("can't read the config: {error:#}")),
            }
        }
        if TOGGLE_PAUSE.swap(false, Ordering::SeqCst) {
            paused = !paused;
            log(if paused { "paused" } else { "resumed" });
        }
        let full_sync_every = Duration::from_secs(config.indexing.full_sync_min * 60);
        if RESYNC.swap(false, Ordering::SeqCst) || last_full.elapsed() > full_sync_every {
            changes.rescan = true;
        }
        changes.drain();

        if changes.rescan {
            changes.rescan = false;
            changes.paths.clear();
            reporter.set(IndexerState::Syncing, None);
            match indexer.sync(&config) {
                Ok(event) => reporter.event(&event),
                Err(error) => log(&format!("sync failed: {error:#}")),
            }
            last_full = Instant::now();
        } else {
            let paths = changes.take();
            if !paths.is_empty() {
                match indexer.sync_paths(&config, &paths) {
                    Ok(event) => reporter.event(&event),
                    Err(error) => {
                        // Those paths were taken: look at everything next time instead.
                        log(&format!("sync failed: {error:#}"));
                        changes.rescan = true;
                    }
                }
            }
        }

        let pending = indexer.index.pending_count().unwrap_or(0);
        if pending > 0 && paused {
            reporter.set(IndexerState::Paused, Some(format!("Paused, {pending} files waiting")));
        } else if pending > 0 {
            // Conditions are checked every few seconds, not on every pass.
            let reason = match &waiting_reason {
                Some((reason, checked)) if checked.elapsed() < WAIT_CHECK => Some(reason.clone()),
                _ => {
                    let reason = reason_to_wait(&config);
                    waiting_reason = reason.clone().map(|reason| (reason, Instant::now()));
                    reason
                }
            };
            if let Some(reason) = reason {
                reporter.set(IndexerState::Waiting, Some(reason));
                indexer.release_model();
            } else {
                waiting_reason = None;
                if !crate::models::status(&config::models_dir()).media {
                    reporter.set(IndexerState::Waiting, Some("Waiting for the model to download".into()));
                } else {
                    let stop = || {
                        SHUTDOWN.load(Ordering::SeqCst)
                            || TOGGLE_PAUSE.load(Ordering::SeqCst)
                            || RESYNC.load(Ordering::SeqCst)
                            || modified(&config_path) != config_time
                    };
                    let mut events = |event: Event| reporter.event(&event);
                    let mut take = || changes.take();
                    // Speech beside the reading, when memory allows both models.
                    let beside = Plan::of(&config.indexing).beside
                        && crate::speech::unavailable().is_none()
                        && speech_failed_at.is_none_or(|at| at.elapsed() > SPEECH_RETRY);
                    let helper = beside.then_some(&mut transcriber);
                    if let Err(error) = indexer.index_pending(&config, &mut events, &stop, &mut take, helper) {
                        log(&format!("indexing stopped: {error:#}"));
                        reporter.status.last_error = Some(format!("{error:#}"));
                        if error.is::<Stuck>() {
                            // The stuck thread can't be stopped, and what it's stuck in stays stuck in this process:
                            // it ends, and launchd starts a new daemon (KeepAlive), which goes on with the rest.
                            reporter.set(IndexerState::Stopped, None);
                            return Err(error);
                        }
                        changes.rescan = true;
                        // Not straight back into the same failure.
                        sleep_interruptibly(Duration::from_secs(30));
                    }
                    idle_since = Instant::now();
                    continue;
                }
            }
        } else {
            // Everything is read: what's said in recordings and videos is written down meanwhile.
            let to_write = !paused
                && indexer.index.transcripts_pending(1).map(|rows| !rows.is_empty()).unwrap_or(false)
                && reason_to_wait(&config).is_none();
            let may_start = speech_failed_at.is_none_or(|at| at.elapsed() > SPEECH_RETRY);
            if to_write && transcriber.is_none() && may_start {
                match Transcriber::start() {
                    Ok(started) => {
                        transcriber = Some(started);
                        speech_problem = None;
                        speech_failed_at = None;
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if speech_problem.as_deref() != Some(&error) {
                            log(&format!("can't write down speech: {error}"));
                        }
                        speech_problem = Some(error);
                        speech_failed_at = Some(Instant::now());
                    }
                }
            }
            // A change that's still settling is synced and read first: the helper waits for it, loaded.
            let settling = !changes.paths.is_empty();
            if let (true, Some(running), false) = (to_write, transcriber.as_mut(), settling) {
                if let Ok(index::Transcripts { done, total, .. }) = indexer.index.transcripts() {
                    reporter.status.speech_done = done;
                    reporter.status.speech_total = total;
                    reporter.status.done = done;
                    reporter.status.total = total;
                }
                reporter.set(IndexerState::Transcribing, reporter.status.detail.clone());
                // Between chunks it makes way for anything that changes in the folders (synced, and read first).
                let arrived = std::cell::RefCell::new(Vec::new());
                let stop = || {
                    if SHUTDOWN.load(Ordering::SeqCst)
                        || TOGGLE_PAUSE.load(Ordering::SeqCst)
                        || RESYNC.load(Ordering::SeqCst)
                        || modified(&config_path) != config_time
                    {
                        return true;
                    }
                    while let Ok(event) = changes.receiver.try_recv() {
                        arrived.borrow_mut().push(event);
                    }
                    arrived.borrow().iter().any(|event| match event {
                        Ok(event) => event.need_rescan() || event.paths.iter().any(|path| changes.worth_a_look(path)),
                        Err(_) => true,
                    })
                };
                let mut events = |event: Event| reporter.event(&event);
                if let Err(error) = indexer.transcribe_next(running, &mut events, &stop) {
                    log(&format!("writing down speech stopped: {error:#}"));
                    reporter.status.last_error = Some(format!("{error:#}"));
                    // A new helper next time (one that stopped is no use), after a pause.
                    transcriber = None;
                    sleep_interruptibly(Duration::from_secs(30));
                }
                for event in arrived.into_inner() {
                    changes.add(event);
                }
                idle_since = Instant::now();
                continue;
            }
            if !(to_write && settling) {
                // Nothing (more) to write down: the helper goes, and its memory with it.
                transcriber = None;
            }
            reporter.status.current = None;
            // Partitions with many dead rows, or sketches made against a mean that moved: their next generation.
            if !paused && compact_failed_at.is_none_or(|at| at.elapsed() > COMPACT_RETRY) {
                match indexer.index.compact_vectors() {
                    Ok(written) if !written.is_empty() => {
                        log(&format!("wrote the vectors again: {}", written.join(", ")))
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log(&format!("can't write the vectors again: {error:#}"));
                        compact_failed_at = Some(Instant::now());
                    }
                }
            }
            let detail = speech_problem.clone().filter(|_| to_write);
            reporter.set(IndexerState::Idle, detail);
        }

        if indexer.model_loaded() && idle_since.elapsed() > Duration::from_secs(config.indexing.release_model_s) {
            indexer.release_model();
            log("released the model");
        }
        // Wait for something to happen.
        match changes.receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => changes.add(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the file watcher stopped"),
        }
    }
    reporter.set(IndexerState::Stopped, None);
    log("sift-it daemon stopped");
    drop(watcher);
    Ok(())
}

fn sleep_interruptibly(duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until && !SHUTDOWN.load(Ordering::SeqCst) && !RESYNC.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Keeps the log (where launchd sends stderr) from growing without end: past `max` bytes, its older half goes.
fn trim_log(max: u64) {
    let path = config::home().join("indexer.log");
    let Ok(metadata) = std::fs::metadata(&path) else { return };
    if metadata.len() <= max {
        return;
    }
    if let Ok(bytes) = std::fs::read(&path) {
        let half = &bytes[bytes.len() / 2..];
        let start = half.iter().position(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
        let _ = std::fs::write(&path, &half[start..]);
    }
}
