//! `sift-it eval <queries.json>`: how well search finds known answers (evals/evals.json, against the testbed).
//!
//! Each query names the files that count as the answer (by file name) and, optionally, where in the file the match
//! must be (a page or a range of seconds). Queries marked "none" have no answer: anything shown among the strong
//! matches for them is a false alarm.

use crate::embedder::Embedder;
use crate::search::{Request, Searcher};
use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Case {
    q: String,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    loc: Option<[f64; 2]>,
    #[serde(default)]
    none: bool,
}

pub fn run(file: &Path, searcher: &mut Searcher, encoder: &mut Embedder, verbose: bool) -> Result<()> {
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(file)?)?;
    let (mut answered, mut top1, mut top3, mut reciprocal, mut located, mut with_loc) = (0, 0, 0, 0.0, 0, 0);
    let (mut negatives, mut false_alarms) = (0, 0);
    for case in &cases {
        let request = Request { q: case.q.clone(), kinds: Vec::new(), limit: 20, prefix_last: false };
        let vector = encoder.embed_text(&request.prompt())?;
        let response = searcher.search(&request, Some(&vector))?;
        let names: Vec<String> = response.hits.iter().map(|h| h.name.clone()).collect();
        if case.none {
            negatives += 1;
            let strong: Vec<&str> = response.hits.iter().filter(|h| h.strong).map(|h| h.name.as_str()).collect();
            if !strong.is_empty() {
                false_alarms += 1;
            }
            println!(
                "{:<52} {}",
                truncate(&case.q, 52),
                if strong.is_empty() { "✓ nothing strong".to_string() } else { format!("✗ {}", strong.join(", ")) }
            );
            continue;
        }
        answered += 1;
        let rank = response.hits.iter().position(|h| case.expect.contains(&h.name));
        if let Some(rank) = rank {
            reciprocal += 1.0 / (rank + 1) as f64;
            if rank == 0 {
                top1 += 1;
            }
            if rank < 3 {
                top3 += 1;
            }
        }
        let mut where_ok = String::new();
        if let (Some([low, high]), Some(rank)) = (case.loc, rank) {
            with_loc += 1;
            let at = response.hits[rank].loc;
            let ok = at.is_some_and(|at| at >= low - 0.01 && at <= high + 0.01);
            if ok {
                located += 1;
            }
            where_ok =
                format!(" at {} {}", at.map(|a| format!("{a:.0}")).unwrap_or("-".into()), if ok { "✓" } else { "✗" });
        }
        let mark = match rank {
            Some(0) => "✓".to_string(),
            Some(r) => format!("#{}", r + 1),
            None => "✗".to_string(),
        };
        println!(
            "{:<52} {mark}{where_ok}   {}",
            truncate(&case.q, 52),
            names.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        );
        if verbose {
            for hit in response.hits.iter().take(5) {
                println!(
                    "      {:<40} lift {:>5} cos {:>5} {}{}",
                    truncate(&hit.name, 40),
                    hit.lift.map(|z| format!("{z:.3}")).unwrap_or("-".into()),
                    hit.cosine.map(|c| format!("{c:.3}")).unwrap_or("-".into()),
                    hit.segment.as_str(),
                    if hit.strong { "" } else { " (weak)" }
                );
            }
        }
    }
    println!();
    println!(
        "top 1: {top1}/{answered}   top 3: {top3}/{answered}   MRR: {:.3}   right place: {located}/{with_loc}   false alarms: {false_alarms}/{negatives}",
        reciprocal / answered.max(1) as f64
    );
    Ok(())
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width - 1).collect::<String>() + "…"
    }
}
