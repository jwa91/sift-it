//! Hybrid search: meaning (the model's vectors) and exact words (the full-text index), one result per file.
//!
//! Meaning: the vectors are scored against the query (cosine similarity, from the vector files, mapped: see vectors.rs),
//! in the form `search.vectors` says. An exact form scores every vector. With sketches, only the few thousand whose
//! sketches come closest are scored (Searcher::candidates), exactly, and so are the segments the query's words are in. What counts is the
//! lift: how far a segment stands above the median segment for this query, taken from a fixed sample of the vectors.
//! Queries differ in how close they land to everything (a vague query is near all of it), and the lift takes that out.
//! EmbeddingGemma 2 puts text, pictures and sound on one scale (measured on the testbed: the typical score of each kind
//! for a query is within a few hundredths of the others'), so one median serves all of them. A segment that lifts
//! little isn't shown, which gives an honest "nothing found".
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

use crate::config::{self, Ranking};
use crate::embedder::DIM;
use crate::store::Store;
use crate::vectors::{self, Form, Map, SketchQuery};
use anyhow::Result;
use protocol::{Hit, Kind, Results, Scan, Search, Segment};
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How many segments a query's median is taken from (all of them, in a smaller index).
const SAMPLE: usize = 8192;

/// How many segments a search with sketches scores exactly, at least: those whose sketches come closest to the query.
/// Measured on Rust's books (7,400 vectors, all on one subject, so many come close): the closest 1,024 keep all of the
/// exact top 20 for 45 queries, the closest 256 keep 99.3%.
const CANDIDATES: usize = 4096;

/// The share of the Mac's memory searches may read with `search.memory_mb` at auto.
const MEMORY_SHARE: f64 = 0.1;

pub struct Searcher {
    store: Store,
    ranking: Ranking,
    search: config::Search,
    file_kinds: HashMap<i64, Kind>,
    data_version: i64,
    /// The index's vectors come from the pipeline the query encoder belongs to (else: words only, until reindexed).
    compatible: bool,
    /// The vectors, for searches by meaning: empty until loaded (load_vectors, set_vectors).
    vectors: Vectors,
    loaded: bool,
    /// What each vector's lift is lowered by for the length of its file (`ranking.length_penalty`).
    penalties: Vec<f32>,
}

/// The index's vectors, for searches by meaning: each partition's rows in every form, mapped, which segment each live
/// row is, in the order the segments were made, and the sample a query's median comes from.
#[derive(Default)]
pub struct Vectors {
    folder: PathBuf,
    partitions: Vec<Mapped>,
    ids: Vec<i64>,
    files: Vec<i64>,
    kinds: Vec<Kind>,
    /// Where each segment's vector is: its partition (in `partitions`) and row.
    places: Vec<(usize, usize)>,
    /// Each segment's place in `ids`, `files`, `kinds` and `places`.
    index: HashMap<i64, usize>,
    max_id: i64,
    sample: Sample,
}

/// A partition's rows, as far as they're mapped, and what its sketches are compared by.
struct Mapped {
    id: i64,
    generation: i64,
    rows: usize,
    f32: Map,
    f16: Map,
    sketch: Map,
    mean: Vec<f32>,
    spread: Vec<f32>,
}

impl Mapped {
    fn map(&self, form: Form) -> &Map {
        match form {
            Form::F32 => &self.f32,
            Form::F16 => &self.f16,
            Form::Sketch => &self.sketch,
        }
    }
}

/// The vectors a query's median comes from, copied into memory: the SAMPLE segments whose ids hash lowest. A segment is
/// in or out for as long as it lives, so the sample only changes where the index does, and it's the same for every
/// form a search scans.
#[derive(Default)]
struct Sample {
    /// The segments in it, as (hash, id), lowest first.
    members: Vec<(u64, i64)>,
    /// Their vectors, in that order.
    rows: Vec<f32>,
}

impl Vectors {
    /// All the vectors in the index.
    pub fn load(store: &Store) -> Result<Self> {
        let mut vectors = Self { folder: store.vectors(), ..Default::default() };
        vectors.catch_up(store)?;
        Ok(vectors)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// The rows an exact scan reads: the live ones, and the dead ones until the next generation.
    fn rows(&self) -> usize {
        self.partitions.iter().map(|partition| partition.rows).sum()
    }

    /// How much of the vectors is in memory now: of every form mapped, and the sample.
    pub fn resident(&self) -> usize {
        let forms = |partition: &Mapped| Form::ALL.map(|form| partition.map(form).resident()).iter().sum::<usize>();
        self.partitions.iter().map(forms).sum::<usize>() + self.sample.rows.len() * 4
    }

    /// Catches up with the indexer's commits: only the new rows when nothing was removed or moved.
    fn catch_up(&mut self, store: &Store) -> Result<()> {
        // A partition's files go when its next generation is written, which can be between reading where its rows
        // are and mapping them: then that's read again.
        let mut tries = 0;
        loop {
            match self.try_catch_up(store) {
                Err(error) if tries < 3 && is_gone(&error) => tries += 1,
                outcome => return outcome,
            }
        }
    }

    fn try_catch_up(&mut self, store: &Store) -> Result<()> {
        // One read transaction: the partitions and the segments as they were at one moment.
        let snapshot = store.conn.unchecked_transaction()?;
        let partitions = vectors::partitions(&snapshot)?;
        // A next generation moved rows: everything is read again (but for the sample: a vector doesn't change).
        let moved = self.partitions.iter().any(|mapped| {
            partitions.iter().any(|partition| partition.id == mapped.id && partition.generation != mapped.generation)
        });
        if moved {
            let (folder, sample) = (std::mem::take(&mut self.folder), std::mem::take(&mut self.sample));
            *self = Self { folder, sample, ..Default::default() };
        }
        let before = (self.ids.len(), self.max_id);
        // The partitions with new rows are mapped again, as far as their rows go now: all or none.
        let mut maps = Vec::new();
        for partition in &partitions {
            let mapped = self.partitions.iter().position(|mapped| mapped.id == partition.id);
            if mapped.is_none_or(|at| self.partitions[at].rows != partition.rows as usize) {
                let rows = partition.rows as usize;
                let map = |form: Form| Map::open(&partition.path(&self.folder, form), rows * form.row_bytes());
                maps.push((
                    mapped,
                    Mapped {
                        id: partition.id,
                        generation: partition.generation,
                        rows,
                        f32: map(Form::F32)?,
                        f16: map(Form::F16)?,
                        sketch: map(Form::Sketch)?,
                        mean: partition.mean().unwrap_or_default().to_vec(),
                        spread: partition.spread(),
                    },
                ));
            }
        }
        for (at, mapped) in maps {
            match at {
                Some(at) => self.partitions[at] = mapped,
                None => self.partitions.push(mapped),
            }
        }
        // Segment ids are never reused, so what's here is still right unless some of it was deleted.
        let kept: i64 = snapshot.query_row(
            "SELECT COUNT(*) FROM segments WHERE row IS NOT NULL AND id <= ?1",
            [self.max_id],
            |row| row.get(0),
        )?;
        if kept as usize != self.ids.len() {
            self.drop_deleted(&snapshot)?;
        }
        let mut statement = snapshot.prepare(
            "SELECT s.id, s.file_id, f.kind, s.partition, s.row FROM segments s JOIN files f ON f.id = s.file_id
             WHERE s.row IS NOT NULL AND s.id > ?1 ORDER BY s.id",
        )?;
        let mut rows = statement.query([self.max_id])?;
        while let Some(row) = rows.next()? {
            let (id, file, kind, partition, place): (i64, i64, String, i64, i64) =
                (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
            let Some(partition) = self.partitions.iter().position(|mapped| mapped.id == partition) else { continue };
            self.index.insert(id, self.ids.len());
            self.ids.push(id);
            self.files.push(file);
            self.kinds.push(Kind::parse(&kind).unwrap_or(Kind::Plaintext));
            self.places.push((partition, place as usize));
            self.max_id = self.max_id.max(id);
        }
        if (self.ids.len(), self.max_id) != before {
            self.resample();
        }
        Ok(())
    }

    /// Drops the vectors whose segments are gone (a file changed or left), keeping the rest in place.
    fn drop_deleted(&mut self, conn: &rusqlite::Connection) -> Result<()> {
        let mut statement = conn.prepare("SELECT id FROM segments WHERE row IS NOT NULL AND id <= ?1")?;
        let present: std::collections::HashSet<i64> =
            statement.query_map([self.max_id], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
        let mut kept = 0;
        for at in 0..self.ids.len() {
            if present.contains(&self.ids[at]) {
                self.ids[kept] = self.ids[at];
                self.files[kept] = self.files[at];
                self.kinds[kept] = self.kinds[at];
                self.places[kept] = self.places[at];
                kept += 1;
            }
        }
        self.ids.truncate(kept);
        self.files.truncate(kept);
        self.kinds.truncate(kept);
        self.places.truncate(kept);
        self.index = self.ids.iter().enumerate().map(|(at, &id)| (id, at)).collect();
        Ok(())
    }

    /// Takes the sample again from the segments there are now, copying in only the vectors it didn't have.
    fn resample(&mut self) {
        let mut members: Vec<(u64, i64)> = self.ids.iter().map(|&id| (mix(id as u64), id)).collect();
        if members.len() > SAMPLE {
            members.select_nth_unstable(SAMPLE);
            members.truncate(SAMPLE);
        }
        members.sort_unstable();
        if members == self.sample.members {
            return;
        }
        let mut rows = Vec::with_capacity(members.len() * DIM);
        for member in &members {
            match self.sample.members.binary_search(member) {
                Ok(at) => rows.extend_from_slice(&self.sample.rows[at * DIM..(at + 1) * DIM]),
                Err(_) => {
                    let (partition, row) = self.places[self.index[&member.1]];
                    rows.extend_from_slice(self.partitions[partition].f32.f32_row(row));
                }
            }
        }
        self.sample = Sample { members, rows };
    }

    /// The median of the query's cosines with the sample.
    fn median(&self, query: &[f32]) -> f32 {
        median(&vectors::dots(&self.sample.rows, query))
    }

    /// The query's cosine with each vector of a kind `wanted` takes, from the rows in `form`: every one exactly, or with
    /// sketches the `candidates` whose sketches come closest. And with each of `also` (places in `ids`), exactly. None
    /// for the others.
    fn cosines(
        &self,
        query: &[f32],
        form: Form,
        candidates: usize,
        wanted: &dyn Fn(Kind) -> bool,
        also: &[usize],
    ) -> Vec<Option<f32>> {
        let mut cosines = vec![None; self.ids.len()];
        let wanted: Vec<usize> = (0..self.ids.len()).filter(|&at| wanted(self.kinds[at])).collect();
        // Only partitions with rows of the kinds wanted are read.
        let mut read = vec![false; self.partitions.len()];
        for &at in &wanted {
            read[self.places[at].0] = true;
        }
        let exact = |at: usize| {
            let (partition, row) = self.places[at];
            vectors::dot(self.partitions[partition].f32.f32_row(row), query)
        };
        match form {
            Form::Sketch => {
                let estimates = self.estimates(query, &read);
                let mut closest: Vec<(f32, usize)> = wanted
                    .into_iter()
                    .map(|at| {
                        let (partition, row) = self.places[at];
                        (estimates[partition][row], at)
                    })
                    .collect();
                if closest.len() > candidates {
                    closest.select_nth_unstable_by(candidates, |a, b| b.0.total_cmp(&a.0));
                    closest.truncate(candidates);
                }
                // In the order of the rows: each page they're on comes in once, and in order.
                closest.sort_unstable_by_key(|&(_, at)| self.places[at]);
                for (_, at) in closest {
                    cosines[at] = Some(exact(at));
                }
            }
            form => {
                let scores: Vec<Vec<f32>> = self
                    .partitions
                    .iter()
                    .zip(&read)
                    .map(
                        |(partition, &read)| {
                            if read { vectors::scores(partition.map(form), form, query) } else { Vec::new() }
                        },
                    )
                    .collect();
                for at in wanted {
                    let (partition, row) = self.places[at];
                    cosines[at] = Some(scores[partition][row]);
                }
            }
        }
        for &at in also {
            if cosines[at].is_none() {
                cosines[at] = Some(exact(at));
            }
        }
        cosines
    }

    /// Each sketch's estimate of its row's cosine with the query, in the partitions to `read`.
    fn estimates(&self, query: &[f32], read: &[bool]) -> Vec<Vec<f32>> {
        self.partitions
            .iter()
            .zip(read)
            .map(|(partition, &read)| {
                if read {
                    SketchQuery::new(query, &partition.mean, &partition.spread).estimates(&partition.sketch)
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    /// Where each of these segments comes among the vectors of a kind `wanted` takes, by its sketch's estimate (from
    /// 0), and how many of those there are: how many candidates a search with sketches needs.
    fn sketch_ranks(
        &self,
        query: &[f32],
        wanted: &dyn Fn(Kind) -> bool,
        segments: &[i64],
    ) -> (Vec<Option<usize>>, usize) {
        let estimates = self.estimates(query, &vec![true; self.partitions.len()]);
        let estimate = |at: usize| {
            let (partition, row) = self.places[at];
            estimates[partition][row]
        };
        let mut all: Vec<f32> = (0..self.ids.len()).filter(|&at| wanted(self.kinds[at])).map(estimate).collect();
        all.sort_unstable_by(|a, b| b.total_cmp(a));
        let ranks = segments
            .iter()
            .map(|segment| {
                let at = *self.index.get(segment)?;
                Some(all.partition_point(|&other| other > estimate(at)))
            })
            .collect();
        (ranks, all.len())
    }
}

/// splitmix64's finalizer: a segment id's place in the sample.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

/// A file that's not there (any more).
fn is_gone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

/// "code: retry with backoff": a search of code, and only of code. Without the prefix (or the code filter), a search
/// never shows code: a project's thousands of stretches would bury everything else.
pub const CODE_PREFIX: &str = "code:";

/// Which kinds of file a search shows: only code in a search of code, else the kinds asked for, never code.
fn wanted(request: &Search) -> impl Fn(Kind) -> bool + use<> {
    let (code, kinds) = (is_code(request), request.kinds.clone());
    move |kind| {
        if code { kind == Kind::Code } else { kind != Kind::Code && (kinds.is_empty() || kinds.contains(&kind)) }
    }
}

/// A search of code: the prefix, or the code filter alone.
pub fn is_code(request: &Search) -> bool {
    has_prefix(request) || request.kinds == [Kind::Code]
}

fn has_prefix(request: &Search) -> bool {
    request.q.trim_start().get(..CODE_PREFIX.len()).is_some_and(|start| start.eq_ignore_ascii_case(CODE_PREFIX))
}

/// The words searched for, without the prefix.
pub fn text(request: &Search) -> &str {
    if has_prefix(request) { request.q.trim_start()[CODE_PREFIX.len()..].trim() } else { request.q.trim() }
}

/// Parts in quotes ("INV-2024-0113", or curly quotes): words that must be there, in that order.
pub fn phrases(request: &Search) -> Vec<Vec<String>> {
    let text = text(request).replace(['“', '”', '„'], "\"");
    text.split('"').skip(1).step_by(2).map(words_of).filter(|words| !words.is_empty()).collect()
}

/// The query's words decide where they're found: it has words the model can't read (a number or a code such as
/// INV-2024-0113), it's a search of code, or words are in quotes. Other words the model reads, so where they're found
/// only breaks near-ties.
pub fn words_decide(request: &Search) -> bool {
    is_code(request)
        || !phrases(request).is_empty()
        || words_of(text(request)).iter().any(|word| word.chars().any(char::is_numeric))
}

/// The prompt a search is embedded with (quotes don't mean anything to the model).
pub fn prompt(request: &Search) -> String {
    let text = text(request).replace(['"', '“', '”', '„'], "");
    if is_code(request) { crate::embedder::prompts::code_query(&text) } else { crate::embedder::prompts::query(&text) }
}

/// A segment in the running for its file.
#[derive(Clone, Copy)]
struct Candidate {
    segment: i64,
    score: f32,
    words: bool,
}

impl Searcher {
    /// A searcher of the index at `path`, ranking by `ranking` and scanning the vectors as `search` says (config.json's).
    /// It searches by words until vectors are loaded.
    pub fn open(path: &Path, ranking: Ranking, search: config::Search) -> Result<Self> {
        let store = Store::open_readonly(path)?;
        let mut searcher = Self {
            store,
            ranking,
            search,
            file_kinds: HashMap::new(),
            data_version: -1,
            compatible: true,
            vectors: Vectors::default(),
            loaded: false,
            penalties: Vec::new(),
        };
        searcher.refresh()?;
        Ok(searcher)
    }

    /// Loads the index's vectors, for searches by meaning.
    pub fn load_vectors(&mut self) -> Result<()> {
        let vectors = Vectors::load(&self.store)?;
        self.set_vectors(vectors)
    }

    /// Takes vectors loaded elsewhere (on another connection, off the searching thread), and catches them up with
    /// what the indexer committed meanwhile.
    pub fn set_vectors(&mut self, mut vectors: Vectors) -> Result<()> {
        vectors.catch_up(&self.store)?;
        self.penalties = length_penalties(&vectors.files, &self.ranking);
        self.vectors = vectors;
        self.loaded = true;
        Ok(())
    }

    /// Lets the vectors go: searches are by words until they're loaded again.
    pub fn unload(&mut self) {
        self.vectors = Vectors::default();
        self.penalties = Vec::new();
        self.loaded = false;
    }

    /// Ranks by these values from the next search on.
    pub fn set_ranking(&mut self, ranking: Ranking) {
        self.penalties = length_penalties(&self.vectors.files, &ranking);
        self.ranking = ranking;
    }

    /// Scans the vectors as `search` says from the next search on. Every form is mapped, so that's all it takes.
    pub fn set_search(&mut self, search: config::Search) {
        self.search = search;
    }

    /// The form searches scan: the one `search.vectors` says, or with `auto` the most precise whose rows fit in
    /// `search.memory_mb`.
    pub fn form(&self) -> Form {
        match self.search.vectors {
            Scan::ExactF32 => Form::F32,
            Scan::ExactF16 => Form::F16,
            Scan::Sketch => Form::Sketch,
            Scan::Auto => {
                let (budget, rows) = (self.search.memory_mb.bytes(MEMORY_SHARE), self.vectors.rows() as u64);
                let fits = |form: &Form| rows * form.row_bytes() as u64 <= budget;
                [Form::F32, Form::F16].into_iter().find(fits).unwrap_or(Form::Sketch)
            }
        }
    }

    /// The form searches scan, as `search.vectors` names it.
    pub fn scan(&self) -> Scan {
        match self.form() {
            Form::F32 => Scan::ExactF32,
            Form::F16 => Scan::ExactF16,
            Form::Sketch => Scan::Sketch,
        }
    }

    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    /// How much of the vectors is in memory now (see Vectors::resident).
    pub fn vectors_resident(&self) -> usize {
        self.vectors.resident()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Catches up with the indexer's commits: the files' kinds, and the vectors when they're loaded.
    pub fn refresh(&mut self) -> Result<bool> {
        let version = self.store.data_version()?;
        if version == self.data_version {
            return Ok(false);
        }
        self.data_version = version;
        self.compatible =
            self.store.meta("vector_source")?.is_none_or(|source| source == crate::embedder::vector_source());
        let mut statement = self.store.conn.prepare("SELECT id, kind FROM files")?;
        self.file_kinds = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|row| row.ok())
            .filter_map(|(id, kind)| Kind::parse(&kind).map(|kind| (id, kind)))
            .collect();
        drop(statement);
        if self.loaded {
            self.vectors.catch_up(&self.store)?;
            self.penalties = length_penalties(&self.vectors.files, &self.ranking);
        }
        Ok(true)
    }

    /// A search, scanning the vectors in the form `search.vectors` says.
    pub fn search(&mut self, request: &Search, query: Option<&[f32]>) -> Result<Results> {
        let (form, candidates) = (self.form(), self.candidates());
        self.search_in(request, query, form, candidates)
    }

    /// How many vectors a search with sketches scores exactly: CANDIDATES, or a 32nd of the rows in a larger index, as
    /// many rows as the sketches are bytes. The share it takes to keep the exact top 20 falls as an index grows (its
    /// best matches stand further above the rest), so a fixed share keeps up.
    pub fn candidates(&self) -> usize {
        CANDIDATES.max(self.vectors.rows() / 32)
    }

    /// A search, scanning the vectors in `form` (with sketches, scoring the `candidates` closest exactly).
    pub fn search_in(
        &mut self,
        request: &Search,
        query: Option<&[f32]>,
        form: Form,
        candidates: usize,
    ) -> Result<Results> {
        let started = std::time::Instant::now();
        self.refresh()?;
        let ranking = self.ranking.clone();
        let wanted = wanted(request);
        let wanted_file = |file: i64| self.file_kinds.get(&file).is_some_and(|&kind| wanted(kind));

        // Words: the segments with all of them, or with the quoted ones.
        let words = words_of(text(request));
        let phrases = phrases(request);
        let matches = if !phrases.is_empty() {
            self.phrase_matches(&phrases, &wanted_file, 5000)?
        } else if !words.is_empty() {
            self.word_matches(&words, request.prefix_last, &wanted_file, 1000)?
        } else {
            Vec::new()
        };

        // Meaning: the vectors' cosines (with sketches: the closest, and those with the words), and the query's median.
        let query = query.filter(|_| self.compatible && self.loaded);
        let vectors = &self.vectors;
        let (cosines, median) = match query {
            Some(query) if vectors.len() > 0 => {
                let also: Vec<usize> =
                    matches.iter().filter_map(|segment| vectors.index.get(segment).copied()).collect();
                let cosines = vectors.cosines(query, form, candidates, &wanted, &also);
                let n = vectors.len() as f32;
                let prior = ranking.prior_median * ranking.prior_weight;
                let median = (vectors.median(query) * n + prior) / (n + ranking.prior_weight);
                (cosines, median)
            }
            _ => (Vec::new(), 0.0),
        };
        let cosine_of = |segment: i64| vectors.index.get(&segment).and_then(|&at| cosines.get(at).copied().flatten());
        let lift_of = |segment: i64| cosine_of(segment).map(|c| c - median);
        // The lift less what the length of the segment's file explains: what results are ranked and shown by.
        let penalty = |at: usize| self.penalties.get(at).copied().unwrap_or(0.0);
        let fit_of = |segment: i64| {
            let at = *vectors.index.get(&segment)?;
            Some(cosines.get(at).copied().flatten()? - median - penalty(at))
        };

        let mut candidates: HashMap<i64, Candidate> = HashMap::new();
        let exact = words_decide(request);
        let bonus = if exact { ranking.word_bonus } else { ranking.plain_word_bonus };
        if !phrases.is_empty() {
            // Quoted words must be there exactly: only segments with them, ranked by meaning among themselves.
            let count = matches.len().max(1) as f32;
            for (rank, &segment) in matches.iter().enumerate() {
                let fit = fit_of(segment).unwrap_or(0.0).max(0.0);
                let score = ranking.word_bonus + fit + 0.02 * (1.0 - rank as f32 / count);
                candidates.insert(segment, Candidate { segment, score, words: true });
            }
        } else {
            // At most `per_file` of a file before the 5000 are taken (only the kinds asked for have cosines).
            let mut shown: Vec<(usize, f32)> = cosines
                .iter()
                .enumerate()
                .filter_map(|(at, cosine)| Some((at, (*cosine)? - median - penalty(at))))
                .filter(|&(_, fit)| fit >= ranking.shown_lift)
                .collect();
            shown.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut per_file: HashMap<i64, usize> = HashMap::new();
            let taken = shown.into_iter().filter(|&(at, _)| {
                let count = per_file.entry(vectors.files[at]).or_default();
                *count += 1;
                *count <= ranking.per_file
            });
            for (at, fit) in taken.take(5000) {
                candidates.insert(vectors.ids[at], Candidate { segment: vectors.ids[at], score: fit, words: false });
            }
            let count = matches.len().max(1) as f32;
            for (rank, &segment) in matches.iter().enumerate() {
                // Earlier in the full-text ranking breaks ties between files that only match by words.
                let score = bonus + fit_of(segment).unwrap_or(0.0).max(0.0) + 0.02 * (1.0 - rank as f32 / count);
                candidates.insert(segment, Candidate { segment, score, words: true });
            }
        }

        // Group by file, keeping the segments close to each file's best.
        let mut by_file: HashMap<i64, Vec<Candidate>> = HashMap::new();
        for candidate in candidates.into_values() {
            let file = match vectors.index.get(&candidate.segment) {
                Some(&at) => vectors.files[at],
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
                segments.retain(|c| c.score >= best - ranking.close);
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
            hit.cosine = cosine_of(chosen);
            hit.meaning = best_fit >= ranking.shown_lift;
            hit.words = has_words;
            let strong = ranking.strong_lift;
            hit.strong = best_fit >= strong || (has_words && (exact || score >= strong || score >= meaning_best));
            hits.push(hit);
        }
        // Strong matches first, then the weaker ones, each by score.
        hits.sort_by(|a, b| b.strong.cmp(&a.strong).then(b.score.total_cmp(&a.score)));
        Ok(Results { hits, meaning: query.is_some(), ms: started.elapsed().as_secs_f64() * 1000.0 })
    }

    /// Where each of these segments comes by its sketch's estimate for the query, among the vectors the request wants
    /// (from 0; None for a segment without a vector), and how many vectors it wants.
    pub fn sketch_ranks(
        &mut self,
        request: &Search,
        query: &[f32],
        segments: &[i64],
    ) -> Result<(Vec<Option<usize>>, usize)> {
        self.refresh()?;
        Ok(self.vectors.sketch_ranks(query, &wanted(request), segments))
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
    pub fn recent(&self, request: &Search) -> Result<Results> {
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
        Ok(Results { hits, meaning: false, ms: 0.0 })
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

    /// The segments an FTS5 query matches, best first (bm25): only of files `wanted` takes, at most `per_file` of one,
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
            if *count < self.ranking.per_file && wanted(file) {
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
                    kind: Kind::parse(&row.get::<_, String>(1)?).unwrap_or(Kind::Plaintext),
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
        if let (Segment::Passage, Kind::Code | Kind::Plaintext, Some(first), false) =
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

/// For each vector, what its lift is lowered by for the length of its file: length_penalty × ln(n / length_free) for a
/// file of n vectors, nothing for a short one.
fn length_penalties(files: &[i64], ranking: &Ranking) -> Vec<f32> {
    let mut lengths: HashMap<i64, usize> = HashMap::new();
    for &file in files {
        *lengths.entry(file).or_default() += 1;
    }
    let penalty = |n: usize| ranking.length_penalty * (n as f32 / ranking.length_free).ln().max(0.0);
    files.iter().map(|file| penalty(lengths[file])).collect()
}

/// The median of the scores (0 for none).
fn median(scores: &[f32]) -> f32 {
    let mut scores = scores.to_vec();
    if scores.is_empty() {
        return 0.0;
    }
    let middle = scores.len() / 2;
    let (_, median, _) = scores.select_nth_unstable_by(middle, |a, b| a.total_cmp(b));
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

    /// An index in a folder of its own, removed when the test ends.
    struct TempIndex(std::path::PathBuf);
    impl TempIndex {
        fn new(name: &str) -> Self {
            let folder = std::env::temp_dir().join(format!("sift-it-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&folder);
            Self(folder.join("index.sqlite"))
        }
    }
    impl Drop for TempIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    /// A file of `kind` whose passages are `texts`, indexed without vectors.
    fn add(store: &mut Store, path: &str, kind: Kind, texts: Vec<String>) {
        add_segments(store, path, kind, texts.into_iter().map(|text| (text, None)).collect());
    }

    /// A file of `kind` whose passages are these texts, with these vectors.
    fn add_segments(store: &mut Store, path: &str, kind: Kind, passages: Vec<(String, Option<Vec<f32>>)>) -> i64 {
        let settings = crate::config::Settings::BASE;
        let found = crate::crawl::Found { path: path.into(), kind, size: 1, modified: 1.0, settings };
        store.queue(&found, None, 0.0).unwrap();
        let row = store.pending(100).unwrap().into_iter().find(|row| row.path == Path::new(path)).unwrap();
        let segments: Vec<NewSegment> = passages
            .into_iter()
            .enumerate()
            .map(|(i, (text, vector))| NewSegment {
                segment: Segment::Passage,
                loc: Some(i as f64 + 1.0),
                loc_end: None,
                text: Some(text),
                vector,
            })
            .collect();
        assert!(store.finish_file(&row, &segments, &serde_json::json!({}), false).unwrap());
        row.id
    }

    #[test]
    fn the_vectors_are_followed_through_new_rows_and_a_next_generation() {
        let index = TempIndex::new("generations");
        let mut store = Store::open(&index.0).unwrap();
        let unit = crate::vectors::unit;
        let passage = |seed| (format!("passage {seed}"), Some(unit(seed)));
        let one = add_segments(&mut store, "/a/one.txt", Kind::Plaintext, vec![passage(1), passage(2)]);
        let mut searcher = Searcher::open(&index.0, Ranking::default(), scanning(Scan::ExactF32)).unwrap();
        searcher.load_vectors().unwrap();
        let best = |searcher: &mut Searcher, seed| {
            let request = Search { q: "zebra".into(), limit: 1, ..Default::default() };
            let hit = searcher.search(&request, Some(&unit(seed))).unwrap().hits.remove(0);
            (hit.name, hit.cosine.unwrap())
        };
        assert_eq!(best(&mut searcher, 2).0, "one.txt");

        // New rows are mapped as they come.
        add_segments(&mut store, "/a/two.txt", Kind::Plaintext, vec![passage(3)]);
        let (name, cosine) = best(&mut searcher, 3);
        assert_eq!(name, "two.txt");
        assert!((cosine - 1.0).abs() < 1e-5);

        // Two of three rows dead: the next generation moves the last one, and the searcher follows.
        store.remove_file(one).unwrap();
        assert_eq!(store.compact_vectors().unwrap(), ["main"]);
        let (name, cosine) = best(&mut searcher, 3);
        assert_eq!((name.as_str(), searcher.vector_count()), ("two.txt", 1));
        assert!((cosine - 1.0).abs() < 1e-5);

        // At half precision: nearly the same. With sketches: exactly, for the closest.
        searcher.set_search(scanning(Scan::ExactF16));
        let (name, cosine) = best(&mut searcher, 3);
        assert_eq!(name, "two.txt");
        assert!((cosine - 1.0).abs() < 1e-3);
        searcher.set_search(scanning(Scan::Sketch));
        let (name, cosine) = best(&mut searcher, 3);
        assert_eq!(name, "two.txt");
        assert!((cosine - 1.0).abs() < 1e-5);
    }

    /// Searching as config.json's `search` would with `vectors` set to `scan`.
    fn scanning(scan: Scan) -> config::Search {
        config::Search { vectors: scan, ..Default::default() }
    }

    #[test]
    fn sketches_find_what_the_exact_scan_finds() {
        let index = TempIndex::new("sketches");
        let mut store = Store::open(&index.0).unwrap();
        let unit = crate::vectors::unit;
        // More segments than are scored exactly, and a code file the search mustn't show.
        let files = CANDIDATES / 50 + 20;
        for file in 0..files as u64 {
            let passages = (0..50).map(|n| (format!("passage {n}"), Some(unit(file * 100 + n)))).collect();
            add_segments(&mut store, &format!("/a/{file}.txt"), Kind::Plaintext, passages);
        }
        add_segments(&mut store, "/a/code.rs", Kind::Code, vec![("fn".into(), Some(unit(7 * 100 + 3)))]);
        let mut searcher = Searcher::open(&index.0, Ranking::default(), scanning(Scan::Sketch)).unwrap();
        searcher.load_vectors().unwrap();
        let request = Search { q: "zebra".into(), limit: 1, ..Default::default() };
        let hits = searcher.search(&request, Some(&unit(7 * 100 + 3))).unwrap().hits;
        assert_eq!((hits[0].name.as_str(), hits[0].loc), ("7.txt", Some(4.0)));
        assert!((hits[0].cosine.unwrap() - 1.0).abs() < 1e-5);
        let ranks = searcher.sketch_ranks(&request, &unit(7 * 100 + 3), &[hits[0].segment_id]).unwrap();
        assert_eq!(ranks, (vec![Some(0)], files * 50));
    }

    #[test]
    fn auto_scans_the_most_precise_form_that_fits() {
        let index = TempIndex::new("auto");
        let mut store = Store::open(&index.0).unwrap();
        let unit = crate::vectors::unit;
        let passages = (0..400).map(|n| (format!("passage {n}"), Some(unit(n)))).collect();
        add_segments(&mut store, "/a/one.txt", Kind::Plaintext, passages);
        let mut searcher = Searcher::open(&index.0, Ranking::default(), config::Search::default()).unwrap();
        searcher.load_vectors().unwrap();
        // 400 rows: 1.2 MB at f32, 0.6 MB at f16.
        for (mb, scan) in [(2, Scan::ExactF32), (1, Scan::ExactF16), (0, Scan::Sketch)] {
            searcher.set_search(config::Search { memory_mb: config::Budget::Mb(mb), ..Default::default() });
            assert_eq!(searcher.scan(), scan);
        }
    }

    #[test]
    fn the_sample_takes_the_lowest_hashes_and_follows_the_index() {
        let index = TempIndex::new("sample");
        let mut store = Store::open(&index.0).unwrap();
        let unit = crate::vectors::unit;
        let file = |n: u64| (0..SAMPLE as u64 / 2).map(|i| (format!("{i}"), Some(unit(n * 100_000 + i)))).collect();
        let one = add_segments(&mut store, "/a/one.txt", Kind::Plaintext, file(1));
        add_segments(&mut store, "/a/two.txt", Kind::Plaintext, file(2));
        let mut vectors = Vectors::load(&store).unwrap();
        let check = |vectors: &Vectors| {
            let mut lowest: Vec<(u64, i64)> = vectors.ids.iter().map(|&id| (mix(id as u64), id)).collect();
            lowest.sort_unstable();
            lowest.truncate(SAMPLE);
            assert_eq!(vectors.sample.members, lowest);
            for (at, &(_, id)) in lowest.iter().enumerate() {
                let (partition, row) = vectors.places[vectors.index[&id]];
                let copied = &vectors.sample.rows[at * DIM..(at + 1) * DIM];
                assert_eq!(copied, vectors.partitions[partition].f32.f32_row(row));
            }
        };
        check(&vectors);
        // More than the sample holds: the lowest of them, some of them new.
        add_segments(&mut store, "/a/three.txt", Kind::Plaintext, file(3));
        vectors.catch_up(&store).unwrap();
        check(&vectors);
        // Fewer again, and moved to a next generation: all of them, each with its own vector.
        store.remove_file(one).unwrap();
        store.compact_vectors().unwrap();
        vectors.catch_up(&store).unwrap();
        assert_eq!(vectors.sample.members.len(), SAMPLE);
        check(&vectors);
    }

    #[test]
    fn a_long_file_or_an_unwanted_kind_doesnt_use_up_the_word_matches() {
        let index = TempIndex::new("budget");
        let mut store = Store::open(&index.0).unwrap();
        add(&mut store, "/docs/long.html", Kind::Document, (0..1200).map(|i| format!("needle {i}, needle")).collect());
        add(&mut store, "/notes/note.txt", Kind::Plaintext, vec!["needle extra context in a note".into()]);
        drop(store);
        let mut searcher = Searcher::open(&index.0, Ranking::default(), config::Search::default()).unwrap();
        let search = |searcher: &mut Searcher, kinds: Vec<Kind>| {
            let request = Search { q: "needle".into(), kinds, limit: 10, ..Default::default() };
            searcher.search(&request, None).unwrap().hits.into_iter().map(|hit| hit.path).collect::<Vec<_>>()
        };
        assert_eq!(search(&mut searcher, vec![Kind::Plaintext]), vec!["/notes/note.txt"]);
        assert_eq!(search(&mut searcher, vec![]).len(), 2);
    }

    #[test]
    fn quoted_parts_are_phrases() {
        let request = Search { q: r#"invoice "INV-2024-0113" from “Altink Design”"#.into(), ..Default::default() };
        assert_eq!(phrases(&request), vec![vec!["inv", "2024", "0113"], vec!["altink", "design"]]);
        assert!(!prompt(&request).contains('"'));
        let plain = Search { q: "no quotes here".into(), ..Default::default() };
        assert!(phrases(&plain).is_empty());
    }

    #[test]
    fn numbers_codes_and_quotes_decide() {
        let decides = |q: &str| words_decide(&Search { q: q.into(), ..Default::default() });
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
        let ranking = Ranking::default();
        let penalties = length_penalties(&files, &ranking);
        assert_eq!(penalties[0], 0.0);
        assert_eq!(penalties[files.len() - 1], 0.0);
        let (forty, four_hundred) = (penalties[4], penalties[44]);
        assert!((forty - ranking.length_penalty * 10f32.ln()).abs() < 1e-6);
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
