//! Retrieving: a query's candidates, from two sources. The full-text index gives the segments with all of the query's
//! words (or its quoted parts), best first. The vectors give each segment's cosine with the query, in the form the
//! search scans, and the query's median (index::Vectors).

use super::understand::Query;
use crate::index::Reader;
use anyhow::Result;
use protocol::{Kind, Search};
use std::collections::HashMap;

/// A segment the full-text index matched, and its file.
pub struct Match {
    pub segment: i64,
    pub file: i64,
}

/// The segments with all of the query's words (the last one as a prefix, while typing), or with all of its quoted
/// parts, best first (bm25): of files `wanted` takes, at most `per_file` of one. The kinds and the cap come before the
/// limit, or a long file or files of a kind not asked for could use it up and leave out what was asked for.
pub fn text_matches(
    reader: &Reader,
    query: &Query,
    wanted: &dyn Fn(i64) -> bool,
    per_file: usize,
) -> Result<Vec<Match>> {
    // With quoted parts, only the segments with them are in the running: more of them are.
    let limit = if query.phrases.is_empty() { 1000 } else { 5000 };
    let mut counts: HashMap<i64, usize> = HashMap::new();
    let mut matches = Vec::new();
    let mut take = |segment, file| {
        let count = counts.entry(file).or_default();
        if *count < per_file && wanted(file) {
            *count += 1;
            matches.push(Match { segment, file });
        }
        matches.len() < limit
    };
    if !query.phrases.is_empty() {
        reader.phrase_matches(&query.phrases, &mut take)?;
    } else if !query.words.is_empty() {
        reader.word_matches(&query.words, query.prefix_last, &mut take)?;
    }
    Ok(matches)
}

/// The files changed last, newest first, each by its first segment: of the kinds asked for, else of all but code.
pub fn recent(reader: &Reader, request: &Search) -> Result<Vec<i64>> {
    let kinds: Vec<Kind> = if request.kinds.is_empty() {
        Kind::ALL.into_iter().filter(|&kind| kind != Kind::Code).collect()
    } else {
        request.kinds.clone()
    };
    reader.recent(&kinds, request.limit)
}
