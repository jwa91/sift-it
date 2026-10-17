mod av;
mod client;
mod config;
mod crawl;
mod daemon;
mod embedder;
mod engine;
mod estimate;
mod eval;
mod finder;
mod imaging;
mod indexer;
mod llama;
mod mac;
mod models;
mod open;
mod power;
mod present;
mod preview;
mod readers;
mod search;
mod selftest;
mod serve;
mod service;
mod speech;
mod store;
mod term;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Mode};
use crawl::Kind;
use std::path::PathBuf;

/// Sift It: search your files by what's in them, with EmbeddingGemma 2 on this Mac.
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
        /// Only these kinds: plaintext, document, image, screenshot, audio, video, code
        #[arg(long, value_delimiter = ',')]
        kind: Vec<String>,
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
    /// Time search over an index of random vectors (for tuning)
    #[command(hide = true)]
    BenchSearch {
        #[arg(long, default_value_t = 200_000)]
        vectors: usize,
    },
    /// Print the query vectors of these queries, one JSON array per line (for analysis)
    #[command(hide = true)]
    EmbedQuery { queries: Vec<String> },
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
    /// Print the LaunchAgent plists it would install
    Plist,
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
            return finder::run("");
        }
        <Cli as clap::CommandFactory>::command().print_help()?;
        return Ok(());
    };
    match command {
        Command::Find { words } => finder::run(&words.join(" ")),
        Command::Folders { action } => folders(action),
        Command::Config { key, value } => configure(key, value),
        Command::Estimate { folders, json } => {
            let config = Config::load()?;
            let folders: Vec<PathBuf> = if folders.is_empty() {
                config.sources.paths.clone()
            } else {
                folders.iter().map(|f| config::expand(f)).collect()
            };
            let estimates: Vec<_> = folders.into_iter().map(|folder| estimate::estimate(&config, folder)).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&estimates)?);
            } else {
                for e in &estimates {
                    let kinds = e.by_kind.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ");
                    println!("{}: {} files ({kinds}), {}", e.folder.display(), e.files, estimate::describe(e.seconds));
                }
            }
            Ok(())
        }
        Command::Index { json, reindex } => index(json, reindex),
        Command::Search { words, kind, limit, by, json } => search_once(&words.join(" "), &kind, limit, by, json),
        Command::Open { target, at, reveal } => {
            let place = match target.parse::<usize>() {
                Ok(number) if !std::path::Path::new(&target).exists() => open::recall(number)?,
                _ => open::Place::file(&config::expand(&target), at)?,
            };
            println!("{}", open::open(&place, reveal)?);
            Ok(())
        }
        Command::Serve => serve::serve(),
        Command::Engine { action: None } => engine::run(&config::index_path(), &config::models_dir()),
        Command::Engine { action: Some(action) } => {
            let Some(mut engine) = client::Client::running() else {
                println!("the engine isn't running");
                return Ok(());
            };
            let op = match action {
                EngineAction::Stop => "stop",
                EngineAction::Unload => "unload",
            };
            engine.request(op, serde_json::json!({}))?;
            Ok(())
        }
        Command::Daemon => daemon::run(),
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(),
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Status => service::status(),
            ServiceAction::Plist => {
                print!("{}", service::plists()?);
                Ok(())
            }
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
                if let Some(mut engine) = client::Client::running()
                    && engine.request("status", serde_json::json!({}))?["meaning"] == "unavailable"
                {
                    engine.request("unload", serde_json::json!({}))?;
                }
                Ok(())
            }),
        },
        Command::Eval { file, verbose } => {
            // The engine's own answers, in this process: what's measured is what front ends get.
            let core = engine::Core::new(&config::index_path(), &config::models_dir(), Box::new(|_| {}))?;
            let outcome = eval::run(&file, &core, verbose);
            // The model goes before the process does (llama.cpp aborts when its Metal device is freed under one).
            core.unload();
            outcome
        }
        Command::Read { path, tokens } => {
            let path = config::expand(&path.to_string_lossy());
            let config = Config::load()?;
            let kind = crawl::kind_of(&config, &path)
                .ok_or_else(|| anyhow::anyhow!("Sift It doesn't read .{} files", crawl::extension_of(&path)))?;
            let settings = config.reading.resolve(&path, kind);
            let mut model = if tokens {
                Some(embedder::Embedder::load(&embedder::ModelFiles::in_folder(&config::models_dir())?, true)?)
            } else {
                None
            };
            let started = std::time::Instant::now();
            let mut count = 0;
            let mut total_tokens = 0;
            let info = readers::read(&path, kind, &settings, &mut |piece| {
                count += 1;
                let what = match &piece.payload {
                    readers::Payload::None => "words only".to_string(),
                    readers::Payload::Text(text) => match model.as_mut() {
                        Some(model) => {
                            let n = model.count_tokens(text);
                            total_tokens += n;
                            format!("text, {n} tokens")
                        }
                        None => format!("text, {} chars", text.len()),
                    },
                    readers::Payload::Image(image) => format!("picture {}×{}", image.width, image.height),
                    readers::Payload::Audio(samples) => format!("sound {:.1} s", samples.len() as f64 / 16000.0),
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
        Command::BenchSearch { vectors } => bench_search(vectors),
        Command::BenchEmbed => {
            let files = embedder::ModelFiles::in_folder(&config::models_dir())?;
            let mut model = embedder::Embedder::load(&files, false)?;
            let passage = "The tenant pays a security deposit of two months' rent, returned within fourteen days of the \
                end of the lease minus the cost of any damage beyond normal wear. Gas, water and electricity are paid by \
                the tenant; internet is included in the rent. ".repeat(4);
            let texts: Vec<String> = (0..64).map(|i| format!("title: doc {i} | text: {passage}")).collect();
            let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
            model.embed_texts(&refs[..4])?;
            let started = std::time::Instant::now();
            model.embed_texts(&refs)?;
            let seconds = started.elapsed().as_secs_f64();
            println!("64 passages of {} chars: {:.2} s, {:.1} passages/s", passage.len(), seconds, 64.0 / seconds);
            let started = std::time::Instant::now();
            for text in &refs {
                model.embed_texts(&[text])?;
            }
            let seconds = started.elapsed().as_secs_f64();
            println!("the same one at a time: {:.2} s, {:.1} passages/s", seconds, 64.0 / seconds);
            let image = imaging::Image { width: 1024, height: 768, rgb: vec![128; 1024 * 768 * 3] };
            for budget in [70, 140, 280] {
                let fitted = imaging::fit(&image, budget)?;
                model.embed_image(&fitted)?;
                let started = std::time::Instant::now();
                for _ in 0..10 {
                    model.embed_image(&fitted)?;
                }
                println!("picture at {budget} tokens: {:.0} ms", started.elapsed().as_secs_f64() * 100.0);
            }
            let sound: Vec<f32> = (0..embedder::AUDIO_WINDOW).map(|i| ((i as f32) * 0.05).sin() * 0.1).collect();
            model.embed_audio(&sound)?;
            let started = std::time::Instant::now();
            for _ in 0..5 {
                model.embed_audio(&sound)?;
            }
            println!("30 s of sound: {:.0} ms", started.elapsed().as_secs_f64() * 200.0);
            Ok(())
        }
        Command::EmbedQuery { queries } => {
            let files = embedder::ModelFiles::in_folder(&config::models_dir())?;
            let mut encoder = embedder::Embedder::load(&files, true)?;
            for query in queries {
                let prompt = search::Request { q: query, ..Default::default() }.prompt();
                println!("{}", serde_json::to_string(&encoder.embed_text(&prompt)?)?);
            }
            Ok(())
        }
        Command::Selftest { folder } => {
            let models = embedder::ModelFiles::in_folder(&config::models_dir())?;
            match folder {
                Some(folder) => selftest::run(&models, &folder),
                None => selftest::run_built_in(&models),
            }
        }
    }
}

fn folders(action: Option<FolderAction>) -> Result<()> {
    let mut config = Config::load()?;
    // The folder an ignore pattern names by its full path, if it does ("~/Documents/Old").
    let folder_of = |pattern: &config::Pattern| {
        let source = pattern.as_str();
        (source.starts_with('/') || source.starts_with("~/")).then(|| config::expand(source))
    };
    let changes = action.is_some();
    match action {
        None => {}
        Some(FolderAction::Add { paths }) => {
            for path in paths {
                let path = config::expand(&path);
                if !path.is_dir() {
                    bail!("not a folder: {}", path.display());
                }
                if !config.sources.paths.contains(&path) {
                    config.sources.paths.push(path);
                }
            }
        }
        Some(FolderAction::Remove { paths }) => {
            for path in paths {
                let path = config::expand(&path);
                config.sources.paths.retain(|folder| folder != &path);
                config.sources.ignore.retain(|pattern| !folder_of(pattern).is_some_and(|f| f.starts_with(&path)));
            }
        }
        Some(FolderAction::Include { paths }) => {
            for path in paths {
                let path = config::expand(&path);
                config.sources.ignore.retain(|pattern| folder_of(pattern).as_ref() != Some(&path));
            }
        }
        Some(FolderAction::Exclude { paths }) => {
            for path in paths {
                let path = config::expand(&path);
                if !config.sources.ignore.iter().any(|pattern| folder_of(pattern).as_ref() == Some(&path)) {
                    config.sources.ignore.push(config::Pattern::new(&path.to_string_lossy())?);
                }
            }
        }
    }
    if changes {
        config.save()?;
    }
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

/// Prints config.json, or the value at a dotted key, after setting it when there's a value.
fn configure(key: Option<String>, value: Option<String>) -> Result<()> {
    let mut config = Config::load()?;
    if let (Some(key), Some(value)) = (&key, &value) {
        config.set(key, value)?;
        config.save()?;
    }
    // All of it in config.json's order; a part through JSON values, which sort their keys.
    let shown = match key.as_deref() {
        None => serde_json::to_string_pretty(&config)?,
        Some(key) => serde_json::to_string_pretty(&config.get(Some(key))?)?,
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
    if let Some(pid) = daemon::running_pid() {
        if reindex {
            bail!("the background indexer is running (pid {pid}); stop it before --reindex");
        }
        unsafe { libc::kill(pid, libc::SIGUSR1) };
        eprintln!("the background indexer (pid {pid}) is looking for changes now");
        return Ok(());
    }
    let store = store::Store::open(&config::index_path())?;
    if reindex {
        store.requeue_all()?;
    }
    let mut indexer = indexer::Indexer::new(store, config::models_dir())?;
    let mut report = |event: indexer::Event| {
        if json {
            println!("{}", serde_json::to_string(&event).unwrap());
            return;
        }
        match event {
            indexer::Event::Sync { files, added, changed, removed, pending, ms, .. } => eprintln!(
                "{files} files in the folders ({added} new, {changed} changed, {removed} gone), {pending} to index ({ms} ms)"
            ),
            indexer::Event::Started { pending } => eprintln!("indexing {pending} files…"),
            indexer::Event::Queued { count } => eprintln!("{count} more files joined the queue"),
            indexer::Event::File { path, segments, vectors, ms, .. } => {
                eprintln!("  {path}: {segments} segments, {vectors} vectors, {ms} ms")
            }
            indexer::Event::Failed { path, error } => eprintln!("  {path}: {error}"),
            indexer::Event::Finished { indexed, failed, seconds, .. } => {
                eprintln!("{indexed} files indexed, {failed} failed, in {seconds:.1} s")
            }
            indexer::Event::Transcribing { .. } => {}
            indexer::Event::Transcribed { path, passages, seconds, ms } => {
                eprintln!("  {path}: {passages} passages of speech in {seconds:.0} s of sound, {ms} ms")
            }
            indexer::Event::TranscribeFailed { path, error } => eprintln!("  {path}: {error}"),
        }
    };
    let sync = indexer.sync(&config)?;
    report(sync);
    if let Err(error) = indexer.index_pending(&config, &mut report, &|| false, &mut Vec::new) {
        // This process can't go on (indexer::Stuck); a new one can, with the file set aside.
        if error.is::<indexer::Stuck>() {
            bail!("{error}. Run `sift-it index` again for the rest");
        }
        return Err(error);
    }
    // Then what's said in recordings and videos, when the speech model is there.
    if !indexer.store.transcripts_pending(1)?.is_empty() {
        match speech::Transcriber::start() {
            Ok(mut transcriber) => {
                if !json {
                    eprintln!("writing down what's said in recordings and videos…");
                }
                while indexer.transcribe_next(&mut transcriber, &mut report, &|| false)? {}
            }
            Err(error) => eprintln!("what's said in recordings isn't written down: {error:#}"),
        }
    }
    Ok(())
}

fn search_once(q: &str, kinds: &[String], limit: usize, by: Option<Mode>, json: bool) -> Result<()> {
    let kinds = kinds
        .iter()
        .map(|name| Kind::parse(name).ok_or_else(|| anyhow::anyhow!("no such kind: {name}")))
        .collect::<Result<Vec<_>>>()?;
    let request = search::Request { q: q.to_string(), mode: by, kinds, limit, prefix_last: false };
    let mut client = client::Client::connect()?;
    // By meaning, it waits for the engine to load the model if it hasn't yet.
    let response = client.search(&request, true)?;
    open::remember(q, &response.hits);
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    let terminal = term::Terminal::detect();
    if !terminal.tty {
        // Piped: one path per line, for xargs and friends.
        for hit in &response.hits {
            println!("{}", hit.path);
        }
        return Ok(());
    }
    present::print(&terminal, &response, q, &mut client)
}

fn status(json: bool) -> Result<()> {
    let store = store::Store::open(&config::index_path())?;
    let stats = store.stats()?;
    let config = Config::load()?;
    let models = models::status(&config::models_dir());
    let transcripts = store.transcripts()?;
    let queued = store.queued(5)?;
    // The engine, if it's running (status doesn't start it): what it holds.
    let engine = client::Client::running().and_then(|mut engine| engine.request("status", serde_json::json!({})).ok());
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
                    "unavailable": speech::unavailable(),
                },
                "engine": engine,
                "config": config,
                "models": models,
            }))?
        );
        return Ok(());
    }
    println!("index:   {}", config::index_path().display());
    println!(
        "files:   {} ({} indexed, {} waiting, {} unreadable)",
        stats.files, stats.done, stats.pending, stats.failed
    );
    println!("vectors: {} in {} segments", stats.vectors, stats.segments);
    if queued.files > 0 {
        // The work still to do, and where most of it is: what `ignore` or a lower depth could leave out.
        println!("waiting: {} files, {} of reading", queued.files, estimate::describe(queued.seconds));
        for heavy in queued.heaviest.iter().filter(|heavy| heavy.seconds >= 60.0) {
            println!("         {} ({})", heavy.path, estimate::describe(heavy.seconds));
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
        Some(engine) => match engine["meaning"].as_str().unwrap_or("") {
            "ready" => format!("running, {} vectors in memory for search by meaning", engine["vectors"]),
            "loading" => "running, loading the model and the vectors".to_string(),
            "unavailable" => format!("running, by words only: {}", engine["error"].as_str().unwrap_or("no model")),
            _ => "running, nothing loaded (search by meaning loads it)".to_string(),
        },
    };
    println!("engine:  {engine}");
    let speech = match speech::unavailable() {
        Some(why) => why,
        None if transcripts.done < transcripts.total => format!(
            "{} of {} recordings and videos written down, {} of sound to go",
            transcripts.done,
            transcripts.total,
            estimate::describe(transcripts.seconds).trim_start_matches("about ")
        ),
        None => format!("{} of {} recordings and videos written down", transcripts.done, transcripts.total),
    };
    println!("speech:  {speech}");
    Ok(())
}

/// An index of random unit vectors (and words), then searches over it.
fn bench_search(count: usize) -> Result<()> {
    let folder = std::env::temp_dir().join(format!("sift-it-bench-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    let path = folder.join("index.sqlite");
    let started = std::time::Instant::now();
    {
        let mut store = store::Store::open(&path)?;
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        let per_file = 10;
        for file in 0..count.div_ceil(per_file) {
            let found = crawl::Found {
                path: folder.join(format!("file{file}.txt")),
                kind: Kind::Plaintext,
                size: 1,
                modified: 0.0,
                settings: config::Settings::BASE,
            };
            let id = store.queue(&found, None, 0.0)?;
            let segments: Vec<store::NewSegment> = (0..per_file)
                .map(|n| {
                    let mut vector: Vec<f32> = (0..embedder::DIM).map(|_| random()).collect();
                    embedder::normalize(&mut vector);
                    store::NewSegment {
                        segment: store::Segment::Passage,
                        loc: Some(n as f64),
                        loc_end: None,
                        text: Some(format!("passage {n} of file {file} about topic{}", file % 97)),
                        vector: Some(vector),
                    }
                })
                .collect();
            let row = store::FileRow {
                id,
                path: found.path.clone(),
                kind: Kind::Plaintext,
                size: 1,
                modified: 0.0,
                reading: found.reading(),
                speech: false,
            };
            store.finish_file(&row, &segments, &serde_json::json!({}), false)?;
        }
    }
    println!("built an index of {count} vectors in {:.1} s", started.elapsed().as_secs_f64());
    let started = std::time::Instant::now();
    let mut searcher = search::Searcher::open(&path, config::Ranking::default())?;
    searcher.load_vectors()?;
    println!("loaded it in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
    let mut query: Vec<f32> = (0..embedder::DIM).map(|i| ((i * 7919) % 101) as f32 - 50.0).collect();
    embedder::normalize(&mut query);
    for q in ["topic42", "something else entirely"] {
        let request = search::Request { q: q.into(), mode: None, kinds: vec![], limit: 40, prefix_last: true };
        searcher.search(&request, Some(&query))?;
        let started = std::time::Instant::now();
        for _ in 0..20 {
            searcher.search(&request, Some(&query))?;
        }
        println!("search {q:?}: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0 / 20.0);
    }
    std::fs::remove_dir_all(&folder)?;
    Ok(())
}
