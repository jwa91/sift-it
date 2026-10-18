//! The vectors as searches read them: each partition's files mapped in every form, which segment each live row is,
//! caught up with the writer's commits, and scanned in the form a search asks for. An exact form scores every vector.
//! With sketches, only the few thousand whose sketches come closest are scored, exactly, and so are the segments a
//! search names besides (those with its words). A query's median is taken from a fixed sample of the vectors.

use super::Reader;
use super::vectors::{self, Form, Map, SketchQuery};
use crate::embedder::DIM;
use anyhow::Result;
use protocol::Kind;
use std::collections::HashMap;
use std::path::PathBuf;

/// How many segments a query's median is taken from (all of them, in a smaller index).
const SAMPLE: usize = 8192;

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
    pub fn load(reader: &Reader) -> Result<Self> {
        let mut vectors = Self { folder: reader.vectors_folder(), ..Default::default() };
        vectors.catch_up(reader)?;
        Ok(vectors)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Each vector's segment, by its place (as `cosines` gives them).
    pub fn segments(&self) -> &[i64] {
        &self.ids
    }

    /// Each vector's file, by its place.
    pub fn files(&self) -> &[i64] {
        &self.files
    }

    /// A segment's place among the vectors (None without one).
    pub fn place(&self, segment: i64) -> Option<usize> {
        self.index.get(&segment).copied()
    }

    /// The rows an exact scan reads: the live ones, and the dead ones until the next generation.
    pub fn rows(&self) -> usize {
        self.partitions.iter().map(|partition| partition.rows).sum()
    }

    /// How much of the vectors is in memory now: of every form mapped, and the sample.
    pub fn resident(&self) -> usize {
        let forms = |partition: &Mapped| Form::ALL.map(|form| partition.map(form).resident()).iter().sum::<usize>();
        self.partitions.iter().map(forms).sum::<usize>() + self.sample.rows.len() * 4
    }

    /// Catches up with the indexer's commits: only the new rows when nothing was removed or moved.
    pub fn catch_up(&mut self, reader: &Reader) -> Result<()> {
        // A partition's files go when its next generation is written, which can be between reading where its rows
        // are and mapping them: then that's read again.
        let mut tries = 0;
        loop {
            match self.try_catch_up(reader) {
                Err(error) if tries < 3 && is_gone(&error) => tries += 1,
                outcome => return outcome,
            }
        }
    }

    fn try_catch_up(&mut self, reader: &Reader) -> Result<()> {
        // One read transaction: the partitions and the segments as they were at one moment.
        let snapshot = reader.conn.unchecked_transaction()?;
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
    pub fn median(&self, query: &[f32]) -> f32 {
        median(&vectors::dots(&self.sample.rows, query))
    }

    /// The query's cosine with each vector of a kind `wanted` takes, from the rows in `form`: every one exactly, or with
    /// sketches the `candidates` whose sketches come closest. And with each of `also` (places in `ids`), exactly. None
    /// for the others.
    pub fn cosines(
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
    pub fn sketch_ranks(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Writer;
    use crate::index::testing::{TempIndex, add, unit};

    #[test]
    fn the_sample_takes_the_lowest_hashes_and_follows_the_index() {
        let index = TempIndex::new("sample");
        let mut store = Writer::open(&index.path).unwrap();
        let file = |n: u64| (0..SAMPLE as u64 / 2).map(|i| (format!("{i}"), Some(unit(n * 100_000 + i)))).collect();
        let one = add(&mut store, "/a/one.txt", Kind::Plaintext, file(1));
        add(&mut store, "/a/two.txt", Kind::Plaintext, file(2));
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
        add(&mut store, "/a/three.txt", Kind::Plaintext, file(3));
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
    fn median_of_odd_and_even() {
        assert_eq!(median(&[0.3, 0.1, 0.2]), 0.2);
        assert_eq!(median(&[0.4, 0.1, 0.3, 0.2]), 0.3);
    }
}
