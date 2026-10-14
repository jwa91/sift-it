//! Hybrid search: meaning (the model's vectors) and exact words (the full-text index), one result per file.
//!
//! Meaning: every vector is scored against the query (cosine similarity, one matrix-vector product). What counts is the
//! lift: how far a segment stands above the median segment for this query. Queries differ in how close they land to
//! everything (a vague query is near all of it), and the lift takes that out. EmbeddingGemma 2 puts text, pictures and
//! sound on one scale (measured on the testbed: the typical score of each kind for a query is within a few hundredths
//! of the others'), so one median serves all of them. A segment that lifts little isn't shown, which gives an honest
//! "nothing found".
//!
//! A file counts by its best segment, and the best of many segments fits nearly any query a little: a book has a
//! passage about church bells, a crying baby and Christmas presents. So a long file's lift is lowered by about what
//! its length alone explains, and a book no longer outranks the recording of church bells.
//!
//! Words: a segment with every word of the query (the last one as a prefix, while typing) gets a bonus. For words the
//! model can't read, a number or a code (INV-2024-0113), the bonus is large: no vector holds them exactly. The model
//! does read ordinary words, so for those the meaning has already counted them, and the bonus only breaks near-ties: a
//! note that mentions steam and a train doesn't outrank a photo of a steam train. Words also pick which of a file's
//! segments a result opens at, between segments the meaning scores about the same.

use crate::crawl::Kind;
use crate::embedder::DIM;
use crate::store::{Segment, Store, blob_to_vector};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Lift (cosine above the query's median) a meaning match needs to be shown at all, and to count as a strong match.
/// On the testbed (evals/evals.json), answers lift 0.12–0.30 (nearly all above 0.135); the best results for queries
/// without an answer lift 0.05–0.10, except a near miss at 0.144.
const SHOWN_LIFT: f32 = 0.09;
const STRONG_LIFT: f32 = 0.13;
/// The bonus for containing all of the query's words, in the same units, when one of them is a number or the search
/// is of code or quoted words: the words decide.
const WORD_BONUS: f32 = 0.10;
/// The bonus for containing all of them otherwise: the words only choose between matches the meaning scores about the
/// same. Any larger, and a passage that only mentions them outranks a better picture or recording of what they
/// describe.
const PLAIN_WORD_BONUS: f32 = 0.03;
/// A file of n segments has its lifts lowered by LENGTH_PENALTY × ln(n / LENGTH_FREE), and one of up to LENGTH_FREE
/// segments (a photo, a few pages, a minute and a half of sound) not at all. That is about how far the best of n
/// segments stands above a typical one by chance alone: the expected maximum of n draws, at the usual spread of a
/// query's scores (0.03), less that of LENGTH_FREE. A 3-minute video loses 0.03, a novel 0.05–0.07. On a folder of 520
/// mixed files and eight books (docs/ranking.md), a book had come first for 21 of 95 queries about something else;
/// now 1.
const LENGTH_PENALTY: f32 = 0.0125;
const LENGTH_FREE: f32 = 4.0;
/// Segments of one file within this much of its best are close enough for words to choose between them.
const CLOSE: f32 = 0.04;
/// At most this many of one file's segments are candidates, so a long file can't use up what's taken.
const PER_FILE: usize = 10;
/// Above this many vectors, the median is taken from a sample (it's the same median, faster).
const MEDIAN_SAMPLE: usize = 50_000;
/// A typical median: across the testbed's queries, a query's median segment scores 0.49–0.59. A small index's own
/// median is pulled toward it (as if it held PRIOR_WEIGHT more segments scoring that), or with a file or two the
/// median would be the match itself, and nothing could ever stand out.
const PRIOR_MEDIAN: f32 = 0.55;
const PRIOR_WEIGHT: f32 = 30.0;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn cblas_sgemv(
        order: i32,
        trans: i32,
        m: i32,
        n: i32,
        alpha: f32,
        a: *const f32,
        lda: i32,
        x: *const f32,
        incx: i32,
        beta: f32,
        y: *mut f32,
        incy: i32,
    );
}
const ROW_MAJOR: i32 = 101;
const NO_TRANS: i32 = 111;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Request {
    #[serde(default)]
    pub q: String,
    /// Only these kinds of file (all when empty).
    #[serde(default)]
    pub kinds: Vec<Kind>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Match the last word as a prefix (the user is still typing it).
    #[serde(default)]
    pub prefix_last: bool,
}

fn default_limit() -> usize {
    40
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub kind: Kind,
    /// The segment that matched best: where the result opens.
    pub segment: Segment,
    pub segment_id: i64,
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    /// What it's ranked by: its best segment's lift, less what the length of the file explains, plus any bonus for
    /// the query's words.
    pub score: f32,
    /// How far the segment's meaning stands above the query's median (None without a query vector, or for a segment
    /// without one).
    pub lift: Option<f32>,
    pub cosine: Option<f32>,
    /// It matched by meaning (its lift clears the bar).
    pub meaning: bool,
    /// All of the query's words are in it.
    pub words: bool,
    pub strong: bool,
    pub excerpt: Option<String>,
    pub modified: f64,
    pub size: u64,
    pub info: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub hits: Vec<Hit>,
    /// Meaning was part of this search (false: words only).
    pub meaning: bool,
    pub ms: f64,
}

pub struct Searcher {
    store: Store,
    ids: Vec<i64>,
    rows: HashMap<i64, usize>,
    files: Vec<i64>,
    /// What each vector's lift is lowered by for the length of its file (LENGTH_PENALTY).
    penalties: Vec<f32>,
    matrix: Vec<f32>,
    file_kinds: HashMap<i64, Kind>,
    data_version: i64,
    max_id: i64,
    /// The index's vectors come from the pipeline the query encoder belongs to (else: words only, until reindexed).
    compatible: bool,
}

/// "code: retry with backoff": a search of code, and only of code. Without the prefix (or the code filter), a search
/// never shows code: a project's thousands of stretches would bury everything else.
pub const CODE_PREFIX: &str = "code:";

impl Request {
    /// A search of code: the prefix, or the code filter alone.
    pub fn is_code(&self) -> bool {
        self.has_prefix() || self.kinds == [Kind::Code]
    }

    fn has_prefix(&self) -> bool {
        self.q.trim_start().get(..CODE_PREFIX.len()).is_some_and(|start| start.eq_ignore_ascii_case(CODE_PREFIX))
    }

    /// The words searched for, without the prefix.
    pub fn text(&self) -> &str {
        if self.has_prefix() { self.q.trim_start()[CODE_PREFIX.len()..].trim() } else { self.q.trim() }
    }

    /// Parts in quotes ("INV-2024-0113", or curly quotes): words that must be there, in that order.
    pub fn phrases(&self) -> Vec<Vec<String>> {
        let text = self.text().replace(['“', '”', '„'], "\"");
        text.split('"').skip(1).step_by(2).map(words_of).filter(|words| !words.is_empty()).collect()
    }

    /// The query's words decide where they're found: it has words the model can't read (a number or a code such as
    /// INV-2024-0113), it's a search of code, or words are in quotes. Other words the model reads, so where they're
    /// found only breaks near-ties.
    pub fn words_decide(&self) -> bool {
        self.is_code()
            || !self.phrases().is_empty()
            || words_of(self.text()).iter().any(|word| word.chars().any(char::is_numeric))
    }

    /// The prompt the query is embedded with (quotes don't mean anything to the model).
    pub fn prompt(&self) -> String {
        let text = self.text().replace(['"', '“', '”', '„'], "");
        if self.is_code() {
            crate::embedder::prompts::code_query(&text)
        } else {
            crate::embedder::prompts::query(&text)
        }
    }
}

/// A segment in the running for its file.
#[derive(Clone, Copy)]
struct Candidate {
    segment: i64,
    score: f32,
    words: bool,
}

impl Searcher {
    pub fn open(path: &Path) -> Result<Self> {
        let store = Store::open_readonly(path)?;
        let mut searcher = Self {
            store,
            ids: Vec::new(),
            rows: HashMap::new(),
            files: Vec::new(),
            penalties: Vec::new(),
            matrix: Vec::new(),
            file_kinds: HashMap::new(),
            data_version: -1,
            max_id: 0,
            compatible: true,
        };
        searcher.refresh()?;
        Ok(searcher)
    }

    pub fn vector_count(&self) -> usize {
        self.ids.len()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Catches up with the indexer's commits: only the new vectors when nothing was removed, all of them otherwise.
    pub fn refresh(&mut self) -> Result<bool> {
        let version = self.store.data_version()?;
        if version == self.data_version {
            return Ok(false);
        }
        self.data_version = version;
        self.compatible =
            self.store.meta("vector_source")?.is_none_or(|source| source == crate::embedder::vector_source());
        // Segment ids are never reused, so what's loaded is still right unless some of it was deleted.
        let kept: i64 = self.store.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE vector IS NOT NULL AND id <= ?1",
            [self.max_id],
            |row| row.get(0),
        )?;
        if kept as usize != self.ids.len() {
            self.drop_deleted()?;
        }
        let mut statement = self
            .store
            .conn
            .prepare("SELECT id, file_id, vector FROM segments WHERE vector IS NOT NULL AND id > ?1 ORDER BY id")?;
        let mut rows = statement.query([self.max_id])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let blob = row.get_ref(2)?.as_blob()?;
            if blob.len() != DIM * 4 {
                continue;
            }
            self.rows.insert(id, self.ids.len());
            self.ids.push(id);
            self.files.push(row.get(1)?);
            blob_to_vector(blob, &mut self.matrix);
            self.max_id = self.max_id.max(id);
        }
        drop(rows);
        drop(statement);
        let mut statement = self.store.conn.prepare("SELECT id, kind FROM files")?;
        self.file_kinds = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|row| row.ok())
            .filter_map(|(id, kind)| Kind::parse(&kind).map(|kind| (id, kind)))
            .collect();
        self.penalties = length_penalties(&self.files);
        Ok(true)
    }

    /// Drops the loaded vectors whose segments are gone (a file changed or left), keeping the rest in place.
    fn drop_deleted(&mut self) -> Result<()> {
        let mut statement = self.store.conn.prepare("SELECT id FROM segments WHERE vector IS NOT NULL AND id <= ?1")?;
        let present: std::collections::HashSet<i64> =
            statement.query_map([self.max_id], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
        let mut kept = 0;
        for row in 0..self.ids.len() {
            if present.contains(&self.ids[row]) {
                if kept != row {
                    self.ids[kept] = self.ids[row];
                    self.files[kept] = self.files[row];
                    self.matrix.copy_within(row * DIM..(row + 1) * DIM, kept * DIM);
                }
                kept += 1;
            }
        }
        self.ids.truncate(kept);
        self.files.truncate(kept);
        self.matrix.truncate(kept * DIM);
        self.rows = self.ids.iter().enumerate().map(|(row, &id)| (id, row)).collect();
        Ok(())
    }

    pub fn search(&mut self, request: &Request, query: Option<&[f32]>) -> Result<Response> {
        let started = std::time::Instant::now();
        self.refresh()?;
        let code = request.is_code();
        let wanted = |kind: Kind| {
            if code {
                kind == Kind::Code
            } else {
                kind != Kind::Code && (request.kinds.is_empty() || request.kinds.contains(&kind))
            }
        };
        let wanted_file = |file: i64| self.file_kinds.get(&file).is_some_and(|&kind| wanted(kind));

        // Meaning: every vector's cosine, and the query's median.
        let query = query.filter(|_| self.compatible);
        let (cosines, median) = match query {
            Some(query) if !self.ids.is_empty() => {
                let cosines = self.cosines(query);
                let n = cosines.len() as f32;
                let median = (median(&cosines) * n + PRIOR_MEDIAN * PRIOR_WEIGHT) / (n + PRIOR_WEIGHT);
                (cosines, median)
            }
            _ => (Vec::new(), 0.0),
        };
        let lift_of = |segment: i64| self.rows.get(&segment).and_then(|&row| cosines.get(row)).map(|c| c - median);
        // The lift less what the length of the segment's file explains: what results are ranked and shown by.
        let fit_at = |row: usize| cosines[row] - median - self.penalties.get(row).copied().unwrap_or(0.0);
        let fit_of = |segment: i64| self.rows.get(&segment).filter(|&&row| row < cosines.len()).map(|&row| fit_at(row));

        let mut candidates: HashMap<i64, Candidate> = HashMap::new();
        let words = words_of(request.text());
        let phrases = request.phrases();
        let exact = request.words_decide();
        let bonus = if exact { WORD_BONUS } else { PLAIN_WORD_BONUS };
        if !phrases.is_empty() {
            // Quoted words must be there exactly: only segments with them, ranked by meaning among themselves.
            let matches = self.phrase_matches(&phrases, &wanted_file, 5000)?;
            let count = matches.len().max(1) as f32;
            for (rank, segment) in matches.into_iter().enumerate() {
                let score = WORD_BONUS + fit_of(segment).unwrap_or(0.0).max(0.0) + 0.02 * (1.0 - rank as f32 / count);
                candidates.insert(segment, Candidate { segment, score, words: true });
            }
        } else if !cosines.is_empty() {
            // The kinds asked for, and at most PER_FILE of a file, before the 5000 are taken.
            let mut shown: Vec<(usize, f32)> = (0..cosines.len())
                .map(|i| (i, fit_at(i)))
                .filter(|&(i, fit)| fit >= SHOWN_LIFT && wanted_file(self.files[i]))
                .collect();
            shown.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut per_file: HashMap<i64, usize> = HashMap::new();
            let taken = shown.into_iter().filter(|&(i, _)| {
                let count = per_file.entry(self.files[i]).or_default();
                *count += 1;
                *count <= PER_FILE
            });
            for (i, fit) in taken.take(5000) {
                candidates.insert(self.ids[i], Candidate { segment: self.ids[i], score: fit, words: false });
            }
        }
        if phrases.is_empty() && !words.is_empty() {
            let matches = self.word_matches(&words, request.prefix_last, &wanted_file, 1000)?;
            let count = matches.len().max(1) as f32;
            for (rank, segment) in matches.into_iter().enumerate() {
                // Earlier in the full-text ranking breaks ties between files that only match by words.
                let score = bonus + fit_of(segment).unwrap_or(0.0).max(0.0) + 0.02 * (1.0 - rank as f32 / count);
                candidates.insert(segment, Candidate { segment, score, words: true });
            }
        }

        // Group by file, keeping the segments close to each file's best.
        let mut by_file: HashMap<i64, Vec<Candidate>> = HashMap::new();
        for candidate in candidates.into_values() {
            let file = match self.rows.get(&candidate.segment) {
                Some(&row) => self.files[row],
                None => match self.file_of(candidate.segment)? {
                    Some(file) => file,
                    None => continue,
                },
            };
            if self.file_kinds.get(&file).is_some_and(|&kind| wanted(kind)) {
                by_file.entry(file).or_default().push(candidate);
            }
        }
        let mut ranked: Vec<(f32, Vec<Candidate>)> = by_file
            .into_values()
            .map(|mut segments| {
                segments.sort_by(|a, b| b.score.total_cmp(&a.score));
                let best = segments[0].score;
                segments.retain(|c| c.score >= best - CLOSE);
                (best, segments)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Ordinary words make a match strong only when it scores well, or better than anything found by meaning alone
        // (a note that mentions a name, with nothing else to show). Else a passage that happens to hold "glass" and
        // "breaking" would come before the recording of breaking glass.
        let meaning_best = ranked
            .iter()
            .filter(|(_, segments)| !segments.iter().any(|c| c.words))
            .map(|&(score, _)| score)
            .fold(f32::MIN, f32::max);
        ranked.truncate(request.limit);

        let mut hits = Vec::with_capacity(ranked.len());
        for (score, segments) in ranked {
            let has_words = segments.iter().any(|c| c.words);
            let chosen = self.choose(&segments, &words)?;
            let Some(mut hit) = self.hit(chosen, &words)? else { continue };
            let lift = lift_of(chosen);
            let best_fit = segments.iter().filter_map(|c| fit_of(c.segment)).fold(f32::MIN, f32::max);
            hit.score = score;
            hit.lift = lift;
            hit.cosine = lift.map(|l| l + median);
            hit.meaning = best_fit >= SHOWN_LIFT;
            hit.words = has_words;
            hit.strong =
                best_fit >= STRONG_LIFT || (has_words && (exact || score >= STRONG_LIFT || score >= meaning_best));
            hits.push(hit);
        }
        // Strong matches first, then the weaker ones, each by score.
        hits.sort_by(|a, b| b.strong.cmp(&a.strong).then(b.score.total_cmp(&a.score)));
        Ok(Response { hits, meaning: query.is_some(), ms: started.elapsed().as_secs_f64() * 1000.0 })
    }

    /// A segment's whole text.
    pub fn segment_text(&self, segment_id: i64) -> Result<Option<String>> {
        self.store.segment_text(segment_id)
    }

    /// A file's text: its text segments in order (see Store::file_text).
    pub fn file_text(&self, path: &str) -> Result<Option<String>> {
        self.store.file_text(path)
    }

    /// Of a file's segments that score about the same, the one with the most of the query's words.
    fn choose(&self, segments: &[Candidate], words: &[String]) -> Result<i64> {
        if segments.len() == 1 || words.is_empty() {
            return Ok(segments[0].segment);
        }
        let mut best = (segments[0].segment, 0usize, segments[0].score);
        for candidate in segments {
            let text: Option<String> = self
                .store
                .conn
                .prepare_cached("SELECT text FROM segments WHERE id = ?1")?
                .query_row([candidate.segment], |row| row.get(0))
                .optional()?
                .flatten();
            let text = text.unwrap_or_default().to_lowercase();
            // Short words ("the", "for") are in every passage; numbers count whatever their length.
            let telling = |word: &&String| word.chars().count() > 3 || word.chars().any(|c| c.is_numeric());
            let found = words.iter().filter(telling).filter(|word| text.contains(word.as_str())).count();
            if found > best.1 || (found == best.1 && candidate.score > best.2) {
                best = (candidate.segment, found, candidate.score);
            }
        }
        Ok(best.0)
    }

    /// Recently changed files, for an empty query.
    pub fn recent(&self, request: &Request) -> Result<Response> {
        // The kinds go into the query itself (kind names are fixed words, safe to inline): filtering after the limit
        // could leave nothing.
        let kinds = if request.kinds.is_empty() {
            "f.kind != 'code'".to_string()
        } else {
            let names: Vec<String> = request.kinds.iter().map(|kind| format!("'{}'", kind.as_str())).collect();
            format!("f.kind IN ({})", names.join(", "))
        };
        let mut statement = self.store.conn.prepare(&format!(
            "SELECT (SELECT MIN(id) FROM segments WHERE file_id = f.id) FROM files f
             WHERE f.state = 'done' AND {kinds} ORDER BY f.modified DESC LIMIT ?1"
        ))?;
        let candidates: Vec<Option<i64>> =
            statement.query_map([request.limit as i64], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
        let mut hits = Vec::new();
        for segment_id in candidates.into_iter().flatten() {
            if let Some(mut hit) = self.hit(segment_id, &[])? {
                hit.strong = true;
                hits.push(hit);
            }
        }
        Ok(Response { hits, meaning: false, ms: 0.0 })
    }

    /// Cosine similarity of every vector with the query (vectors are normalized).
    fn cosines(&self, query: &[f32]) -> Vec<f32> {
        let n = self.ids.len();
        let mut scores = vec![0.0f32; n];
        unsafe {
            cblas_sgemv(
                ROW_MAJOR,
                NO_TRANS,
                n as i32,
                DIM as i32,
                1.0,
                self.matrix.as_ptr(),
                DIM as i32,
                query.as_ptr(),
                1,
                0.0,
                scores.as_mut_ptr(),
                1,
            );
        }
        scores
    }

    /// Segments with every word, best first (FTS5's bm25).
    fn word_matches(
        &self,
        words: &[String],
        prefix_last: bool,
        wanted: &dyn Fn(i64) -> bool,
        limit: usize,
    ) -> Result<Vec<i64>> {
        let query = words
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let quoted = format!("\"{}\"", word.replace('"', "\"\""));
                if prefix_last && i == words.len() - 1 { format!("{quoted}*") } else { quoted }
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.text_matches(&query, wanted, limit)
    }

    /// Segments with every phrase, its words together and in order (FTS5 phrase queries), best first.
    fn phrase_matches(&self, phrases: &[Vec<String>], wanted: &dyn Fn(i64) -> bool, limit: usize) -> Result<Vec<i64>> {
        let query = phrases
            .iter()
            .map(|words| format!("\"{}\"", words.join(" ").replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        self.text_matches(&query, wanted, limit)
    }

    /// The segments an FTS5 query matches, best first (bm25): only of files `wanted` takes, at most PER_FILE of one,
    /// `limit` in all. The kinds and the cap come before the limit, or a long file or files of a kind not asked for
    /// could use it up and leave out what was asked for.
    fn text_matches(&self, query: &str, wanted: &dyn Fn(i64) -> bool, limit: usize) -> Result<Vec<i64>> {
        let mut statement = self.store.conn.prepare_cached(
            "SELECT words.rowid, segments.file_id FROM words JOIN segments ON segments.id = words.rowid
             WHERE words MATCH ?1 ORDER BY words.rank",
        )?;
        let mut rows = statement.query([query])?;
        let mut per_file: HashMap<i64, usize> = HashMap::new();
        let mut ids = Vec::new();
        while ids.len() < limit
            && let Some(row) = rows.next()?
        {
            let (segment, file): (i64, i64) = (row.get(0)?, row.get(1)?);
            let count = per_file.entry(file).or_default();
            if *count < PER_FILE && wanted(file) {
                *count += 1;
                ids.push(segment);
            }
        }
        Ok(ids)
    }

    fn file_of(&self, segment_id: i64) -> Result<Option<i64>> {
        Ok(self
            .store
            .conn
            .query_row("SELECT file_id FROM segments WHERE id = ?1", [segment_id], |row| row.get(0))
            .optional()?)
    }

    fn hit(&self, segment_id: i64, words: &[String]) -> Result<Option<Hit>> {
        let mut statement = self.store.conn.prepare_cached(
            "SELECT f.path, f.kind, f.modified, f.size, f.info, s.segment, s.loc, s.loc_end, s.text
             FROM segments s JOIN files f ON f.id = s.file_id WHERE s.id = ?1",
        )?;
        let hit: Option<Hit> = statement
            .query_row([segment_id], |row| {
                let path: String = row.get(0)?;
                let text: Option<String> = row.get(8)?;
                Ok(Hit {
                    name: Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    path,
                    kind: Kind::parse(&row.get::<_, String>(1)?).unwrap_or(Kind::Text),
                    modified: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    info: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|info| serde_json::from_str(&info).ok())
                        .unwrap_or(serde_json::Value::Null),
                    segment: Segment::parse(&row.get::<_, String>(5)?).unwrap_or(Segment::Passage),
                    segment_id,
                    loc: row.get(6)?,
                    loc_end: row.get(7)?,
                    excerpt: text.map(|text| excerpt(&text, words)),
                    score: 0.0,
                    lift: None,
                    cosine: None,
                    meaning: false,
                    words: false,
                    strong: false,
                })
            })
            .optional()?;
        let Some(mut hit) = hit else { return Ok(None) };
        // In text and code, open at the first line with one of the query's telling words, not at the passage's top.
        // (A notebook's places are cells, not lines.)
        let notebook = hit.path.ends_with(".ipynb");
        if let (Segment::Passage, Kind::Code | Kind::Text, Some(first), false) =
            (hit.segment, hit.kind, hit.loc, notebook)
            && let Some(offset) = self.focus_line(segment_id, words)?
        {
            hit.loc = Some(first + offset as f64);
        }
        Ok(Some(hit))
    }

    /// The first line of a segment (from 0) with one of the words, ignoring short ones.
    fn focus_line(&self, segment_id: i64, words: &[String]) -> Result<Option<usize>> {
        let telling: Vec<&String> =
            words.iter().filter(|w| w.chars().count() > 3 || w.chars().any(|c| c.is_numeric())).collect();
        if telling.is_empty() {
            return Ok(None);
        }
        let text: Option<String> = self
            .store
            .conn
            .prepare_cached("SELECT text FROM segments WHERE id = ?1")?
            .query_row([segment_id], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(text.and_then(|text| {
            text.lines().position(|line| {
                let line = line.to_lowercase();
                telling.iter().any(|word| line.contains(word.as_str()))
            })
        }))
    }
}

/// For each vector, what its lift is lowered by for the length of its file: LENGTH_PENALTY × ln(n / LENGTH_FREE) for a
/// file of n vectors, nothing for a short one.
fn length_penalties(files: &[i64]) -> Vec<f32> {
    let mut lengths: HashMap<i64, usize> = HashMap::new();
    for &file in files {
        *lengths.entry(file).or_default() += 1;
    }
    files.iter().map(|file| LENGTH_PENALTY * (lengths[file] as f32 / LENGTH_FREE).ln().max(0.0)).collect()
}

/// The median of the scores (of a sample of them, for a large index).
fn median(scores: &[f32]) -> f32 {
    let mut sample: Vec<f32> = if scores.len() > MEDIAN_SAMPLE {
        let step = scores.len() / MEDIAN_SAMPLE;
        scores.iter().step_by(step).copied().collect()
    } else {
        scores.to_vec()
    };
    let middle = sample.len() / 2;
    let (_, median, _) = sample.select_nth_unstable_by(middle, |a, b| a.total_cmp(b));
    *median
}

/// The query's words, as the full-text index splits them (letters and digits; everything else separates).
pub fn words_of(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

/// About 300 characters of the text, around the first of the query's words in it when there is one.
pub fn excerpt(text: &str, words: &[String]) -> String {
    const LENGTH: usize = 300;
    let text = text.trim();
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths; only trust a position that lands on a boundary in the original.
    let found = words
        .iter()
        .filter(|word| word.len() > 2)
        .filter_map(|word| lower.find(word.as_str()))
        .min()
        .filter(|&position| text.is_char_boundary(position));
    let start = match found {
        Some(position) if position > LENGTH / 3 => {
            let mut start = position - LENGTH / 3;
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            // Begin at a word (after the whole space character: some are more than one byte).
            text[start..]
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, c)| start + i + c.len_utf8())
                .unwrap_or(start)
        }
        _ => 0,
    };
    let mut end = (start + LENGTH).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(text[start..end].trim());
    if end < text.len() {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewSegment;

    /// An index in a file of its own, removed when the test ends.
    struct TempIndex(std::path::PathBuf);
    impl TempIndex {
        fn new(name: &str) -> Self {
            let index = Self(std::env::temp_dir().join(format!("sift-it-test-{name}-{}.sqlite", std::process::id())));
            index.remove();
            index
        }
        fn remove(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }
    impl Drop for TempIndex {
        fn drop(&mut self) {
            self.remove();
        }
    }

    /// A file of `kind` whose passages are `texts`, indexed without vectors.
    fn add(store: &mut Store, path: &str, kind: Kind, texts: Vec<String>) {
        let found = crate::crawl::Found { path: path.into(), kind, size: 1, modified: 1.0 };
        store.queue(&found, None).unwrap();
        let row = store.pending(100).unwrap().into_iter().find(|row| row.path == Path::new(path)).unwrap();
        let segments: Vec<NewSegment> = texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| NewSegment {
                segment: Segment::Passage,
                loc: Some(i as f64 + 1.0),
                loc_end: None,
                text: Some(text),
                vector: None,
            })
            .collect();
        assert!(store.finish_file(&row, &segments, &serde_json::json!({})).unwrap());
    }

    #[test]
    fn a_long_file_or_an_unwanted_kind_doesnt_use_up_the_word_matches() {
        let index = TempIndex::new("budget");
        let mut store = Store::open(&index.0).unwrap();
        add(&mut store, "/docs/long.html", Kind::Doc, (0..1200).map(|i| format!("needle {i}, needle")).collect());
        add(&mut store, "/notes/note.txt", Kind::Text, vec!["needle extra context in a note".into()]);
        drop(store);
        let mut searcher = Searcher::open(&index.0).unwrap();
        let search = |searcher: &mut Searcher, kinds: Vec<Kind>| {
            let request = Request { q: "needle".into(), kinds, limit: 10, prefix_last: false };
            searcher.search(&request, None).unwrap().hits.into_iter().map(|hit| hit.path).collect::<Vec<_>>()
        };
        assert_eq!(search(&mut searcher, vec![Kind::Text]), vec!["/notes/note.txt"]);
        assert_eq!(search(&mut searcher, vec![]).len(), 2);
    }

    #[test]
    fn quoted_parts_are_phrases() {
        let request = Request { q: r#"invoice "INV-2024-0113" from “Altink Design”"#.into(), ..Default::default() };
        assert_eq!(request.phrases(), vec![vec!["inv", "2024", "0113"], vec!["altink", "design"]]);
        assert!(!request.prompt().contains('"'));
        let plain = Request { q: "no quotes here".into(), ..Default::default() };
        assert!(plain.phrases().is_empty());
    }

    #[test]
    fn numbers_codes_and_quotes_decide() {
        let decides = |q: &str| Request { q: q.into(), ..Default::default() }.words_decide();
        assert!(decides("INV-2024-0113"));
        assert!(decides("tax return 2023"));
        assert!(decides(r#"the "Altink Design" invoice"#));
        assert!(decides("code: refreshAccessToken"));
        assert!(!decides("a steam train"));
        assert!(!decides("Bram"));
    }

    #[test]
    fn long_files_are_lowered_short_ones_not() {
        let files: Vec<i64> = [vec![1; 4], vec![2; 40], vec![3; 400], vec![4]].concat();
        let penalties = length_penalties(&files);
        assert_eq!(penalties[0], 0.0);
        assert_eq!(penalties[files.len() - 1], 0.0);
        let (forty, four_hundred) = (penalties[4], penalties[44]);
        assert!((forty - LENGTH_PENALTY * 10f32.ln()).abs() < 1e-6);
        assert!(four_hundred > forty && four_hundred < 0.06);
    }

    #[test]
    fn median_of_odd_and_even() {
        assert_eq!(median(&[0.3, 0.1, 0.2]), 0.2);
        assert_eq!(median(&[0.4, 0.1, 0.3, 0.2]), 0.3);
    }

    #[test]
    fn excerpt_survives_wide_spaces() {
        let text = format!("{}\u{3000}word\u{00A0}more {}", "あ".repeat(60), "pet ".repeat(50));
        assert!(excerpt(&text, &["pet".to_string()]).contains("pet"));
        let text = format!("{}\u{00A0}{} pet", "x".repeat(150), "y".repeat(40));
        assert!(excerpt(&text, &["pet".to_string()]).contains("pet"));
    }

    #[test]
    fn excerpt_centres_on_the_words() {
        let text = format!("{} the pet clause is here {}", "filler ".repeat(80), "more ".repeat(80));
        let excerpt = excerpt(&text, &["pet".to_string()]);
        assert!(excerpt.contains("pet clause"));
        assert!(excerpt.starts_with('…'));
    }
}
