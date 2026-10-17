//! The vector store: every segment's vector in flat files beside the index, which the engine maps and scans in place.
//! SQLite keeps, for each segment, the partition and row that hold its vector (store.rs); this keeps the files and the
//! partitions table.
//!
//!   vectors/<partition>.<generation>.f32      768 floats per row
//!   vectors/<partition>.<generation>.f16      the same rows at half precision
//!   vectors/<partition>.<generation>.sketch   the same rows in 96 bytes: a bit per dimension, set when the value is
//!                                             above the partition's mean
//!
//! Code is a partition of its own, and everything else is `main`. Every row is written in all three forms, so whichever
//! form a search scans is ready, and switching costs nothing.
//!
//! Append, then commit: the indexer writes a segment's rows, and they're on disk before the transaction that points at
//! them commits. Rows past what the partition counts were never committed (a crash, or a rollback), and the next append
//! writes over them.
//!
//! The next generation: rows whose segments are gone stay until a quarter of a partition is dead, and the sketches keep
//! the mean they were made against until the partition's mean moves away from it. Then, while the indexer is idle, it
//! writes the partition's next generation from the live rows, moves their segments to their new rows in one transaction,
//! and removes the old files. A reader that still maps them keeps them until it lets go.

use crate::embedder::DIM;
use anyhow::{Context, Result, bail};
use protocol::Kind;
use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// A partition's sketches are made again once about this share of their bits would come out differently against the
/// partition's mean as it is now.
const RECENTER: f64 = 0.01;

/// The partitions, and how many of their rows are dead: a segment that goes takes its row with it.
pub const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS partitions (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,                -- main, or code
        generation INTEGER NOT NULL DEFAULT 0,    -- in the names of its files
        rows INTEGER NOT NULL DEFAULT 0,          -- rows in this generation's files
        dead INTEGER NOT NULL DEFAULT 0,          -- of those, rows whose segment is gone
        sums BLOB,                                -- the sum of the rows, then of their squares, per dimension (f64)
        mean BLOB                                 -- what the sketches are made against, per dimension (f32)
    );
    CREATE TRIGGER IF NOT EXISTS dead_rows AFTER DELETE ON segments WHEN old.row IS NOT NULL
    BEGIN
        UPDATE partitions SET dead = dead + 1 WHERE id = old.partition;
    END;";

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn cblas_sdot(n: i32, x: *const f32, incx: i32, y: *const f32, incy: i32) -> f32;
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

/// How a file holds its rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Form {
    #[default]
    F32,
    F16,
    Sketch,
}

impl Form {
    pub const ALL: [Form; 3] = [Form::F32, Form::F16, Form::Sketch];

    pub fn row_bytes(self) -> usize {
        match self {
            Form::F32 => DIM * 4,
            Form::F16 => DIM * 2,
            Form::Sketch => DIM / 8,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Form::F32 => "f32",
            Form::F16 => "f16",
            Form::Sketch => "sketch",
        }
    }
}

/// The folder the vector files of the index at `index` are in.
pub fn folder(index: &Path) -> PathBuf {
    index.with_file_name("vectors")
}

/// The partition a file's vectors go to.
pub fn partition_of(kind: Kind) -> &'static str {
    if kind == Kind::Code { "code" } else { "main" }
}

/// A partition, as the partitions table has it.
#[derive(Clone, Debug)]
pub struct Partition {
    pub id: i64,
    pub name: String,
    pub generation: i64,
    pub rows: i64,
    pub dead: i64,
    sums: Sums,
    mean: Option<Vec<f32>>,
}

impl Partition {
    pub fn path(&self, folder: &Path, form: Form) -> PathBuf {
        folder.join(format!("{}.{}.{}", self.name, self.generation, form.extension()))
    }

    /// The mean its sketches are made against (none while it's empty).
    pub fn mean(&self) -> Option<&[f32]> {
        self.mean.as_deref()
    }

    /// How far its rows spread around their mean, per dimension (the standard deviation).
    pub fn spread(&self) -> Vec<f32> {
        self.sums.spread(self.rows)
    }

    /// The partition called `name`, made when it isn't there yet.
    fn named(conn: &Connection, name: &str) -> Result<Self> {
        conn.execute("INSERT OR IGNORE INTO partitions (name) VALUES (?1)", [name])?;
        Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM partitions WHERE name = ?1"), [name], Self::from_row)?)
    }

    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let sums: Option<Vec<u8>> = row.get(5)?;
        let mean: Option<Vec<u8>> = row.get(6)?;
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            generation: row.get(2)?,
            rows: row.get(3)?,
            dead: row.get(4)?,
            sums: sums.map(|blob| Sums::from_blob(&blob)).unwrap_or_default(),
            mean: mean.map(|blob| blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()),
        })
    }

    fn save(&self, conn: &Connection) -> Result<()> {
        let mean: Option<Vec<u8>> = self.mean.as_ref().map(|mean| mean.iter().flat_map(|v| v.to_le_bytes()).collect());
        conn.execute(
            "UPDATE partitions SET generation = ?2, rows = ?3, dead = ?4, sums = ?5, mean = ?6 WHERE id = ?1",
            params![self.id, self.generation, self.rows, self.dead, self.sums.to_blob(), mean],
        )?;
        Ok(())
    }

    /// Due its next generation: a quarter of its rows is dead, or its sketches were made against a mean it has moved
    /// away from.
    fn due(&self) -> bool {
        let drifted = self.mean.as_ref().is_some_and(|mean| self.sums.drift(self.rows, mean) >= RECENTER);
        self.rows > 0 && ((self.dead > 0 && self.dead * 4 >= self.rows) || drifted)
    }
}

const COLUMNS: &str = "id, name, generation, rows, dead, sums, mean";

/// Every partition, by id.
pub fn partitions(conn: &Connection) -> Result<Vec<Partition>> {
    let mut statement = conn.prepare(&format!("SELECT {COLUMNS} FROM partitions ORDER BY id"))?;
    Ok(statement.query_map([], Partition::from_row)?.collect::<std::result::Result<_, _>>()?)
}

/// The sums of a partition's rows and of their squares, per dimension: its mean, and how far its rows spread around it.
#[derive(Clone, Debug, Default)]
struct Sums {
    values: Vec<f64>,
    squares: Vec<f64>,
}

impl Sums {
    fn add(&mut self, row: &[f32]) {
        if self.values.is_empty() {
            *self = Self { values: vec![0.0; DIM], squares: vec![0.0; DIM] };
        }
        for (d, &value) in row.iter().enumerate() {
            self.values[d] += value as f64;
            self.squares[d] += value as f64 * value as f64;
        }
    }

    /// The mean of `rows` rows.
    fn mean(&self, rows: i64) -> Vec<f32> {
        self.values.iter().map(|&sum| (sum / rows.max(1) as f64) as f32).collect()
    }

    /// The standard deviation of `rows` rows, per dimension.
    fn spread(&self, rows: i64) -> Vec<f32> {
        if self.values.is_empty() {
            return vec![0.0; DIM];
        }
        let n = rows.max(1) as f64;
        (0..DIM)
            .map(|d| {
                let centre = self.values[d] / n;
                (self.squares[d] / n - centre * centre).max(0.0).sqrt() as f32
            })
            .collect()
    }

    /// About the share of the bits of `rows` rows' sketches that would come out differently against their mean than
    /// against `mean`. Per dimension, the rows between the two flip: for values spread like a normal distribution
    /// that's 0.4 of the distance in standard deviations, at most half of them.
    fn drift(&self, rows: i64, mean: &[f32]) -> f64 {
        if rows < 2 || self.values.is_empty() {
            return 0.0;
        }
        let n = rows as f64;
        let spread = self.spread(rows);
        let flipped: f64 = (0..DIM)
            .map(|d| {
                let (centre, spread) = (self.values[d] / n, spread[d] as f64);
                if spread > 0.0 { (0.4 * (centre - mean[d] as f64).abs() / spread).min(0.5) } else { 0.0 }
            })
            .sum();
        flipped / DIM as f64
    }

    fn to_blob(&self) -> Option<Vec<u8>> {
        (!self.values.is_empty())
            .then(|| self.values.iter().chain(&self.squares).flat_map(|v| v.to_le_bytes()).collect())
    }

    fn from_blob(blob: &[u8]) -> Self {
        let all: Vec<f64> = blob.as_chunks::<8>().0.iter().map(|b| f64::from_le_bytes(*b)).collect();
        if all.len() != 2 * DIM {
            return Self::default();
        }
        Self { values: all[..DIM].to_vec(), squares: all[DIM..].to_vec() }
    }
}

/// Writes rows to the partition called `partition`, after the ones it counts, and returns its id and the first new
/// row. They're on disk when this returns, so the transaction `conn` is in, which points segments at them, commits after
/// them. It has to hold the write lock (BEGIN IMMEDIATE): no other writer may append between here and its commit.
pub fn append(conn: &Connection, folder: &Path, partition: &str, rows: &[&[f32]]) -> Result<(i64, i64)> {
    debug_assert!(!rows.is_empty(), "a mean of no rows");
    let mut partition = Partition::named(conn, partition)?;
    let first = partition.rows;
    let mean = partition
        .mean
        .get_or_insert_with(|| {
            let mut sums = Sums::default();
            rows.iter().for_each(|row| sums.add(row));
            sums.mean(rows.len() as i64)
        })
        .clone();
    std::fs::create_dir_all(folder)?;
    for form in Form::ALL {
        let mut bytes = Vec::with_capacity(rows.len() * form.row_bytes());
        for row in rows {
            encode(form, row, &mean, &mut bytes);
        }
        write_at(&partition.path(folder, form), first as u64 * form.row_bytes() as u64, &bytes)?;
    }
    rows.iter().for_each(|row| partition.sums.add(row));
    partition.rows += rows.len() as i64;
    partition.save(conn)?;
    Ok((partition.id, first))
}

/// Writes `bytes` at `offset`, in place of whatever was past it (rows never committed), and has them on disk before
/// anything written after them.
fn write_at(path: &Path, offset: u64, bytes: &[u8]) -> Result<()> {
    let file = File::options().create(true).truncate(false).write(true).open(path)?;
    let length = file.metadata()?.len();
    if length < offset {
        bail!("{} has fewer rows than the index says (sift-it index --reindex makes them again)", path.display());
    }
    if length > offset {
        file.set_len(offset)?;
    }
    file.write_all_at(bytes, offset)?;
    barrier(&file, path)
}

/// Writes reach the disk before any written after them (a barrier, not a full flush: the order is what counts).
fn barrier(file: &File, path: &Path) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("can't sync {}", path.display()));
    }
    Ok(())
}

/// A row in a form.
fn encode(form: Form, row: &[f32], mean: &[f32], into: &mut Vec<u8>) {
    match form {
        Form::F32 => into.extend(row.iter().flat_map(|v| v.to_le_bytes())),
        Form::F16 => into.extend(half(row).into_iter().flat_map(u16::to_le_bytes)),
        Form::Sketch => into.extend(sketch(row, mean)),
    }
}

/// A row at half precision (IEEE binary16, rounded to the nearest).
fn half(row: &[f32]) -> Vec<u16> {
    use std::arch::aarch64::*;
    let mut halves = vec![0u16; row.len()];
    for (from, to) in row.as_chunks::<4>().0.iter().zip(halves.as_chunks_mut::<4>().0) {
        // Safety: four floats in, four halves out.
        unsafe { vst1_u16(to.as_mut_ptr(), vreinterpret_u16_f16(vcvt_f16_f32(vld1q_f32(from.as_ptr())))) };
    }
    halves
}

/// A row's sketch: a bit per dimension, set when its value is above the mean's.
fn sketch(row: &[f32], mean: &[f32]) -> [u8; DIM / 8] {
    let mut bits = [0u8; DIM / 8];
    for (d, (value, centre)) in row.iter().zip(mean).enumerate() {
        if value > centre {
            bits[d / 8] |= 1 << (d % 8);
        }
    }
    bits
}

/// Every partition starts over, empty: a new generation with no rows (every segment is gone).
pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE partitions SET generation = generation + 1, rows = 0, dead = 0, sums = NULL, mean = NULL",
        [],
    )?;
    Ok(())
}

/// Writes the next generation of each partition that's due one (see the top), and removes the files of the ones before.
/// Returns the names of the partitions it wrote.
pub fn compact(conn: &mut Connection, folder: &Path) -> Result<Vec<String>> {
    // A look first, so an idle indexer doesn't take the write lock for nothing.
    if !partitions(conn)?.iter().any(Partition::due) {
        return Ok(Vec::new());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut written = Vec::new();
    for partition in partitions(&tx)?.into_iter().filter(Partition::due) {
        next_generation(&tx, folder, &partition)?;
        written.push(partition.name);
    }
    tx.commit()?;
    tidy(conn, folder)?;
    Ok(written)
}

/// Writes a partition's next generation from its live rows, with sketches against their mean, and points its segments
/// at their new rows (in `conn`'s transaction).
fn next_generation(conn: &Connection, folder: &Path, partition: &Partition) -> Result<()> {
    let live: Vec<(i64, i64)> = conn
        .prepare("SELECT id, row FROM segments WHERE partition = ?1 AND row IS NOT NULL ORDER BY row")?
        .query_map([partition.id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let rows = partition.rows as usize;
    let f32s = Map::open(&partition.path(folder, Form::F32), rows * Form::F32.row_bytes())?;
    let f16s = Map::open(&partition.path(folder, Form::F16), rows * Form::F16.row_bytes())?;
    let mut sums = Sums::default();
    for &(_, row) in &live {
        sums.add(f32s.f32_row(row as usize));
    }
    // No live rows, no mean: the next row to come sets one.
    let mean = (!live.is_empty()).then(|| sums.mean(live.len() as i64));
    let next = Partition {
        generation: partition.generation + 1,
        rows: live.len() as i64,
        dead: 0,
        sums,
        mean: mean.clone(),
        ..partition.clone()
    };
    let path = |form| next.path(folder, form);
    for form in Form::ALL {
        // What a crash left of this generation before.
        let _ = std::fs::remove_file(path(form));
    }
    let moved = live.len() < rows;
    if moved {
        let mut f32_file = BufWriter::new(File::create(path(Form::F32))?);
        let mut f16_file = BufWriter::new(File::create(path(Form::F16))?);
        for &(_, row) in &live {
            f32_file.write_all(f32s.bytes(Form::F32, row as usize))?;
            f16_file.write_all(f16s.bytes(Form::F16, row as usize))?;
        }
        for (file, form) in [(f32_file, Form::F32), (f16_file, Form::F16)] {
            barrier(&file.into_inner().map_err(|error| error.into_error())?, &path(form))?;
        }
        let mut update = conn.prepare("UPDATE segments SET row = ?2 WHERE id = ?1")?;
        for (new_row, &(id, _)) in live.iter().enumerate() {
            update.execute(params![id, new_row as i64])?;
        }
    } else {
        // Only the sketches change: the rows stay where they are, in the same files.
        for form in [Form::F32, Form::F16] {
            std::fs::hard_link(partition.path(folder, form), path(form))?;
        }
    }
    let mut sketches = BufWriter::new(File::create(path(Form::Sketch))?);
    for &(_, row) in &live {
        sketches.write_all(&sketch(f32s.f32_row(row as usize), mean.as_deref().unwrap_or_default()))?;
    }
    barrier(&sketches.into_inner().map_err(|error| error.into_error())?, &path(Form::Sketch))?;
    next.save(conn)
}

/// Removes the files that no partition's generation uses: the ones before, and what a crash left.
pub fn tidy(conn: &mut Connection, folder: &Path) -> Result<()> {
    // The write lock: a next generation being written by another indexer isn't left over.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let used: HashSet<PathBuf> =
        partitions(&tx)?.iter().flat_map(|partition| Form::ALL.map(|form| partition.path(folder, form))).collect();
    let Ok(entries) = std::fs::read_dir(folder) else { return Ok(()) };
    for entry in entries.flatten() {
        if !used.contains(&entry.path()) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// A file of rows, mapped read-only. Its pages belong to the file: under memory pressure macOS drops them and reads them
/// again when they're needed, where a copy in memory would be swapped out.
pub struct Map {
    address: *mut libc::c_void,
    length: usize,
}

// Safety: the mapping is read-only, and lives as long as the Map.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    /// The first `length` bytes of the file at `path` (nothing to map for none).
    pub fn open(path: &Path, length: usize) -> Result<Self> {
        if length == 0 {
            return Ok(Self { address: std::ptr::null_mut(), length: 0 });
        }
        let file = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
        if (file.metadata()?.len() as usize) < length {
            bail!("{} has fewer rows than the index says (sift-it index --reindex makes them again)", path.display());
        }
        let address =
            unsafe { libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("can't map {}", path.display()));
        }
        Ok(Self { address, length })
    }

    fn all(&self) -> &[u8] {
        if self.length == 0 {
            return &[];
        }
        // Safety: mapped for `length` bytes until drop.
        unsafe { std::slice::from_raw_parts(self.address as *const u8, self.length) }
    }

    fn bytes(&self, form: Form, row: usize) -> &[u8] {
        &self.all()[row * form.row_bytes()..(row + 1) * form.row_bytes()]
    }

    /// Row `row` of a file of f32 rows.
    pub fn f32_row(&self, row: usize) -> &[f32] {
        &self.f32_rows()[row * DIM..(row + 1) * DIM]
    }

    /// All of a file of f32 rows.
    fn f32_rows(&self) -> &[f32] {
        let bytes = self.all();
        // Safety: the mapping starts at a page, and holds whole floats.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, bytes.len() / 4) }
    }

    /// How much of it is in memory now.
    pub fn resident(&self) -> usize {
        if self.length == 0 {
            return 0;
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut pages = vec![0 as libc::c_char; self.length.div_ceil(page)];
        if unsafe { libc::mincore(self.address, self.length, pages.as_mut_ptr()) } != 0 {
            return 0;
        }
        pages.iter().filter(|&&flags| flags as i32 & libc::MINCORE_INCORE != 0).count() * page
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.length > 0 {
            unsafe { libc::munmap(self.address, self.length) };
        }
    }
}

/// The query's dot product with each of the rows of `map`, which holds rows in `form` (cosines: vectors are
/// normalized).
pub fn scores(map: &Map, form: Form, query: &[f32]) -> Vec<f32> {
    match form {
        Form::F32 => dots(map.f32_rows(), query),
        Form::F16 => map.all().chunks_exact(Form::F16.row_bytes()).map(|row| dot_half(row, query)).collect(),
        Form::Sketch => unreachable!("a sketch only estimates its score (SketchQuery)"),
    }
}

/// The query's dot product with each row of `matrix` (rows of DIM floats, one after the other).
pub fn dots(matrix: &[f32], query: &[f32]) -> Vec<f32> {
    let rows = matrix.len() / DIM;
    let mut scores = vec![0.0f32; rows];
    if rows > 0 {
        // Safety: `matrix` has `rows` rows of DIM, `query` DIM values, `scores` room for `rows`.
        unsafe {
            cblas_sgemv(
                ROW_MAJOR,
                NO_TRANS,
                rows as i32,
                DIM as i32,
                1.0,
                matrix.as_ptr(),
                DIM as i32,
                query.as_ptr(),
                1,
                0.0,
                scores.as_mut_ptr(),
                1,
            )
        };
    }
    scores
}

/// The query's dot product with one row.
pub fn dot(row: &[f32], query: &[f32]) -> f32 {
    // Safety: both have DIM values.
    unsafe { cblas_sdot(DIM as i32, row.as_ptr(), 1, query.as_ptr(), 1) }
}

/// A query, ready to compare with the sketches of one partition.
///
/// A sketch keeps each dimension's side of the partition's mean m: s = +1 above it, -1 below. The query q's dot product
/// with a row x is q·m + q·(x - m), and on its side of the mean a value is about √(2/π) of a standard deviation σ away
/// from it. So the sketch's estimate is q·m + √(2/π) Σ q σ s, which weighs every dimension by how much of the query is
/// in it and by how far rows spread in it. With the weights w = qσ that's q·m + √(2/π) (2 Σ w over the set bits - Σ w).
/// The weights go into four bit planes (each rounded to one of 16 steps between the least and the most), so the sum
/// over the set bits is five counts of bits per sketch.
pub struct SketchQuery {
    planes: [[u8; DIM / 8]; 4],
    /// The least weight, and the step between two of the 16.
    low: f32,
    step: f32,
    /// The estimate is base + scale × (the sum of the weights of the set bits).
    base: f32,
    scale: f32,
}

impl SketchQuery {
    pub fn new(query: &[f32], mean: &[f32], spread: &[f32]) -> Self {
        let weights: Vec<f32> = query.iter().zip(spread).map(|(q, s)| q * s).collect();
        let low = weights.iter().copied().fold(f32::INFINITY, f32::min);
        let high = weights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let step = if high > low { (high - low) / 15.0 } else { 1.0 };
        let mut planes = [[0u8; DIM / 8]; 4];
        for (d, weight) in weights.iter().enumerate() {
            let level = ((weight - low) / step).round() as u8;
            for (plane, bits) in planes.iter_mut().enumerate() {
                if level >> plane & 1 == 1 {
                    bits[d / 8] |= 1 << (d % 8);
                }
            }
        }
        let side = (2.0 / std::f32::consts::PI).sqrt();
        let centre: f32 = query.iter().zip(mean).map(|(q, m)| q * m).sum();
        Self { planes, low, step, base: centre - side * weights.iter().sum::<f32>(), scale: 2.0 * side }
    }

    /// Each sketch's estimate of its row's cosine with the query.
    pub fn estimates(&self, sketches: &Map) -> Vec<f32> {
        self.estimate_all(sketches.all())
    }

    fn estimate_all(&self, sketches: &[u8]) -> Vec<f32> {
        use std::arch::aarch64::*;
        const CHUNKS: usize = DIM / 128;
        let mut estimates = Vec::with_capacity(sketches.len() / (DIM / 8));
        // Safety: a sketch and each plane are DIM / 8 bytes, CHUNKS times 16.
        unsafe {
            let planes: [[uint8x16_t; CHUNKS]; 4] =
                std::array::from_fn(|k| std::array::from_fn(|c| vld1q_u8(self.planes[k].as_ptr().add(c * 16))));
            for sketch in sketches.as_chunks::<{ DIM / 8 }>().0 {
                let mut ones = vdupq_n_u8(0);
                let mut counts = [vdupq_n_u8(0); 4];
                for (c, chunk) in sketch.as_chunks::<16>().0.iter().enumerate() {
                    let bits = vld1q_u8(chunk.as_ptr());
                    ones = vaddq_u8(ones, vcntq_u8(bits));
                    for (count, plane) in counts.iter_mut().zip(&planes) {
                        *count = vaddq_u8(*count, vcntq_u8(vandq_u8(bits, plane[c])));
                    }
                }
                // A lane counts at most 8 bits a chunk: 48 fits a byte, but not once weighed by its plane.
                let levels = vaddq_u16(
                    vaddq_u16(vpaddlq_u8(counts[0]), vshlq_n_u16::<1>(vpaddlq_u8(counts[1]))),
                    vaddq_u16(vshlq_n_u16::<2>(vpaddlq_u8(counts[2])), vshlq_n_u16::<3>(vpaddlq_u8(counts[3]))),
                );
                let (ones, levels) = (vaddlvq_u8(ones) as f32, vaddvq_u16(levels) as f32);
                estimates.push(self.base + self.scale * (self.low * ones + self.step * levels));
            }
        }
        estimates
    }
}

/// A row at half precision (DIM halves, as bytes) dotted with a query.
fn dot_half(row: &[u8], query: &[f32]) -> f32 {
    use std::arch::aarch64::*;
    debug_assert!(row.len() == DIM * 2 && query.len() == DIM);
    // Safety: DIM is a multiple of 16, and both have DIM values.
    unsafe {
        let halves = row.as_ptr() as *const u16;
        let query = query.as_ptr();
        let mut sums = [vdupq_n_f32(0.0); 4];
        for i in (0..DIM).step_by(16) {
            let low = vreinterpretq_f16_u16(vld1q_u16(halves.add(i)));
            let high = vreinterpretq_f16_u16(vld1q_u16(halves.add(i + 8)));
            sums[0] = vfmaq_f32(sums[0], vcvt_f32_f16(vget_low_f16(low)), vld1q_f32(query.add(i)));
            sums[1] = vfmaq_f32(sums[1], vcvt_high_f32_f16(low), vld1q_f32(query.add(i + 4)));
            sums[2] = vfmaq_f32(sums[2], vcvt_f32_f16(vget_low_f16(high)), vld1q_f32(query.add(i + 8)));
            sums[3] = vfmaq_f32(sums[3], vcvt_high_f32_f16(high), vld1q_f32(query.add(i + 12)));
        }
        vaddvq_f32(vaddq_f32(vaddq_f32(sums[0], sums[1]), vaddq_f32(sums[2], sums[3])))
    }
}

/// The current generation's rows of the partition called `name`, in `form`.
#[cfg(test)]
pub fn read_rows(conn: &Connection, folder: &Path, name: &str, form: Form) -> Result<Vec<u8>> {
    use rusqlite::OptionalExtension;
    let partition: Option<Partition> = conn
        .query_row(&format!("SELECT {COLUMNS} FROM partitions WHERE name = ?1"), [name], Partition::from_row)
        .optional()?;
    let Some(partition) = partition else { return Ok(Vec::new()) };
    let map = Map::open(&partition.path(folder, form), partition.rows as usize * form.row_bytes())?;
    Ok(map.all().to_vec())
}

/// A unit vector made from `seed`, for tests.
#[cfg(test)]
pub fn unit(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mut vector: Vec<f32> = (0..DIM)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        })
        .collect();
    crate::embedder::normalize(&mut vector);
    vector
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_precision_scores_nearly_exactly() {
        let rows: Vec<Vec<f32>> = (0..40).map(unit).collect();
        let query = unit(99);
        let mut f16 = Vec::new();
        for row in &rows {
            encode(Form::F16, row, &[], &mut f16);
        }
        for (row, bytes) in rows.iter().zip(f16.chunks_exact(Form::F16.row_bytes())) {
            let exact: f32 = row.iter().zip(&query).map(|(a, b)| a * b).sum();
            assert!((dot_half(bytes, &query) - exact).abs() < 1e-3);
        }
    }

    #[test]
    fn sketches_split_at_the_mean() {
        let mean = vec![0.0; DIM];
        let mut row = vec![-0.1; DIM];
        row[0] = 0.1;
        row[9] = 0.2;
        let bits = sketch(&row, &mean);
        assert_eq!((bits[0], bits[1], bits[2]), (0b1, 0b10, 0));
    }

    #[test]
    fn sketches_estimate_the_cosine() {
        // Rows around a common direction, as a model's are: a mean that isn't zero, and spreads that differ.
        let base = unit(1000);
        let rows: Vec<Vec<f32>> = (0..500)
            .map(|seed| {
                let mut row: Vec<f32> = unit(seed)
                    .iter()
                    .zip(&base)
                    .enumerate()
                    .map(|(d, (x, b))| x * (1 + d % 3) as f32 + 2.0 * b)
                    .collect();
                crate::embedder::normalize(&mut row);
                row
            })
            .collect();
        let mut sums = Sums::default();
        rows.iter().for_each(|row| sums.add(row));
        let (mean, spread) = (sums.mean(rows.len() as i64), sums.spread(rows.len() as i64));
        let sketches: Vec<u8> = rows.iter().flat_map(|row| sketch(row, &mean)).collect();
        let query = &rows[7];
        let estimates = SketchQuery::new(query, &mean, &spread).estimate_all(&sketches);
        let exact: Vec<f32> = rows.iter().map(|row| dot(row, query)).collect();
        // The row itself comes out best, and the estimates follow the cosines.
        let best = (0..rows.len()).max_by(|&a, &b| estimates[a].total_cmp(&estimates[b])).unwrap();
        assert_eq!(best, 7);
        let others = (0..rows.len()).filter(|&row| row != 7);
        let error = others.clone().map(|row| (estimates[row] - exact[row]).abs()).sum::<f32>() / 499.0;
        let spread_of_exact = {
            let centre = others.clone().map(|row| exact[row]).sum::<f32>() / 499.0;
            others.map(|row| (exact[row] - centre).abs()).sum::<f32>() / 499.0
        };
        assert!(error < spread_of_exact, "{error} {spread_of_exact}");
    }

    #[test]
    fn drift_counts_the_bits_that_would_flip() {
        let mut sums = Sums::default();
        let rows: Vec<Vec<f32>> = (0..2000).map(unit).collect();
        rows.iter().for_each(|row| sums.add(row));
        let mean = sums.mean(rows.len() as i64);
        assert!(sums.drift(rows.len() as i64, &mean) < 1e-6);
        // Every threshold a tenth of a spread off: about 4% of the bits.
        let spread = (1.0 / 12.0f32).sqrt() / (DIM as f32 / 12.0).sqrt();
        let moved: Vec<f32> = mean.iter().map(|m| m + 0.1 * spread).collect();
        let drift = sums.drift(rows.len() as i64, &moved);
        assert!((0.03..0.05).contains(&drift), "{drift}");
    }
}
