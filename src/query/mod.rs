//! The query path: hybrid search by meaning (the model's vectors) and by exact words (the full-text index), one
//! result per file. A request goes through four stages, each in its own module:
//!
//! - understand: its words, the parts in quotes, the kinds it shows, the prompt it's embedded with;
//! - retrieve: the segments with its words, and the vectors' cosines with it, in the form the search scans;
//! - rank: from those scores to files, best first (pure);
//! - present: where each result opens, and its excerpt.
//!
//! The Searcher holds what they work on (the index, and its vectors once loaded) and runs them in turn.

mod present;
mod rank;
mod retrieve;
pub mod understand;

pub use understand::Query;

use crate::config::{self, Ranking};
use crate::index::{Form, Reader, Vectors};
use crate::resources::SearchPlan;
use anyhow::Result;
use protocol::{Kind, Results, Search};
use rank::Scored;
use std::collections::HashMap;
use std::path::Path;

pub struct Searcher {
    index: Reader,
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

impl Searcher {
    /// A searcher of the index at `path`, ranking by `ranking` and scanning the vectors as `search` says (config.json's).
    /// It searches by words until vectors are loaded.
    pub fn open(path: &Path, ranking: Ranking, search: config::Search) -> Result<Self> {
        let mut searcher = Self {
            index: Reader::open(path)?,
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
        let vectors = Vectors::load(&self.index)?;
        self.set_vectors(vectors)
    }

    /// Takes vectors loaded elsewhere (on another connection, off the searching thread), and catches them up with
    /// what the indexer committed meanwhile.
    pub fn set_vectors(&mut self, mut vectors: Vectors) -> Result<()> {
        vectors.catch_up(&self.index)?;
        self.penalties = rank::length_penalties(vectors.files(), &self.ranking);
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
        self.penalties = rank::length_penalties(self.vectors.files(), &ranking);
        self.ranking = ranking;
    }

    /// Scans the vectors as `search` says from the next search on. Every form is mapped, so that's all it takes.
    pub fn set_search(&mut self, search: config::Search) {
        self.search = search;
    }

    /// What searches scan now: the form of the vectors, and how many a search with sketches scores exactly.
    pub fn plan(&self) -> SearchPlan {
        SearchPlan::of(&self.search, self.vectors.rows())
    }

    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    /// How much of the vectors is in memory now (see Vectors::resident).
    pub fn vectors_resident(&self) -> usize {
        self.vectors.resident()
    }

    pub fn index(&self) -> &Reader {
        &self.index
    }

    /// Catches up with the indexer's commits: the files' kinds, and the vectors when they're loaded.
    pub fn refresh(&mut self) -> Result<bool> {
        let version = self.index.data_version()?;
        if version == self.data_version {
            return Ok(false);
        }
        self.data_version = version;
        self.compatible = self.index.meta("vector_source")?.is_none_or(|source| source == embedder::vector_source());
        self.file_kinds = self.index.file_kinds()?;
        if self.loaded {
            self.vectors.catch_up(&self.index)?;
            self.penalties = rank::length_penalties(self.vectors.files(), &self.ranking);
        }
        Ok(true)
    }

    /// A search, scanning the vectors as the plan says.
    pub fn search(&mut self, request: &Search, vector: Option<&[f32]>) -> Result<Results> {
        let plan = self.plan();
        self.search_in(request, vector, plan.form, plan.candidates)
    }

    /// A search, scanning the vectors in `form` (with sketches, scoring the `candidates` closest exactly).
    pub fn search_in(
        &mut self,
        request: &Search,
        vector: Option<&[f32]>,
        form: Form,
        candidates: usize,
    ) -> Result<Results> {
        let started = std::time::Instant::now();
        self.refresh()?;
        let query = Query::of(request);
        let wanted = |kind: Kind| query.wants(kind);
        let wanted_file = |file: i64| self.file_kinds.get(&file).is_some_and(|&kind| wanted(kind));

        // Words: the segments with all of them, or with the quoted ones.
        let matches = retrieve::text_matches(&self.index, &query, &wanted_file, self.ranking.per_file)?;

        // Meaning: the vectors' cosines (with sketches: the closest, and those with the words), and the query's median.
        let vector = vector.filter(|_| self.compatible && self.loaded);
        let vectors = &self.vectors;
        let (cosines, median) = match vector {
            Some(vector) if vectors.len() > 0 => {
                let also: Vec<usize> = matches.iter().filter_map(|found| vectors.place(found.segment)).collect();
                let cosines = vectors.cosines(vector, form, candidates, &wanted, &also);
                (cosines, rank::baseline(vectors.median(vector), vectors.len(), &self.ranking))
            }
            _ => (Vec::new(), 0.0),
        };

        let scored = |at: usize| Scored {
            segment: vectors.segments()[at],
            file: vectors.files()[at],
            cosine: cosines.get(at).copied().flatten(),
            penalty: self.penalties.get(at).copied().unwrap_or(0.0),
        };
        let meaning = (0..cosines.len()).filter(|&at| cosines[at].is_some()).map(scored);
        let matches: Vec<Scored> = matches
            .iter()
            .map(|found| match vectors.place(found.segment) {
                Some(at) => scored(at),
                None => Scored { segment: found.segment, file: found.file, cosine: None, penalty: 0.0 },
            })
            .collect();
        let ranked = rank::rank(&query, &self.ranking, median, meaning, &matches, request.limit);

        let mut hits = Vec::with_capacity(ranked.len());
        for file in &ranked {
            hits.extend(present::result(&self.index, file, &query.words)?);
        }
        Ok(Results { hits, meaning: vector.is_some(), ms: started.elapsed().as_secs_f64() * 1000.0 })
    }

    /// Where each of these segments comes by its sketch's estimate for the query, among the vectors the request wants
    /// (from 0; None for a segment without a vector), and how many vectors it wants.
    pub fn sketch_ranks(
        &mut self,
        request: &Search,
        vector: &[f32],
        segments: &[i64],
    ) -> Result<(Vec<Option<usize>>, usize)> {
        self.refresh()?;
        let query = Query::of(request);
        Ok(self.vectors.sketch_ranks(vector, &|kind| query.wants(kind), segments))
    }

    /// A segment's whole text.
    pub fn segment_text(&self, segment_id: i64) -> Result<Option<String>> {
        self.index.segment_text(segment_id)
    }

    /// A file's text: its text segments in order (see Reader::file_text).
    pub fn file_text(&self, path: &str) -> Result<Option<String>> {
        self.index.file_text(path)
    }

    /// Recently changed files, for an empty query.
    pub fn recent(&self, request: &Search) -> Result<Results> {
        let mut hits = Vec::new();
        for segment in retrieve::recent(&self.index, request)? {
            if let Some(mut hit) = present::hit(&self.index, segment, &[])? {
                hit.strong = true;
                hits.push(hit);
            }
        }
        Ok(Results { hits, meaning: false, ms: 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Writer;
    use crate::index::testing::{TempIndex, add, unit};
    use protocol::Scan;

    #[test]
    fn the_vectors_are_followed_through_new_rows_and_a_next_generation() {
        let index = TempIndex::new("generations");
        let mut store = Writer::open(&index.path).unwrap();
        let passage = |seed| (format!("passage {seed}"), Some(unit(seed)));
        let one = add(&mut store, "/a/one.txt", Kind::Plaintext, vec![passage(1), passage(2)]);
        let mut searcher = Searcher::open(&index.path, Ranking::default(), scanning(Scan::ExactF32)).unwrap();
        searcher.load_vectors().unwrap();
        let best = |searcher: &mut Searcher, seed| {
            let request = Search { q: "zebra".into(), limit: 1, ..Default::default() };
            let hit = searcher.search(&request, Some(&unit(seed))).unwrap().hits.remove(0);
            (hit.name, hit.cosine.unwrap())
        };
        assert_eq!(best(&mut searcher, 2).0, "one.txt");

        // New rows are mapped as they come.
        add(&mut store, "/a/two.txt", Kind::Plaintext, vec![passage(3)]);
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
        let mut store = Writer::open(&index.path).unwrap();
        // More segments than are scored exactly, and a code file the search mustn't show.
        let files = crate::resources::CANDIDATES / 50 + 20;
        for file in 0..files as u64 {
            let passages = (0..50).map(|n| (format!("passage {n}"), Some(unit(file * 100 + n)))).collect();
            add(&mut store, &format!("/a/{file}.txt"), Kind::Plaintext, passages);
        }
        add(&mut store, "/a/code.rs", Kind::Code, vec![("fn".into(), Some(unit(7 * 100 + 3)))]);
        let mut searcher = Searcher::open(&index.path, Ranking::default(), scanning(Scan::Sketch)).unwrap();
        searcher.load_vectors().unwrap();
        let request = Search { q: "zebra".into(), limit: 1, ..Default::default() };
        let hits = searcher.search(&request, Some(&unit(7 * 100 + 3))).unwrap().hits;
        assert_eq!((hits[0].name.as_str(), hits[0].loc), ("7.txt", Some(4.0)));
        assert!((hits[0].cosine.unwrap() - 1.0).abs() < 1e-5);
        let ranks = searcher.sketch_ranks(&request, &unit(7 * 100 + 3), &[hits[0].segment_id]).unwrap();
        assert_eq!(ranks, (vec![Some(0)], files * 50));
    }

    #[test]
    fn a_long_file_or_an_unwanted_kind_doesnt_use_up_the_word_matches() {
        let index = TempIndex::new("budget");
        let mut store = Writer::open(&index.path).unwrap();
        add(
            &mut store,
            "/docs/long.html",
            Kind::Document,
            (0..1200).map(|i| (format!("needle {i}, needle"), None)).collect(),
        );
        add(&mut store, "/notes/note.txt", Kind::Plaintext, vec![("needle extra context in a note".into(), None)]);
        drop(store);
        let mut searcher = Searcher::open(&index.path, Ranking::default(), config::Search::default()).unwrap();
        let search = |searcher: &mut Searcher, kinds: Vec<Kind>| {
            let request = Search { q: "needle".into(), kinds, limit: 10, ..Default::default() };
            searcher.search(&request, None).unwrap().hits.into_iter().map(|hit| hit.path).collect::<Vec<_>>()
        };
        assert_eq!(search(&mut searcher, vec![Kind::Plaintext]), vec!["/notes/note.txt"]);
        assert_eq!(search(&mut searcher, vec![]).len(), 2);
    }
}
