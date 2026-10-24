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
    /// Experiment: embed photos, video frames and sound, a fixed number of each, timed
    #[command(hide = true)]
    BenchMedia {
        #[arg(default_value_t = 100)]
        count: usize,
    },
    /// Experiment: embed the texts in a file with the text encoder that's on, timed (for comparing encoders)
    #[command(hide = true)]
    BenchTexts {
        file: PathBuf,
        /// Write the vectors here
        #[arg(long)]
        out: Option<PathBuf>,
        /// Load the text model alone, as queries do (else with the encoders, as the indexer does)
        #[arg(long)]
        text_only: bool,
        /// Go round the texts until this many seconds have passed
        #[arg(long, default_value_t = 0.0)]
        seconds: f64,
        /// Texts a model call (the indexer's is 16)
        #[arg(long, default_value_t = 16)]
        batch: usize,
    },
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
                    let speech = if e.speech_seconds > 0.0 {
                        format!(", {} of it writing down what's said", tools::estimate::describe(e.speech_seconds))
                    } else {
                        String::new()
                    };
                    println!(
                        "{}: {} files ({kinds}), {}{speech}",
                        e.folder.display(),
                        e.files,
                        tools::estimate::describe(e.seconds)
                    );
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
        Command::Engine { action: None } => services::engine::run(&config::index_path(), &config::models_dir()),
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
                println!("{}", serde_json::to_string_pretty(&models::status(&config::models_dir()))?);
                Ok(())
            }
            ModelAction::Download { json } => models::download(&config::models_dir(), &mut |progress| {
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
            let core = services::engine::Core::new(&config::index_path(), &config::models_dir(), Box::new(|_| {}))?;
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
                Some(embedder::Embedder::load(&embedder::ModelFiles::in_folder(&config::models_dir())?, true)?)
            } else {
                None
            };
            let started = std::time::Instant::now();
            let mut count = 0;
            let mut total_tokens = 0;
            let info = ingest::read(&path, kind, &settings, &mut |piece| {
                count += 1;
                let what = match &piece.payload {
                    ingest::Payload::None => "words only".to_string(),
                    ingest::Payload::Text(text) => match model.as_mut() {
                        Some(model) => {
                            let n = model.count_tokens(text);
                            total_tokens += n;
                            format!("text, {n} tokens")
                        }
                        None => format!("text, {} chars", text.len()),
                    },
                    ingest::Payload::Image(image) => format!("picture {}×{}", image.width, image.height),
                    ingest::Payload::Audio(samples) => {
                        format!("sound {:.1} s", samples.len() as f64 / embedder::AUDIO_RATE as f64)
                    }
                };
                let at = piece.loc.map(|l| format!(" at {l}")).unwrap_or_default();
                let text = piece.text.as_deref().unwrap_or("").replace('\n', " ");
                println!(
                    "{:<8}{at:<10} {what:<22} {}",
                    piece.segment.as_str(),
                    text.chars().take(70).collect::<String>()
                );
                Ok(())
            })?;
            println!(
                "{count} pieces in {:.0} ms{}; {info}",
                started.elapsed().as_secs_f64() * 1000.0,
                if tokens { format!(", {total_tokens} tokens") } else { String::new() }
            );
            Ok(())
        }
        Command::BenchSearch { vectors } => tools::bench::search(vectors),
        Command::BenchEmbed => tools::bench::embed(),
        Command::BenchMedia { count } => tools::bench::media(count),
        Command::BenchTexts { file, out, text_only, seconds, batch } => {
            tools::bench::texts(&file, out.as_deref(), text_only, seconds, batch)
        }
        Command::Selftest { folder } => {
            let models = embedder::ModelFiles::in_folder(&config::models_dir())?;
            match folder {
                Some(folder) => tools::selftest::run(&models, &folder),
                None => tools::selftest::run_built_in(&models),
            }
        }
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
        index.requeue_all()?;
    }
    let mut indexer = ingest::Indexer::new(index, config::models_dir())?;
    let mut report = |event: ingest::Event| {
        if json {
            println!("{}", serde_json::to_string(&event).unwrap());
            return;
        }
        match event {
            ingest::Event::Sync { files, added, changed, removed, pending, ms, .. } => eprintln!(
                "{files} files in the folders ({added} new, {changed} changed, {removed} gone), {pending} to index ({ms} ms)"
            ),
            ingest::Event::Started { pending } => eprintln!("indexing {pending} files…"),
            ingest::Event::Queued { count } => eprintln!("{count} more files joined the queue"),
            ingest::Event::File { path, segments, vectors, ms, .. } => {
                eprintln!("  {path}: {segments} segments, {vectors} vectors, {ms} ms")
            }
            ingest::Event::Failed { path, error } => eprintln!("  {path}: {error}"),
            ingest::Event::Finished { indexed, failed, seconds, .. } => {
                eprintln!("{indexed} files indexed, {failed} failed, in {seconds:.1} s")
            }
            ingest::Event::Transcribing { .. } => {}
            ingest::Event::Transcribed { path, passages, seconds, ms } => {
                eprintln!("  {path}: {passages} passages of speech in {seconds:.0} s of sound, {ms} ms")
            }
            ingest::Event::TranscribeFailed { path, error } => eprintln!("  {path}: {error}"),
            ingest::Event::SpeechStopped { error } => {
                eprintln!("what's said in recordings is written down after the reading: {error}")
            }
        }
    };
    let sync = indexer.sync(&config)?;
    report(sync);
    // What's said in recordings and videos is written down beside the reading when memory allows, else after it.
    let mut helper = None;
    let beside = resources::IngestPlan::of(&config.indexing).beside && adapters::speech::unavailable().is_none();
    if let Err(error) =
        indexer.index_pending(&config, &mut report, &|| false, &mut Vec::new, beside.then_some(&mut helper))
    {
        // This process can't go on (ingest::Stuck); a new one can, with the file set aside.
        if error.is::<ingest::Stuck>() {
            bail!("{error}. Run `sift-it index` again for the rest");
        }
        return Err(error);
    }
    // Then what's said in recordings and videos that's left, when the speech model is there.
    if indexer.index.next_transcript()?.is_some() {
        match helper.map_or_else(adapters::speech::Transcriber::start, Ok) {
            Ok(mut transcriber) => {
                if !json {
                    eprintln!("writing down what's said in recordings and videos…");
                }
                while indexer.transcribe_next(&mut transcriber, &mut report, &|| false)? {}
            }
            Err(error) => eprintln!("what's said in recordings isn't written down: {error:#}"),
        }
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
    let models = models::status(&config::models_dir());
    let transcripts = index.transcripts()?;
    let queued = index.queued(5)?;
    let plan = resources::IngestPlan::of(&config.indexing);
    // The engine, if it's running (status doesn't start it): what it holds.
    let engine =
        clients::Client::running().and_then(|mut engine| engine.request::<protocol::Status>(protocol::Op::Status).ok());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "index": config::index_path().display().to_string(),
                "stats": stats,
                "queued": queued,
                "speech": {
                    "done": transcripts.done,
                    "total": transcripts.total,
                    "seconds": transcripts.seconds,
                    "unavailable": adapters::speech::unavailable(),
                },
                "indexing": {"memory_mb": plan.memory >> 20, "readers": plan.readers, "speech_beside": plan.beside},
                "engine": engine,
                "config": config,
                "models": models,
            }))?
        );
        return Ok(());
    }
    println!("index:   {}", config::index_path().display());
    // Rounded down: 100% only once nothing waits.
    let share = match stats.pending {
        0 => String::new(),
        pending => format!(", {}% done", (stats.files - pending) * 100 / stats.files),
    };
    println!(
        "files:   {}{share} ({} indexed, {} waiting, {} unreadable)",
        stats.files, stats.done, stats.pending, stats.failed
    );
    println!("vectors: {} in {} segments", stats.vectors, stats.segments);
    if queued.files > 0 {
        // The work still to do, and where most of it is: what `ignore` or a lower depth could leave out.
        println!("waiting: {} files, {} of reading", queued.files, tools::estimate::describe(queued.seconds));
        for heavy in queued.heaviest.iter().filter(|heavy| heavy.seconds >= 60.0) {
            println!("         {} ({})", heavy.path, tools::estimate::describe(heavy.seconds));
        }
    }
    let mut kinds: Vec<_> = stats.by_kind.iter().collect();
    kinds.sort();
    println!("kinds:   {}", kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "));
    let folders: Vec<String> = config.sources.paths.iter().map(|f| f.display().to_string()).collect();
    println!("folders: {}", folders.join(", "));
    println!(
        "model:   {}",
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
    println!("engine:  {engine}");
    let speech = match adapters::speech::unavailable() {
        Some(why) => why,
        None if transcripts.done < transcripts.total => format!(
            "{} of {} recordings and videos written down, {} of sound to go ({})",
            transcripts.done,
            transcripts.total,
            tools::estimate::describe(transcripts.seconds).trim_start_matches("about "),
            tools::estimate::describe(ingest::discover::speech(transcripts.seconds))
        ),
        None => format!("{} of {} recordings and videos written down", transcripts.done, transcripts.total),
    };
    println!("speech:  {speech}");
    println!(
        "memory:  indexing takes up to {} MB: {} {} read at once, speech written down {} the reading",
        plan.memory >> 20,
        plan.readers,
        if plan.readers == 1 { "file" } else { "files" },
        if plan.beside { "beside" } else { "after" }
    );
    Ok(())
}
