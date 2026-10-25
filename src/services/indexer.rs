//! `sift-it daemon`: the background indexer. It keeps the index in step with the chosen folders: a full sync when it
//! starts (and every `indexing.full_sync_min`, in case an event was missed), then the paths macOS reports as changed
//! (FSEvents), a moment after they go quiet. It indexes in rounds (ingest/rounds.rs), words first, as the plan says
//! (resources::Plan): what runs side by side by `indexing.memory_mb`, what goes on on battery by
//! `indexing.on_battery`, and less, or nothing, when macOS says memory is short. What changes in the folders goes
//! first in every round it needs. It lets the model go after `indexing.release_model_s` with nothing to do. A change
//! to config.json counts at once, with a full sync. While config.json can't be read, nothing is indexed: status.json
//! says why, and the daemon waits for the file to change.
//!
//! With nothing left to do, it writes the next generation of a vector partition that's due one (index/vectors.rs).
//!
//! It says what it's doing, and how far each round is, in status.json (for `sift-it status` and front ends), and
//! listens to signals: SIGUSR1  look for changes now (a full sync) SIGUSR2  pause or resume SIGTERM  stop (what isn't
//! finished stays queued)
//!
//! One runs at a time (daemon.lock). When a reader gets stuck (ingest::Stuck), it exits, and launchd starts a new one.

use crate::config::{self, Config};
use crate::index::{self, Writer};
use crate::ingest::progress::{self, Progress};
use crate::ingest::{Event, Facts, Indexer, Outcome, Stuck};
use crate::query::timestamp;
use crate::resources::{self, Conditions, Plan, Power, Queue, Tool, Wait};
use anyhow::{Context, Result, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{IndexerState, IndexerStatus};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime};

/// The power and the models are looked at again this often (memory pressure, at once).
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
    /// The power when it last looked, for what waiting means.
    power: Power,
}

impl Reporter {
    fn set(&mut self, state: IndexerState, detail: Option<String>) {
        if self.status.state != state || self.status.detail != detail {
            self.status.state = state;
            self.status.detail = detail;
            self.write(true);
        }
    }

    /// What the work left waits for, in words.
    fn waiting(&self, wait: Option<Wait>) -> Option<String> {
        wait.map(|wait| wait.describe(self.power).to_string())
    }

    fn event(&mut self, event: &Event) {
        match event {
            Event::Sync { added, changed, removed, .. } if added + changed + removed > 0 => {
                log(&format!("sync: {added} new, {changed} changed, {removed} gone"));
            }
            Event::Sync { .. } => {}
            Event::Plan { rounds, readers, waiting } => {
                let rounds: Vec<&str> = rounds.iter().map(|round| round.as_str()).collect();
                log(&format!("running: {} ({readers} reading at once)", rounds.join(", ")));
                let detail = self.waiting(*waiting);
                self.set(IndexerState::Indexing, detail);
            }
            Event::File { path, .. } => self.status.current = Some(path.clone()),
            Event::Failed { round, path, error } => {
                self.status.last_error = Some(format!("{}: {error}", file_name(path)));
                log(&format!("{}: can't do {path}: {error}", round.as_str()));
            }
            Event::Transcribing { path, at, duration } => {
                self.status.current = Some(path.clone());
                if *at == 0.0 {
                    log(&format!("writing down {} ({})", file_name(path), timestamp(*duration)));
                }
            }
            Event::Rounds { rounds } => self.status.rounds = rounds.clone(),
            Event::Finished { files, failed, seconds, stopped } => {
                log(&format!(
                    "{files} files' rounds done ({failed} failed) in {seconds:.1} s{}",
                    if *stopped { ", stopped early" } else { "" }
                ));
                self.status.current = None;
            }
            Event::SpeechStopped { error } => {
                log(&format!("writing down speech stopped, it's tried again later: {error}"))
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

/// A line of the log, after the time ("2026-10-14 12:34:56", in the Mac's time zone).
fn log(line: &str) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&libc::time(std::ptr::null_mut()), &mut tm) };
    let (year, month, day) = (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
    eprintln!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} {line}", tm.tm_hour, tm.tm_min, tm.tm_sec);
}

/// Locks daemon.lock, held for as long as the file is: one indexer at a time.
fn lock() -> Result<File> {
    std::fs::create_dir_all(config::home())?;
    let file = File::options().create(true).truncate(false).write(true).open(lock_path())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another sift-it daemon is running (pid {})", running_pid().map(|p| p.to_string()).unwrap_or("?".into()));
    }
    std::fs::write(lock_path(), std::process::id().to_string())?;
    Ok(file)
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

/// Whether the work in hand should make way: the indexer stops, pauses or syncs again, or config.json changed what it
/// works by. `seen` is when config.json was last looked at, so a change that doesn't matter here is read only once.
fn interrupted(config_path: &Path, seen: &Cell<Option<SystemTime>>, config: &Config) -> bool {
    SHUTDOWN.load(Ordering::SeqCst) || TOGGLE_PAUSE.load(Ordering::SeqCst) || RESYNC.load(Ordering::SeqCst) || {
        let time = modified(config_path);
        time != seen.replace(time) && !Config::load().is_ok_and(|new| same_work(&new, config))
    }
}

/// Whether two configs give the indexer the same work: `search` and `ranking` are the engine's, and `log` is read when
/// the daemon starts.
fn same_work(a: &Config, b: &Config) -> bool {
    a.sources == b.sources && a.reading == b.reading && a.indexing == b.indexing
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// config.json, and when it was written, once it can be read. Until then nothing is indexed and the model goes:
/// status.json says why, and the daemon waits for the file to change. None when the daemon is to stop meanwhile.
fn read_config(
    path: &Path,
    reporter: &mut Reporter,
    mut indexer: Option<&mut Indexer>,
) -> Option<(Config, Option<SystemTime>)> {
    loop {
        let time = modified(path);
        let error = match Config::load() {
            Ok(config) => return Some((config, time)),
            Err(error) => error,
        };
        log(&format!("can't read the config, waiting for it to change: {error:#}"));
        reporter
            .set(IndexerState::Waiting, Some(format!("Waiting: config.json can't be read: {}", error.root_cause())));
        if let Some(indexer) = indexer.as_deref_mut() {
            indexer.release_model();
        }
        while modified(path) == time {
            if SHUTDOWN.load(Ordering::SeqCst) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
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

/// What the plan follows that's dear to look at: the power (IOKit) and which models are there, looked at again every
/// WAIT_CHECK. Memory pressure comes from a dispatch source, and is read each time.
struct Looked {
    at: Option<Instant>,
    power: Power,
    missing: Vec<Tool>,
}

impl Looked {
    fn facts(&mut self, speech_failed_at: Option<Instant>) -> Facts {
        if self.at.is_none_or(|at| at.elapsed() >= WAIT_CHECK) {
            self.at = Some(Instant::now());
            self.power = resources::power();
            self.missing = resources::missing();
        }
        let mut missing = self.missing.clone();
        // A speech helper that couldn't start, or stopped, is tried again after a while.
        if speech_failed_at.is_some_and(|at| at.elapsed() < SPEECH_RETRY) && !missing.contains(&Tool::SpeechModel) {
            missing.push(Tool::SpeechModel);
        }
        let conditions = Conditions { power: self.power, pressure: crate::adapters::macos::pressure::level() };
        Facts { conditions, missing }
    }
}

pub fn run() -> Result<()> {
    let _lock = lock()?;
    install_signal_handlers();
    let config_path = config::config_path();
    log(&format!("sift-it daemon started (pid {})", std::process::id()));
    let mut reporter = Reporter {
        status: IndexerStatus { pid: std::process::id(), ..Default::default() },
        last_write: Instant::now(),
        power: Power::Charger,
    };
    reporter.write(true);
    let Some((mut config, mut config_time)) = read_config(&config_path, &mut reporter, None) else {
        reporter.set(IndexerState::Stopped, None);
        return Ok(());
    };
    trim_log(config.log.max_mb << 20);

    let index = Writer::open(&config::index_path()).context("can't open the index")?;
    let mut indexer = Indexer::new(index)?;
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
    let mut looked = Looked { at: None, power: Power::Charger, missing: Vec::new() };
    // How fast each round went while it ran, across runs: for the time each has left.
    let mut progress = Progress::default();
    // When the speech helper last couldn't start or stopped.
    let speech_failed_at: Cell<Option<Instant>> = Cell::new(None);
    // When writing a partition's next generation last failed: it's tried again a while later, not on every pass.
    let mut compact_failed_at: Option<Instant> = None;
    // The plan how far each round is was last shown with: it's shown again when that changes, or the index did (None).
    let mut shown: Option<Plan> = None;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        // The config changed: watch the folders it names now, and sync it all (what's read, and how, may differ). A
        // change to search, ranking or the log is only taken in.
        if modified(&config_path) != config_time {
            let Some((new, time)) = read_config(&config_path, &mut reporter, Some(&mut indexer)) else { break };
            config_time = time;
            let changed = !same_work(&new, &config);
            config = new;
            if changed {
                looked.at = None;
                changes.roots = config.sources.paths.clone();
                changes.quiet = Duration::from_millis(config.indexing.quiet_ms);
                drop(watcher);
                watcher = watch(&config, sender.clone())?;
                changes.rescan = true;
                log("the config changed");
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
            shown = None;
            match indexer.sync(&config) {
                Ok(event) => reporter.event(&event),
                Err(error) => log(&format!("sync failed: {error:#}")),
            }
            last_full = Instant::now();
        } else {
            let paths = changes.take();
            if !paths.is_empty() {
                shown = None;
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

        let facts = looked.facts(speech_failed_at.get());
        reporter.power = facts.conditions.power;
        let queues: Vec<Queue> = indexer
            .index
            .queues()
            .unwrap_or_default()
            .into_iter()
            .map(|(round, new, old)| Queue { round, new, old })
            .collect();
        let held = if indexer.model_loaded() { vec![Tool::TextModel] } else { Vec::new() };
        let plan = Plan::of(&config.indexing, facts.conditions, &queues, &facts.missing, &held);
        let mut report = |reporter: &mut Reporter, indexer: &Indexer| {
            if shown.as_ref() != Some(&plan)
                && let Ok(totals) = indexer.index.rounds()
            {
                reporter.status.rounds = progress::statuses(&totals, &plan, |_| None);
                shown = Some(plan.clone());
            }
        };
        if !queues.is_empty() && paused {
            report(&mut reporter, &indexer);
            reporter.set(IndexerState::Paused, None);
            indexer.release_model();
        } else if !plan.runs.is_empty() {
            reporter.set(IndexerState::Indexing, reporter.waiting(plan.waiting()));
            let seen = Cell::new(config_time);
            let stop = || interrupted(&config_path, &seen, &config);
            let mut facts = || looked.facts(speech_failed_at.get());
            let mut events = |event: Event| {
                if let Event::SpeechStopped { .. } = event {
                    speech_failed_at.set(Some(Instant::now()));
                }
                reporter.event(&event);
            };
            let mut take = || changes.take();
            match indexer.run(&config, &mut facts, &mut progress, &mut events, &stop, &mut take) {
                Ok(Outcome::Waits(Wait::Memory)) => {
                    indexer.release_model();
                    log("the Mac is short of memory: everything lets go, and waits");
                }
                Ok(_) => {}
                Err(error) => {
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
            }
            idle_since = Instant::now();
            shown = None;
            continue;
        } else if !queues.is_empty() {
            // All that's left waits: for a charger, memory, or a model.
            report(&mut reporter, &indexer);
            let detail = reporter.waiting(plan.waits_for());
            reporter.set(IndexerState::Waiting, detail);
            indexer.release_model();
        } else {
            report(&mut reporter, &indexer);
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
            reporter.set(IndexerState::Idle, None);
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
    let path = super::launchd::INDEXER.log_path();
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
