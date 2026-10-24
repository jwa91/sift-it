//! The benches: `sift-it bench-search` (searching in each form of the vectors) and `sift-it bench-embed` (the model).

use crate::config;
use crate::index;
use crate::ingest::discover::crawl;
use crate::models;
use crate::query;
use anyhow::Result;
use protocol::{Kind, Search};
use std::path::Path;

/// How fast the model embeds here: a query as the engine embeds it, passages in a batch and one at a time, pictures at
/// three token budgets, and 30 s of sound. And what each encoder takes to load, and holds.
pub fn embed() -> Result<()> {
    let text = models::TEXT.require()?;
    let mut queries = embedder::Embedder::load(&text, embedder::Inputs::Queries)?;
    let mut times: Vec<f64> = (0..50)
        .map(|i| {
            let started = std::time::Instant::now();
            queries.embed_text(&embedder::prompts::query(&format!("the warranty for washing machine {i}")))?;
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        })
        .collect::<Result<_>>()?;
    drop(queries);
    times.sort_by(f64::total_cmp);
    println!("a query: {:.1} ms (the median of 50)", times[25]);
    let mut model = embedder::Embedder::load(&text, embedder::Inputs::Documents)?;
    let mmproj = models::MEDIA.require()?;
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
    load_encoder(&mut model, embedder::Encoder::Vision, &mmproj)?;
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
    load_encoder(&mut model, embedder::Encoder::Audio, &mmproj)?;
    let sound: Vec<f32> = (0..embedder::AUDIO_WINDOW).map(|i| ((i as f32) * 0.05).sin() * 0.1).collect();
    model.embed_audio(&sound)?;
    let started = std::time::Instant::now();
    for _ in 0..5 {
        model.embed_audio(&sound)?;
    }
    println!("30 s of sound: {:.0} ms", started.elapsed().as_secs_f64() * 200.0);
    Ok(())
}

/// Loads an encoder, and says what that took: the time, and the memory of its own it holds.
fn load_encoder(model: &mut embedder::Embedder, encoder: embedder::Encoder, mmproj: &Path) -> Result<()> {
    let before = footprint();
    let started = std::time::Instant::now();
    model.load_encoder(encoder, mmproj)?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("the {encoder} encoder: loaded in {ms:.0} ms, holding {} MB", footprint().saturating_sub(before) >> 20);
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
