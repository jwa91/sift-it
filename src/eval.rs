//! `sift-it eval <queries.json>`: how well search finds known answers (evals/evals.json, against the testbed).
//!
//! Each query names the files that count as the answer (by file name) and, optionally, where in the file the match
//! must be (a page or a range of seconds). Queries marked "none" have no answer: anything shown among the strong
//! matches for them is a false alarm.
//!
//! `--compare` measures the search with sketches against the exact one instead: how much of the exact top 20 it keeps
//! when it scores 256 to 8192 candidates exactly, and where the segments the exact top 20 found by meaning come by
//! their sketches' estimates.

use crate::engine::Core;
use crate::index::Form;
use anyhow::Result;
use protocol::{Mode, Search};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

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

pub fn run(file: &Path, core: &Arc<Core>, verbose: bool, compare: bool) -> Result<()> {
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(file)?)?;
    if compare {
        return compare_forms(&cases, core);
    }
    let (mut answered, mut top1, mut top3, mut reciprocal, mut located, mut with_loc) = (0, 0, 0, 0.0, 0, 0);
    let (mut negatives, mut false_alarms) = (0, 0);
    for case in &cases {
        let request =
            Search { q: case.q.clone(), mode: Some(Mode::Meaning), limit: 20, wait: true, ..Default::default() };
        let response = core.search(&request)?;
        if !response.meaning {
            anyhow::bail!("search by meaning isn't available: is the model downloaded?");
        }
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

/// The search with sketches against the exact one, for each query, at several numbers of candidates.
fn compare_forms(cases: &[Case], core: &Arc<Core>) -> Result<()> {
    const TOP: usize = 20;
    const COUNTS: [usize; 6] = [256, 512, 1024, 2048, 4096, 8192];
    let (mut found, mut kept, mut whole, mut fractions) = (0, [0; COUNTS.len()], [0; COUNTS.len()], Vec::new());
    println!("{:<52} {:>5}  {}", "", "", COUNTS.map(|count| format!("{count:>5}")).join(""));
    for case in cases {
        let request = Search { q: case.q.clone(), mode: Some(Mode::Meaning), limit: TOP, ..Default::default() };
        let (exact, sketched, (ranks, of)) = core.with_query(&request, |searcher, query| {
            let query =
                query.ok_or_else(|| anyhow::anyhow!("search by meaning isn't available: is the model downloaded?"))?;
            let paths = |response: protocol::Results| -> Vec<String> {
                response.hits.into_iter().map(|hit| hit.path).collect()
            };
            let exact = searcher.search_in(&request, Some(query), Form::F32, 0)?;
            let by_meaning: Vec<i64> = exact.hits.iter().filter(|hit| !hit.words).map(|hit| hit.segment_id).collect();
            let sketched = COUNTS
                .iter()
                .map(|&count| Ok(paths(searcher.search_in(&request, Some(query), Form::Sketch, count)?)))
                .collect::<Result<Vec<_>>>()?;
            Ok((paths(exact), sketched, searcher.sketch_ranks(&request, query, &by_meaning)?))
        })?;
        found += exact.len();
        let same: Vec<usize> =
            sketched.iter().map(|paths| exact.iter().filter(|path| paths.contains(path)).count()).collect();
        for (at, &same) in same.iter().enumerate() {
            kept[at] += same;
            whole[at] += usize::from(same == exact.len());
        }
        let needed = ranks.iter().flatten().max().map(|rank| rank + 1);
        if let Some(needed) = needed {
            fractions.push(needed as f64 / of.max(1) as f64);
        }
        println!(
            "{:<52} {:>2}/{:<2}  {}   {}",
            truncate(&case.q, 52),
            exact.len(),
            TOP,
            same.iter().map(|same| format!("{same:>5}")).collect::<String>(),
            needed.map(|n| format!("needs {n} of {of}")).unwrap_or_default()
        );
    }
    println!();
    for (at, count) in COUNTS.iter().enumerate() {
        println!(
            "{count:>5} candidates: sketches keep {}/{found} of the exact top {TOP} ({:.1}%), all of it for {}/{} queries",
            kept[at],
            100.0 * kept[at] as f64 / found.max(1) as f64,
            whole[at],
            cases.len()
        );
    }
    fractions.sort_by(f64::total_cmp);
    let at = |share: f64| fractions.get(((fractions.len() as f64 * share) as usize).min(fractions.len().max(1) - 1));
    let percent = |fraction: Option<&f64>| fraction.map(|f| format!("{:.1}%", 100.0 * f)).unwrap_or("-".into());
    println!(
        "What the exact top {TOP} found by meaning comes within the first {} of the vectors by sketch for half the \
         queries, {} for 90%, {} for all",
        percent(at(0.5)),
        percent(at(0.9)),
        percent(fractions.last())
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
