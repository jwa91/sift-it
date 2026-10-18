//! Ranking: from the segments retrieval found to results per file. Pure: numbers in, an order out.
//!
//! Meaning counts by the lift: how far a segment stands above the median segment for this query. Queries differ in how
//! close they land to everything (a vague query is near all of it), and the lift takes that out. EmbeddingGemma 2 puts
//! text, pictures and sound on one scale (measured on the testbed: the typical score of each kind for a query is
//! within a few hundredths of the others'), so one median serves all of them. A segment that lifts little isn't shown,
//! which gives an honest "nothing found".
//!
//! A file counts by its best segment, and the best of many segments fits nearly any query a little: a book has a
//! passage about church bells, a crying baby and Christmas presents. So a long file's lift is lowered by about what
//! its length alone explains, and a book no longer outranks the recording of church bells.
//!
//! Words: a segment with every word of the query (the last one as a prefix, while typing) gets a bonus. For words the
//! model can't read, a number or a code (INV-2024-0113), the bonus is large: no vector holds them exactly. The model
//! does read ordinary words, so for those the meaning has already counted them, and the bonus only breaks near-ties: a
//! note that mentions steam and a train doesn't outrank a photo of a steam train.

use super::understand::Query;
use crate::config::Ranking;
use std::collections::HashMap;

/// How many segments found by meaning are in the running, at most.
const BY_MEANING: usize = 5000;

/// A segment as retrieval found it.
#[derive(Clone, Copy)]
pub struct Scored {
    pub segment: i64,
    pub file: i64,
    /// Its cosine with the query (None when it wasn't scored by meaning).
    pub cosine: Option<f32>,
    /// What its lift is lowered by for the length of its file (length_penalties).
    pub penalty: f32,
}

impl Scored {
    /// The lift less what the length of the segment's file explains: what results are ranked and shown by.
    fn fit(&self, median: f32) -> Option<f32> {
        Some(self.cosine? - median - self.penalty)
    }
}

/// A segment in the running for its file.
#[derive(Clone, Copy)]
pub struct Candidate {
    pub segment: i64,
    pub score: f32,
    /// It has the query's words.
    pub words: bool,
    pub cosine: Option<f32>,
    /// How far its cosine stands above the query's median.
    pub lift: Option<f32>,
    fit: Option<f32>,
}

impl Candidate {
    fn new(scored: &Scored, median: f32, score: f32, words: bool) -> Self {
        let lift = scored.cosine.map(|cosine| cosine - median);
        Self { segment: scored.segment, score, words, cosine: scored.cosine, lift, fit: scored.fit(median) }
    }
}

/// A file among the results.
pub struct Ranked {
    /// Its best segment's score: what it's ranked by.
    pub score: f32,
    /// Its segments that score close to the best, best first: where the result can open.
    pub segments: Vec<Candidate>,
    /// It has the query's words.
    pub words: bool,
    /// It matches by meaning: one of its segments lifts enough to be shown.
    pub meaning: bool,
    pub strong: bool,
}

/// The files the segments found belong to, at most `limit`: strong matches first, then the weaker ones, each by score.
/// `meaning` is every segment scored by meaning; `matches` are those with the query's words (or its phrases), best
/// first. With phrases, only those are in the running, ranked by meaning among themselves.
pub fn rank(
    query: &Query,
    ranking: &Ranking,
    median: f32,
    meaning: impl Iterator<Item = Scored>,
    matches: &[Scored],
    limit: usize,
) -> Vec<Ranked> {
    let mut candidates: HashMap<i64, (i64, Candidate)> = HashMap::new();
    if query.phrases.is_empty() {
        // At most `per_file` of a file before the BY_MEANING are taken.
        let mut shown: Vec<(Scored, f32)> = meaning
            .filter_map(|scored| Some((scored, scored.fit(median)?)))
            .filter(|&(_, fit)| fit >= ranking.shown_lift)
            .collect();
        shown.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut per_file: HashMap<i64, usize> = HashMap::new();
        let taken = shown.into_iter().filter(|(scored, _)| {
            let count = per_file.entry(scored.file).or_default();
            *count += 1;
            *count <= ranking.per_file
        });
        for (scored, fit) in taken.take(BY_MEANING) {
            candidates.insert(scored.segment, (scored.file, Candidate::new(&scored, median, fit, false)));
        }
    }
    let bonus = if query.words_decide { ranking.word_bonus } else { ranking.plain_word_bonus };
    let count = matches.len().max(1) as f32;
    for (at, scored) in matches.iter().enumerate() {
        // Earlier in the full-text ranking breaks ties between files that only match by words.
        let score = bonus + scored.fit(median).unwrap_or(0.0).max(0.0) + 0.02 * (1.0 - at as f32 / count);
        candidates.insert(scored.segment, (scored.file, Candidate::new(scored, median, score, true)));
    }

    // Group by file, keeping the segments close to each file's best.
    let mut by_file: HashMap<i64, Vec<Candidate>> = HashMap::new();
    for (file, candidate) in candidates.into_values() {
        by_file.entry(file).or_default().push(candidate);
    }
    let mut ranked: Vec<Ranked> = by_file
        .into_values()
        .map(|mut segments| {
            segments.sort_by(|a, b| b.score.total_cmp(&a.score));
            let best = segments[0].score;
            segments.retain(|c| c.score >= best - ranking.close);
            let best_fit = segments.iter().filter_map(|c| c.fit).fold(f32::MIN, f32::max);
            Ranked {
                score: best,
                words: segments.iter().any(|c| c.words),
                meaning: best_fit >= ranking.shown_lift,
                // By meaning; by words below, once the best by meaning alone is known.
                strong: best_fit >= ranking.strong_lift,
                segments,
            }
        })
        .collect();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    // Ordinary words make a match strong only when it scores well, or better than anything found by meaning alone (a
    // note that mentions a name, with nothing else to show). Else a passage that happens to hold "glass" and
    // "breaking" would come before the recording of breaking glass.
    let meaning_best = ranked.iter().filter(|file| !file.words).map(|file| file.score).fold(f32::MIN, f32::max);
    ranked.truncate(limit);
    for file in &mut ranked {
        file.strong |=
            file.words && (query.words_decide || file.score >= ranking.strong_lift || file.score >= meaning_best);
    }
    // Strong matches first, then the weaker ones; the sort is stable, so each stays by score.
    ranked.sort_by_key(|file| std::cmp::Reverse(file.strong));
    ranked
}

/// What a lift is measured from: the query's median over the sample of `vectors` vectors, pulled toward
/// `prior_median` as if the index held `prior_weight` more segments scoring that.
pub fn baseline(sampled: f32, vectors: usize, ranking: &Ranking) -> f32 {
    let n = vectors as f32;
    (sampled * n + ranking.prior_median * ranking.prior_weight) / (n + ranking.prior_weight)
}

/// For each vector, what its lift is lowered by for the length of its file: length_penalty × ln(n / length_free) for a
/// file of n vectors, nothing for a short one.
pub fn length_penalties(files: &[i64], ranking: &Ranking) -> Vec<f32> {
    let mut lengths: HashMap<i64, usize> = HashMap::new();
    for &file in files {
        *lengths.entry(file).or_default() += 1;
    }
    let penalty = |n: usize| ranking.length_penalty * (n as f32 / ranking.length_free).ln().max(0.0);
    files.iter().map(|file| penalty(lengths[file])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Search;

    fn query(q: &str) -> Query {
        Query::of(&Search { q: q.into(), ..Default::default() })
    }

    fn scored(segment: i64, file: i64, cosine: Option<f32>) -> Scored {
        Scored { segment, file, cosine, penalty: 0.0 }
    }

    /// The files in their order, each with its segments.
    fn order(ranked: &[Ranked]) -> Vec<(i64, bool)> {
        ranked.iter().map(|file| (file.segments[0].segment, file.strong)).collect()
    }

    #[test]
    fn a_file_counts_by_its_best_segment_and_little_lift_isnt_shown() {
        let ranking = Ranking::default();
        let meaning = [
            scored(1, 10, Some(0.70)),
            scored(2, 10, Some(0.68)),
            scored(3, 20, Some(0.66)),
            scored(4, 30, Some(0.60)),
        ];
        let ranked = rank(&query("bells"), &ranking, 0.55, meaning.into_iter(), &[], 10);
        // Lifts 0.15 and 0.13 for file 10, 0.11 for file 20 (shown, not strong), 0.05 for file 30 (not shown).
        assert_eq!(order(&ranked), [(1, true), (3, false)]);
        assert_eq!(ranked[0].segments.iter().map(|c| c.segment).collect::<Vec<_>>(), [1, 2]);
        assert!((ranked[0].segments[0].lift.unwrap() - 0.15).abs() < 1e-6);
    }

    #[test]
    fn plain_words_break_near_ties_and_numbers_decide() {
        let ranking = Ranking::default();
        let ranked = |q: &str, cosine: f32| {
            // The second file has the words.
            let meaning = [scored(1, 10, Some(0.70)), scored(2, 20, Some(cosine))];
            order(&rank(&query(q), &ranking, 0.55, meaning.into_iter(), &[scored(2, 20, Some(cosine))], 10))
        };
        assert_eq!(ranked("steam train", 0.68), [(2, true), (1, true)]);
        assert_eq!(ranked("steam train", 0.62), [(1, true), (2, false)]);
        assert_eq!(ranked("train 4472", 0.62), [(2, true), (1, true)]);
    }

    #[test]
    fn phrases_leave_only_the_segments_with_them() {
        let meaning = [scored(1, 10, Some(0.80))];
        let matches = [scored(2, 20, None), scored(3, 30, None)];
        let ranked = rank(&query(r#""INV-2024-0113""#), &Ranking::default(), 0.55, meaning.into_iter(), &matches, 10);
        assert_eq!(order(&ranked), [(2, true), (3, true)]);
    }

    #[test]
    fn long_files_are_lowered_short_ones_not() {
        let files: Vec<i64> = [vec![1; 4], vec![2; 40], vec![3; 400], vec![4]].concat();
        let ranking = Ranking::default();
        let penalties = length_penalties(&files, &ranking);
        assert_eq!(penalties[0], 0.0);
        assert_eq!(penalties[files.len() - 1], 0.0);
        let (forty, four_hundred) = (penalties[4], penalties[44]);
        assert!((forty - ranking.length_penalty * 10f32.ln()).abs() < 1e-6);
        assert!(four_hundred > forty && four_hundred < 0.06);
    }
}
