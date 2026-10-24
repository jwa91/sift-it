//! The benches: `sift-it bench-search` (searching in each form of the vectors) and `sift-it bench-embed` (the model).

use crate::config;
use crate::index;
use crate::ingest::discover::crawl;
use crate::query;
use anyhow::{Context, Result};
use protocol::{Kind, Search};
use std::io::{BufRead, Write};
use std::path::Path;

/// How fast the model embeds here: passages in a batch and one at a time, pictures at three token budgets, and 30 s
/// of sound.
pub fn embed() -> Result<()> {
    let files = embedder::ModelFiles::in_folder(&config::models_dir())?;
    let mut model = embedder::Embedder::load(&files, false)?;
    let passage = "The tenant pays a security deposit of two months' rent, returned within fourteen days of the \
        end of the lease minus the cost of any damage beyond normal wear. Gas, water and electricity are paid by \
        the tenant; internet is included in the rent. "
        .repeat(4);
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
    let image = embedder::imaging::Image { width: 1024, height: 768, rgb: vec![128; 1024 * 768 * 3] };
    for budget in [70, 140, 280] {
        let fitted = embedder::imaging::fit(&image, budget)?;
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

/// An index of random vectors around topics (and words), then searches over it in each form of the vectors: how long
/// mapping them takes, the first search (its rows come into memory) and the ones after, what's in memory then, and how
/// much of the exact top 20 the others keep. Mapped rows aren't in the footprint: macOS can drop them under pressure
/// and read them again.
pub fn search(count: usize) -> Result<()> {
    const TOPICS: usize = 1000;
    let folder = std::env::temp_dir().join(format!("sift-it-bench-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    let path = folder.join("index.sqlite");
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32 - 0.5
    };
    // A vector near a topic: as close to it as to everything else.
    let mut near = |topic: &[f32]| {
        let mut noise: Vec<f32> = (0..embedder::DIM).map(|_| random()).collect();
        embedder::normalize(&mut noise);
        let mut vector: Vec<f32> = topic.iter().zip(&noise).map(|(t, n)| t + n).collect();
        embedder::normalize(&mut vector);
        vector
    };
    let origin = vec![0.0; embedder::DIM];
    let topics: Vec<Vec<f32>> = (0..TOPICS).map(|_| near(&origin)).collect();
    let started = std::time::Instant::now();
    {
        let mut index = index::Writer::open(&path)?;
        let per_file = 10;
        for file in 0..count.div_ceil(per_file) {
            let found = crawl::Found {
                path: folder.join(format!("file{file}.txt")),
                kind: Kind::Plaintext,
                size: 1,
                modified: 0.0,
                settings: config::Settings::BASE,
            };
            let id = index.queue(&found.new_file(), None, 0.0)?;
            let segments: Vec<index::NewSegment> = (0..per_file)
                .map(|n| index::NewSegment {
                    segment: protocol::Segment::Passage,
                    loc: Some(n as f64),
                    loc_end: None,
                    text: Some(format!("passage {n} of file {file} about topic{}", file % 97)),
                    vector: Some(near(&topics[(file * per_file + n) % TOPICS])),
                })
                .collect();
            let row = index::FileRow {
                id,
                path: found.path.clone(),
                kind: Kind::Plaintext,
                size: 1,
                modified: 0.0,
                reading: found.reading(),
                speech: false,
            };
            index.finish_file(&row, &segments, &serde_json::json!({}), false)?;
        }
    }
    println!("built an index of {count} vectors around {TOPICS} topics in {:.1} s", started.elapsed().as_secs_f64());
    let queries: Vec<Vec<f32>> = (0..20).map(|i| near(&topics[i * 37 % TOPICS])).collect();
    let request = |q: &str| Search { q: q.into(), limit: 20, prefix_last: true, ..Default::default() };
    let ms = |started: std::time::Instant| started.elapsed().as_secs_f64() * 1000.0;
    let mut exact: Vec<Vec<String>> = Vec::new();
    for scan in [protocol::Scan::ExactF32, protocol::Scan::ExactF16, protocol::Scan::Sketch] {
        let started = std::time::Instant::now();
        let search = config::Search { vectors: scan, ..Default::default() };
        let mut searcher = query::Searcher::open(&path, config::Ranking::default(), search)?;
        searcher.load_vectors()?;
        let loaded = ms(started);
        let started = std::time::Instant::now();
        searcher.search(&request("topic42"), Some(&queries[0]))?;
        let first = ms(started);
        let started = std::time::Instant::now();
        let mut tops = Vec::new();
        for query in &queries {
            let response = searcher.search(&request("something else entirely"), Some(query))?;
            tops.push(response.hits.into_iter().map(|hit| hit.path).collect::<Vec<_>>());
        }
        let then = ms(started) / queries.len() as f64;
        if exact.is_empty() {
            exact = tops.clone();
        }
        let kept: usize =
            tops.iter().zip(&exact).map(|(top, exact)| top.iter().filter(|p| exact.contains(p)).count()).sum();
        println!(
            "{}: mapped in {loaded:.0} ms, first search {first:.1} ms, then {then:.1} ms; footprint {} MB; keeps {kept} of \
             the exact top 20s' {}",
            serde_json::json!(scan).as_str().unwrap_or("?"),
            footprint() >> 20,
            exact.iter().map(Vec::len).sum::<usize>()
        );
    }
    std::fs::remove_dir_all(&folder)?;
    Ok(())
}

/// The memory macOS counts against this process (its footprint).
fn footprint() -> u64 {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let buffer = &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t;
    if unsafe { libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V2, buffer) } != 0 {
        return 0;
    }
    info.ri_phys_footprint
}

/// Experiment: the texts in `file` (one JSON string a line, as SIFT_IT_DUMP_TEXTS writes them) embedded in batches of
/// `batch` (16, as the indexer does), by whichever text encoder is on (SIFT_IT_COREML: Core ML, else llama.cpp). Says how long
/// loading took and how many passages and tokens a second, and writes the vectors to `out` (768 little-endian f32 a
/// text, in order). With `seconds`, it goes round the texts until that long has passed (for energy), and prints the
/// wall-clock start and end so power samples can be lined up with it.
pub fn texts(file: &Path, out: Option<&Path>, text_only: bool, seconds: f64, batch: usize) -> Result<()> {
    let texts: Vec<String> = std::io::BufReader::new(std::fs::File::open(file)?)
        .lines()
        .map(|line| Ok(serde_json::from_str::<String>(&line?)?))
        .collect::<Result<_>>()
        .context("reading the texts")?;
    let files = embedder::ModelFiles::in_folder(&config::models_dir())?;
    let started = std::time::Instant::now();
    let mut model = embedder::Embedder::load(&files, text_only)?;
    let load = started.elapsed().as_secs_f64();
    let encoder = if std::env::var_os("SIFT_IT_COREML").is_some() { "coreml" } else { "llama.cpp" };
    eprintln!("{encoder}: loaded in {load:.2} s {}", model.coreml_report().unwrap_or(""));
    let tokens: usize =
        texts.iter().map(|t| model.count_tokens(t).min(if encoder == "coreml" { 512 } else { usize::MAX })).sum();
    let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let wall_start = now();
    let started = std::time::Instant::now();
    let mut vectors = Vec::with_capacity(texts.len());
    let mut rounds = 0;
    loop {
        for texts in refs.chunks(batch) {
            let embedded = model.embed_texts(texts)?;
            if rounds == 0 {
                vectors.extend(embedded);
            }
        }
        rounds += 1;
        if seconds > 0.0 {
            // Each round's pace, to see a fanless Mac slow down as it warms.
            eprintln!(
                "round {rounds}: {:.0} s, {:.1} passages/s so far",
                started.elapsed().as_secs_f64(),
                (texts.len() * rounds) as f64 / started.elapsed().as_secs_f64()
            );
        }
        if started.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let wall_end = now();
    let passages = (texts.len() * rounds) as f64;
    println!(
        "{}",
        serde_json::json!({
            "encoder": encoder, "texts": texts.len(), "rounds": rounds, "tokens": tokens * rounds,
            "seconds": elapsed, "passages_per_second": passages / elapsed,
            "tokens_per_second": (tokens * rounds) as f64 / elapsed, "load_seconds": load,
            "wall_start": wall_start, "wall_end": wall_end,
        })
    );
    let profile = embedder::op_profile();
    if !profile.is_empty() {
        let total: f64 = profile.iter().map(|r| r.2).sum();
        eprintln!("{:<16} {:>7} {:>9} {:>6} {:>9}", "op", "nodes", "ms", "%", "µs/node");
        for (op, nodes, seconds) in &profile {
            eprintln!(
                "{op:<16} {nodes:>7} {:>9.1} {:>5.1}% {:>9.1}",
                seconds * 1e3,
                100.0 * seconds / total,
                seconds * 1e6 / *nodes as f64
            );
        }
    }
    if let Some(out) = out {
        let mut writer = std::io::BufWriter::new(std::fs::File::create(out)?);
        for vector in &vectors {
            for value in vector {
                writer.write_all(&value.to_le_bytes())?;
            }
        }
        writer.flush()?;
    }
    Ok(())
}

/// Experiment: fixed work for timing and energy: `count` photos at 140 tokens (the indexer's budget for a photo),
/// `count` video frames at 70, and `count` 30 s windows of sound, each phase timed with its wall-clock start and end.
pub fn media(count: usize) -> Result<()> {
    let files = embedder::ModelFiles::in_folder(&config::models_dir())?;
    let mut model = embedder::Embedder::load(&files, false)?;
    // Noise, so nothing about the content is cheaper than a photo.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let image = embedder::imaging::Image {
        width: 1600,
        height: 1200,
        rgb: (0..1600 * 1200 * 3).map(|_| noise() as u8).collect(),
    };
    let sound: Vec<f32> =
        (0..embedder::AUDIO_WINDOW).map(|_| (noise() as u32 as f32 / u32::MAX as f32 - 0.5) * 0.2).collect();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let phase = |name: &str, run: &mut dyn FnMut() -> Result<()>| -> Result<()> {
        run()?;
        let wall_start = now();
        let started = std::time::Instant::now();
        for _ in 0..count {
            run()?;
        }
        let seconds = started.elapsed().as_secs_f64();
        println!(
            "{}",
            serde_json::json!({"phase": name, "count": count, "seconds": seconds, "ms_each": seconds * 1000.0 / count as f64,
                "wall_start": wall_start, "wall_end": now()})
        );
        Ok(())
    };
    let photo = embedder::imaging::fit(&image, 140)?;
    phase("photo-140", &mut || model.embed_image(&photo).map(drop))?;
    let frame = embedder::imaging::fit(&image, 70)?;
    phase("frame-70", &mut || model.embed_image(&frame).map(drop))?;
    phase("sound-30s", &mut || model.embed_audio(&sound).map(drop))?;
    Ok(())
}
