mod adapters;
mod clients;
mod config;
mod index;
mod ingest;
mod models;
mod query;
mod resources;
mod services;
mod tools;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use protocol::{Kind, MeaningState, Mode, Search};
use std::path::PathBuf;

/// Sift It: search your files by what's in them, with EmbeddingGemma 2 on your own Mac.
#[derive(Parser)]
#[command(name = "sift-it", version)]
struct Cli {
    /// Without one, sift-it opens the finder: search as you type
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// The folders to index
    Folders {
        #[command(subcommand)]
        action: Option<FolderAction>,
    },
    /// Show config.json or a part of it, or set a value: `sift-it config reading.video.keep_every_s 10` (null puts
    /// it back to its default)
    Config { key: Option<String>, value: Option<String> },
    /// How many files these folders hold, and about how long indexing them takes
    Estimate {
        folders: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bring the index up to date with the folders, then exit
    Index {
        /// JSON lines instead of text
        #[arg(long)]
        json: bool,
        /// Drop every vector and embed everything again
        #[arg(long)]
        reindex: bool,
    },
    /// Search the index
    Search {
        words: Vec<String>,
        /// Only these kinds
        #[arg(long, value_delimiter = ',')]
        kind: Vec<Kind>,
        #[arg(short = 'n', default_value_t = 10)]
        limit: usize,
        /// By words or by meaning (config.json's `search.mode` when not said)
        #[arg(long, value_enum)]
        by: Option<Mode>,
        #[arg(long)]
        json: bool,
    },
    /// Search as you type, full screen, with a preview of each result
    Find {
        /// Start with these words
        words: Vec<String>,
    },
    /// Open a result where it matched: a recording at its moment, a PDF at its page, code at its line
    Open {
        /// A result's number from the last search, or a file
        target: String,
        /// For a file: the place to open it at (a page, a second, or a line)
        #[arg(long)]
        at: Option<f64>,
        /// Show it in Finder instead
        #[arg(short, long)]
        reveal: bool,
    },
    /// Relay a front end's searches (JSON lines over stdin/stdout) to the engine, and make its previews
    Serve,
    /// The engine that answers every search: run it (launchd does), or stop it or let its model go
    Engine {
        #[command(subcommand)]
        action: Option<EngineAction>,
    },
    /// Keep the index up to date in the background, watching the folders for changes
    Daemon,
    /// The background indexer and the engine as LaunchAgents (they start at login)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// What's in the index
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The model files
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Measure search against known answers (evals/evals.json on the testbed)
    Eval {
        file: PathBuf,
        /// Show each query's top results with their scores
        #[arg(short, long)]
        verbose: bool,
        /// Compare the search with sketches with the exact one instead (search.vectors)
        #[arg(long)]
        compare: bool,
    },
    /// Show what Sift It reads from a file: its pieces, without the model
    Read {
        path: PathBuf,
        /// Also count each text piece's tokens
        #[arg(long)]
        tokens: bool,
    },
    /// Time the model on texts, pictures and sound (for tuning)
    #[command(hide = true)]
    BenchEmbed,
    /// Time search over an index of random vectors in each form of the vectors (for tuning)
    #[command(hide = true)]
    BenchSearch {
        #[arg(long, default_value_t = 200_000)]
        vectors: usize,
    },
    /// Check that this build makes the reference vectors
    Selftest {
        /// A folder with vectors.json and its inputs (default: the ones built in)
        folder: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum FolderAction {
    /// Add folders to index
    Add { paths: Vec<String> },
    /// Stop indexing folders (what was indexed from them leaves at the next sync)
    Remove { paths: Vec<String> },
    /// Skip a folder inside the indexed ones
    Exclude { paths: Vec<String> },
    /// Stop skipping a folder
    Include { paths: Vec<String> },
}

#[derive(Subcommand)]
enum EngineAction {
    /// Stop it (the next search starts it again)
    Stop,
    /// Let the model and the vectors go, until a search by meaning needs them
    Unload,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start it
    Install,
    /// Stop and remove it
    Uninstall,
    /// Whether it's installed and running
    Status,
}

#[derive(Subcommand)]
enum ModelAction {
    /// Whether the model files are there
    Status,
    /// Download the model files (1.6 GB, once: 865 MB to search, 740 MB to write down speech)
    Download {
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    // Safety: nothing else runs yet.
    unsafe { embedder::set_environment() };
    if let Err(error) = run(Cli::parse()) {
        eprintln!("sift-it: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    use std::io::IsTerminal;
    let Some(command) = cli.command else {
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            return clients::finder::run("");
        }
        <Cli as clap::CommandFactory>::command().print_help()?;
        return Ok(());
    };
    match command {
        Command::Find { words } => clients::finder::run(&words.join(" ")),
        Command::Folders { action } => folders(action),
        Command::Config { key, value } => configure(key, value),
        Command::Estimate { folders, json } => {
            let config = Config::load()?;
            let folders: Vec<PathBuf> = if folders.is_empty() {
                config.sources.paths.clone()
            } else {
                folders.iter().map(|f| config::expand(f)).collect()
            };
            let estimates: Vec<_> =
                folders.into_iter().map(|folder| tools::estimate::estimate(&config, folder)).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&estimates)?);
            } else {
                for e in &estimates {
                    let kinds = e.by_kind.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ");
                    let all = tools::estimate::describe(e.seconds as f64);
                    println!("{}: {} files ({kinds}), {all}", e.folder.display(), e.files);
                    for round in &e.rounds {
                        let amount = match round.unit {
                            "seconds" => format!(
                                "{} of sound",
                                tools::estimate::describe(round.total as f64).replacen("about ", "", 1)
                            ),
                            unit => format!("{} {unit}", round.total),
                        };
                        let time = tools::estimate::describe(round.seconds as f64);
                        println!("  {:<9} {amount}, {time}", format!("{}:", round.round.as_str()));
                    }
                }
            }
            Ok(())
        }
        Command::Index { json, reindex } => index(json, reindex),
        Command::Search { words, kind, limit, by, json } => search_once(&words.join(" "), kind, limit, by, json),
        Command::Open { target, at, reveal } => {
            let place = match target.parse::<usize>() {
                Ok(number) if !std::path::Path::new(&target).exists() => clients::open::recall(number)?,
                _ => clients::open::Place::file(&config::expand(&target), at)?,
            };
            println!("{}", clients::open::open(&place, reveal)?);
            Ok(())
        }
        Command::Serve => clients::serve::serve(),
        Command::Engine { action: None } => services::engine::run(&config::index_path()),
        Command::Engine { action: Some(action) } => {
            let Some(mut engine) = clients::Client::running() else {
                println!("the engine isn't running");
                return Ok(());
            };
            let op = match action {
                EngineAction::Stop => protocol::Op::Stop,
                EngineAction::Unload => protocol::Op::Unload,
            };
            engine.request::<protocol::Done>(op)?;
            Ok(())
        }
        Command::Daemon => services::indexer::run(),
        Command::Service { action } => match action {
            ServiceAction::Install => services::launchd::install(),
            ServiceAction::Uninstall => services::launchd::uninstall(),
            ServiceAction::Status => services::launchd::status(),
        },
        Command::Status { json } => status(json),
        Command::Model { action } => match action {
            ModelAction::Status => {
                println!("{}", serde_json::to_string_pretty(&models::status())?);
                Ok(())
            }
            ModelAction::Download { json } => models::download(&mut |progress| {
                if json {
                    println!("{}", serde_json::to_string(&progress).unwrap());
                } else if let models::Progress::Downloading { file, done, total } = progress {
                    eprint!("\r{file}: {} of {} MB   ", done >> 20, total >> 20);
                } else if let models::Progress::Ready { file } = progress {
                    eprintln!("\r{file}: ready{:20}", "");
                }
            })
            .and_then(|()| {
                // An engine that couldn't load the model tries again on its next search by meaning.
                if let Some(mut engine) = clients::Client::running()
                    && engine.request::<protocol::Status>(protocol::Op::Status)?.meaning == MeaningState::Unavailable
                {
                    engine.request::<protocol::Done>(protocol::Op::Unload)?;
                }
                Ok(())
            }),
        },
        Command::Eval { file, verbose, compare } => {
            // The engine's own answers, in this process: what's measured is what front ends get.
            let core = services::engine::Core::new(&config::index_path(), &models::TEXT, Box::new(|_| {}))?;
            let outcome = tools::eval::run(&file, &core, verbose, compare);
            // The model goes before the process does (llama.cpp aborts when its Metal device is freed under one).
            core.unload();
            outcome
        }
        Command::Read { path, tokens } => {
            let path = config::expand(&path.to_string_lossy());
            let config = Config::load()?;
            let kind = ingest::discover::crawl::kind_of(&config, &path)
                .ok_or_else(|| anyhow::anyhow!("Sift It doesn't read .{} files", config::extension_of(&path)))?;
            let settings = config.reading.resolve(&path, kind);
            let mut model = if tokens {
                Some(embedder::Embedder::load(&models::TEXT.require()?, embedder::Inputs::Documents)?)
            } else {
                None
            };
            let started = std::time::Instant::now();
            let (mut count, mut total_tokens) = (0, 0);
            // Each round the file passes through, as the indexer reads it, item after item. Speech is left out: it
            // needs the speech model.
            let mut rounds = vec![(protocol::Round::Words, 0.0)];
            let mut pages = Vec::new();
            let mut at = 0;
            while let Some(&(round, _)) = rounds.get(at) {
                at += 1;
                if matches!(round, protocol::Round::Speech) {
                    continue;
                }
                let mut from = ingest::read::From { pages: pages.clone(), ..Default::default() };
                loop {
                    let ended = ingest::read::read(
                        round,
                        &path,
                        kind,
                        &settings,
                        &from,
                        &mut |piece| {
                            count += 1;
                            let what = match &piece.payload {
                                ingest::Payload::None => "words only".to_string(),
                                ingest::Payload::Text(_) => match (model.as_mut(), piece.text.as_deref()) {
                                    (Some(model), Some(text)) => {
                                        let n = model.count_tokens(text);
                                        total_tokens += n;
                                        format!("text, {n} tokens")
                                    }
                                    (_, text) => format!("text, {} chars", text.map_or(0, str::len)),
                                },
                                ingest::Payload::Later => {
                                    if let Some(page) = piece.loc {
                                        pages.push(page as usize);
                                    }
                                    "picture, for later".to_string()
                                }
                                ingest::Payload::Image(image) => format!("picture {}×{}", image.width, image.height),
                                ingest::Payload::Audio(samples) => {
                                    format!("sound {:.1} s", samples.len() as f64 / embedder::AUDIO_RATE as f64)
                                }
                            };
                            let at = piece.loc.map(|l| format!(" at {l}")).unwrap_or_default();
                            let text = piece.text.as_deref().unwrap_or("").replace('\n', " ");
                            println!(
                                "{:<9}{:<8}{at:<10} {what:<22} {}",
                                round.as_str(),
                                piece.segment.as_str(),
                                text.chars().take(60).collect::<String>()
                            );
                            Ok(())
                        },
                        &mut |_| Ok(true),
                    )?;
                    if round == protocol::Round::Words {
                        rounds.extend(ended.rounds.iter().copied());
                    }
                    if round == protocol::Round::Ocr && ended.finished && !pages.is_empty() {
                        rounds.push((protocol::Round::Pictures, pages.len() as f64));
                    }
                    if !ended.info.as_object().is_none_or(|info| info.is_empty()) {
                        println!("{:<9}{}", round.as_str(), ended.info);
                    }
                    if ended.finished {
                        break;
                    }
                    from = ingest::read::From { done: ended.done, resume: ended.resume, ..Default::default() };
                }
            }
            println!(
                "{count} pieces in {:.0} ms{}",
                started.elapsed().as_secs_f64() * 1000.0,
                if tokens { format!(", {total_tokens} tokens") } else { String::new() }
            );
            Ok(())
        }
        Command::BenchSearch { vectors } => tools::bench::search(vectors),
        Command::BenchEmbed => tools::bench::embed(),
        Command::Selftest { folder } => match folder {
            Some(folder) => tools::selftest::run(&folder),
            None => tools::selftest::run_built_in(),
        },
    }
}

fn folders(action: Option<FolderAction>) -> Result<()> {
    let mut config = Config::load()?;
    if let Some(action) = action {
        let (action, paths) = match action {
            FolderAction::Add { paths } => (protocol::FolderAction::Add, paths),
            FolderAction::Remove { paths } => (protocol::FolderAction::Remove, paths),
            FolderAction::Include { paths } => (protocol::FolderAction::Include, paths),
            FolderAction::Exclude { paths } => (protocol::FolderAction::Exclude, paths),
        };
        config.change_folders(action, &paths)?;
        config.save()?;
    }
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

/// Prints config.json, or the value at a dotted key, after setting it when there's a value.
fn configure(key: Option<String>, value: Option<String>) -> Result<()> {
    let config = config::configure(key.as_deref(), value.as_deref())?;
    // All of it in config.json's order; a part through JSON values, which sort their keys.
    let shown = match key.as_deref() {
        None => serde_json::to_string_pretty(&config)?,
        Some(key) => match config.get(Some(key)) {
            Ok(shown) => serde_json::to_string_pretty(&shown)?,
            // Put back to its default, a kind's or a file type's value is gone: the one above it counts again.
            Err(_) if value.as_deref() == Some("null") => "null".to_string(),
            Err(error) => return Err(error),
        },
    };
    println!("{shown}");
    Ok(())
}

fn index(json: bool, reindex: bool) -> Result<()> {
    let config = Config::load()?;
    if config.sources.paths.is_empty() {
        bail!("no folders to index: add some with `sift-it folders add <folder>`");
    }
    // Two indexers would read the same files: let the running one do it.
    if let Some(pid) = services::indexer::running_pid() {
        if reindex {
            bail!("the background indexer is running (pid {pid}); stop it before --reindex");
        }
        unsafe { libc::kill(pid, libc::SIGUSR1) };
        eprintln!("the background indexer (pid {pid}) is looking for changes now");
        return Ok(());
    }
    let index = index::Writer::open(&config::index_path())?;
    if reindex {
        index.requeue_vectors()?;
    }
    let mut indexer = ingest::Indexer::new(index)?;
    let mut report = |event: ingest::Event| {
        if json {
            println!("{}", serde_json::to_string(&event).unwrap());
            return;
        }
        match event {
            ingest::Event::Sync { files, added, changed, removed, ms, .. } => {
                eprintln!("{files} files in the folders ({added} new, {changed} changed, {removed} gone, {ms} ms)")
            }
            ingest::Event::Plan { rounds, readers, .. } => {
                let rounds: Vec<&str> = rounds.iter().map(|round| round.as_str()).collect();
                eprintln!("running: {} ({readers} reading at once)", rounds.join(", "))
            }
            ingest::Event::File { round, path, segments, vectors, ms, .. } => {
                eprintln!("  {}: {path}: {segments} segments, {vectors} vectors, {ms} ms", round.as_str())
            }
            ingest::Event::Failed { round, path, error } => eprintln!("  {}: {path}: {error}", round.as_str()),
            ingest::Event::Finished { files, failed, seconds, .. } => {
                eprintln!("done: {files} files through their rounds, {failed} failed, in {seconds:.1} s")
            }
            ingest::Event::SpeechStopped { error } => {
                eprintln!("what's said in recordings isn't written down: {error}")
            }
            ingest::Event::Transcribing { .. } | ingest::Event::Rounds { .. } => {}
        }
    };
    let sync = indexer.sync(&config)?;
    report(sync);
    // It doesn't wait for a charger: it was asked to index now. It does make way when memory is short.
    let missing = resources::missing();
    let mut facts = || ingest::Facts {
        conditions: resources::Conditions {
            power: resources::Power::Charger,
            pressure: adapters::macos::pressure::level(),
        },
        missing: missing.clone(),
    };
    let mut progress = ingest::progress::Progress::default();
    match indexer.run(&config, &mut facts, &mut progress, &mut report, &|| false, &mut Vec::new) {
        // This process can't go on (ingest::Stuck); a new one can, with the file set aside.
        Err(error) if error.is::<ingest::Stuck>() => bail!("{error}. Run `sift-it index` again for the rest"),
        Err(error) => return Err(error),
        Ok(ingest::Outcome::Waits(wait)) => {
            eprintln!("{}: run `sift-it index` again for the rest", wait.describe(resources::Power::Charger))
        }
        Ok(_) => {}
    }
    let written = indexer.index.compact_vectors()?;
    if !written.is_empty() && !json {
        eprintln!("wrote the vectors again: {}", written.join(", "));
    }
    Ok(())
}

fn search_once(q: &str, kinds: Vec<Kind>, limit: usize, by: Option<Mode>, json: bool) -> Result<()> {
    // By meaning, it waits for the engine to load the model if it hasn't yet.
    let request = Search { q: q.to_string(), mode: by, kinds, limit, wait: true, ..Default::default() };
    let mut client = clients::Client::connect()?;
    let response = client.search(&request)?;
    clients::open::remember(q, &response.hits);
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    let terminal = clients::term::Terminal::detect();
    if !terminal.tty {
        // Piped: one path per line, for xargs and friends.
        for hit in &response.hits {
            println!("{}", hit.path);
        }
        return Ok(());
    }
    clients::present::print(&terminal, &response, q, &mut client)
}

fn status(json: bool) -> Result<()> {
    let index = index::Reader::open(&config::index_path())?;
    let stats = index.stats()?;
    let config = Config::load()?;
    let models = models::status();
    // The background indexer's own account when it runs (with the time each round has left), else the index's.
    let indexer = services::engine::indexer_status().filter(|status| {
        status.pid > 0
            && status.state != protocol::IndexerState::Stopped
            && unsafe { libc::kill(status.pid as i32, 0) } == 0
    });
    let queues: Vec<resources::Queue> =
        index.queues()?.into_iter().map(|(round, new, old)| resources::Queue { round, new, old }).collect();
    let missing = resources::missing();
    let conditions = resources::Conditions { power: resources::power(), pressure: adapters::macos::pressure::level() };
    let plan = resources::Plan::of(&config.indexing, conditions, &queues, &missing, &[]);
    let rounds = match &indexer {
        Some(indexer) => indexer.rounds.clone(),
        None => ingest::progress::statuses(&index.rounds()?, &plan, |_| None),
    };
    let heaviest = index.heaviest(5)?;
    // The engine, if it's running (status doesn't start it): what it holds.
    let engine =
        clients::Client::running().and_then(|mut engine| engine.request::<protocol::Status>(protocol::Op::Status).ok());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "index": config::index_path().display().to_string(),
                "stats": stats,
                "rounds": rounds,
                "heaviest": heaviest,
                "speech": { "unavailable": adapters::speech::unavailable() },
                "indexing": {
                    "memory_mb": plan.memory >> 20,
                    "readers": plan.readers,
                    "runs": plan.runs.iter().map(|running| running.round).collect::<Vec<_>>(),
                },
                "indexer": indexer,
                "engine": engine,
                "config": config,
                "models": models,
            }))?
        );
        return Ok(());
    }
    println!("index:    {}", config::index_path().display());
    println!(
        "files:    {} ({} done, {} with rounds to go, {} with a round that failed)",
        stats.files, stats.done, stats.pending, stats.failed
    );
    println!("vectors:  {} in {} segments", stats.vectors, stats.segments);
    for round in &rounds {
        println!("{:<9} {}", format!("{}:", round.round.as_str()), describe_round(round));
    }
    if let Some(waits) = indexer.as_ref().and_then(|indexer| indexer.detail.clone()) {
        println!("          {waits}");
    }
    // The work still to do, and where most of it is: what `ignore` or a lower depth could leave out.
    for heavy in heaviest.iter().filter(|heavy| heavy.left >= 1800.0) {
        let left = tools::estimate::describe(heavy.left).replacen("about ", "", 1);
        println!("          {} ({}: {left} of sound to go)", heavy.path, heavy.round.as_str());
    }
    let mut kinds: Vec<_> = stats.by_kind.iter().collect();
    kinds.sort();
    println!("kinds:    {}", kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "));
    let folders: Vec<String> = config.sources.paths.iter().map(|f| f.display().to_string()).collect();
    println!("folders:  {}", folders.join(", "));
    println!(
        "model:    {}",
        if models.media {
            "ready"
        } else if models.text {
            "text only"
        } else {
            "not downloaded"
        }
    );
    let engine = match engine {
        None => "not running (the next search starts it)".to_string(),
        Some(engine) => match engine.meaning {
            MeaningState::Ready => format!(
                "running, {} vectors mapped for search by meaning ({}, {} MB of them in memory)",
                engine.vectors,
                serde_json::json!(engine.scan).as_str().unwrap_or("?"),
                engine.resident >> 20
            ),
            MeaningState::Loading => "running, loading the model and the vectors".to_string(),
            MeaningState::Unavailable => {
                format!("running, by words only: {}", engine.error.as_deref().unwrap_or("no model"))
            }
            MeaningState::Unloaded => "running, nothing loaded (search by meaning loads it)".to_string(),
        },
    };
    println!("engine:   {engine}");
    if let Some(why) = adapters::speech::unavailable() {
        println!("speech:   {why}");
    }
    let runs: Vec<&str> = plan.runs.iter().map(|running| running.round.as_str()).collect();
    println!(
        "memory:   indexing takes up to {} MB: {}, {} {} read at once",
        plan.memory >> 20,
        if runs.is_empty() {
            "nothing to run now".to_string()
        } else {
            format!("{} side by side now", runs.join(", "))
        },
        plan.readers,
        if plan.readers == 1 { "file" } else { "files" },
    );
    Ok(())
}

/// How far a round is, in a few words: "60%, 1,200 of 2,000 passages, about 2 minutes left".
fn describe_round(round: &protocol::RoundStatus) -> String {
    let (done, total) = (round.done.round() as u64, round.total.round() as u64);
    let amount = match round.round.unit() {
        "seconds" => format!(
            "{} of {} of sound",
            tools::estimate::describe(round.done).replacen("about ", "", 1),
            tools::estimate::describe(round.total).replacen("about ", "", 1)
        ),
        unit => format!("{done} of {total} {unit}"),
    };
    let state = match round.state {
        protocol::RoundState::Done => return format!("done, {amount}"),
        protocol::RoundState::Running => "running",
        protocol::RoundState::Queued => "waits its turn",
        protocol::RoundState::Waiting => "waiting",
    };
    // Rounded down: 100% only once nothing waits.
    let share = if round.total > 0.0 { (round.done / round.total * 100.0).floor() as u64 } else { 0 };
    let left = round.seconds_left.map(|left| format!(", {} left", tools::estimate::describe(left))).unwrap_or_default();
    format!("{share}%, {amount}, {state}{left}")
}
