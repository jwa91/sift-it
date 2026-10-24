//! Resources: what the Mac has, and what indexing and searching may take of it. The Mac's facts go in (its memory,
//! read once, its cores, whether it's on battery), and plans come out. A plan is a pure function of the facts and the
//! config, tested without a Mac of each size.
//!
//! - IngestPlan: how many files are read at once, and whether what's said in recordings is written down beside the
//!   reading or after it, by `indexing.memory_mb`. Neither changes what's written.
//! - SearchPlan: which form of the vectors a search scans, by `search.vectors` and `search.memory_mb`, and how many of
//!   them a search with sketches scores exactly.
//! - `wait`: whether indexing waits for a charger.

use crate::adapters::macos::{self, power};
use crate::config::{self, Budget};
use crate::index::Form;
use protocol::Scan;

/// The share of the Mac's memory indexing takes with `indexing.memory_mb` at auto. From 9 GB of memory on, the models
/// run side by side; with less, they take turns.
const INGEST_SHARE: f64 = 0.25;

/// The share of the Mac's memory searches may read with `search.memory_mb` at auto.
const SEARCH_SHARE: f64 = 0.1;

/// What the parts of indexing hold at their peak, as measured (docs/design.md): memory of their own (their footprint),
/// and the model file they map and read on every call. The embedding model with both its encoders: 940 MB,
/// and the text model's 296 MB. The speech helper with a three-minute stretch of sound: 120 MB, the Neural Engine's
/// copy of its model, 607 MB, and the 32 MB it maps of what Core ML compiled for the Neural Engine. The copy is the
/// helper's, but its footprint leaves it out: `footprint` lists it apart ("owned physical footprint (neural)"), and
/// proc_pid_rusage counts it as ri_neural_footprint. A reader, with what it has decoded.
const MODEL: u64 = (940 + 296) << 20;
const SPEECH: u64 = (120 + 607 + 32) << 20;
const READER: u64 = 128 << 20;

/// How many segments a search with sketches scores exactly, at least: those whose sketches come closest to the query.
/// Measured on Rust's books (7,400 vectors, all on one subject, so many come close): the closest 1,024 keep all of the
/// exact top 20 for 45 queries, the closest 256 keep 99.3%. Four times that is the margin for collections that differ.
const CANDIDATES: usize = 4096;

/// A budget in bytes: as set, or else `share` of the Mac's memory.
fn bytes(budget: Budget, share: f64) -> u64 {
    match budget {
        Budget::Mb(mb) => mb << 20,
        Budget::Auto => (macos::memory() as f64 * share) as u64,
    }
}

/// What `indexing.memory_mb` affords.
#[derive(Debug, PartialEq)]
pub struct IngestPlan {
    /// The budget, in bytes.
    pub memory: u64,
    /// Files read at once.
    pub readers: usize,
    /// What's said in recordings is written down beside the reading.
    pub beside: bool,
}

impl IngestPlan {
    pub fn of(indexing: &config::Indexing) -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self::within(bytes(indexing.memory_mb, INGEST_SHARE), cores)
    }

    /// The plan for `budget` bytes on a Mac with `cores` cores. Reading takes at most a third of them (two to four),
    /// so the Mac stays quick; the budget can make that fewer, down to one. Speech is written down beside the reading
    /// when both models fit with two readers.
    fn within(budget: u64, cores: usize) -> Self {
        let beside = budget >= MODEL + SPEECH + 2 * READER;
        let left = budget.saturating_sub(MODEL + if beside { SPEECH } else { 0 });
        Self { memory: budget, readers: ((left / READER) as usize).clamp(1, (cores / 3).clamp(2, 4)), beside }
    }
}

/// What a search scans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchPlan {
    /// The form of the vectors.
    pub form: Form,
    /// With sketches, how many vectors are scored exactly: those whose sketches come closest to the query.
    pub candidates: usize,
}

impl SearchPlan {
    /// The plan for vector files of `rows` rows (as a scan reads them: live and dead), as `search` says.
    pub fn of(search: &config::Search, rows: usize) -> Self {
        Self::within(search.vectors, bytes(search.memory_mb, SEARCH_SHARE), rows)
    }

    /// The form `scan` names, or with `auto` the most precise whose rows fit in `budget` bytes. With sketches,
    /// CANDIDATES are scored exactly, or in a larger index as many rows as reading them takes the bytes the sketches
    /// do (a 32nd): the share it takes to keep the exact top 20 falls as an index grows (its best matches stand
    /// further above the rest), so a fixed share keeps up.
    fn within(scan: Scan, budget: u64, rows: usize) -> Self {
        let form = match scan {
            Scan::ExactF32 => Form::F32,
            Scan::ExactF16 => Form::F16,
            Scan::Sketch => Form::Sketch,
            Scan::Auto => {
                let fits = |form: &Form| rows as u64 * form.row_bytes() as u64 <= budget;
                [Form::F32, Form::F16].into_iter().find(fits).unwrap_or(Form::Sketch)
            }
        };
        Self { form, candidates: CANDIDATES.max(rows * Form::Sketch.row_bytes() / Form::F32.row_bytes()) }
    }

    /// The form, as `search.vectors` names it.
    pub fn scan(self) -> Scan {
        match self.form {
            Form::F32 => Scan::ExactF32,
            Form::F16 => Scan::ExactF16,
            Form::Sketch => Scan::Sketch,
        }
    }
}

/// Why indexing should wait now, if it should: in Low Power Mode it always does, and on battery when
/// `indexing.on_battery` is off.
pub fn wait(indexing: &config::Indexing) -> Option<String> {
    if power::low_power_mode() {
        return Some("Waiting: Low Power Mode is on".into());
    }
    if !indexing.on_battery && power::on_battery() {
        return Some("Waiting for a charger".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_decides_the_readers_and_whether_speech_goes_beside_the_reading() {
        let gb = 1u64 << 30;
        let plan = |budget, cores| {
            let plan = IngestPlan::within(budget, cores);
            (plan.readers, plan.beside)
        };
        // 2 GB: the models take turns, and the cores decide the readers.
        assert_eq!(plan(2 * gb, 8), (2, false));
        // 6 GB on ten cores: both models, three readers.
        assert_eq!(plan(6 * gb, 10), (3, true));
        // Little memory: one reader at a time.
        assert_eq!(plan(gb + READER, 8), (1, false));
        assert_eq!(plan(0, 8), (1, false));
    }

    #[test]
    fn auto_scans_the_most_precise_form_that_fits() {
        // 400 rows: 1.2 MB at f32, 0.6 MB at f16.
        let scan = |mb: u64| SearchPlan::within(Scan::Auto, mb << 20, 400).scan();
        assert_eq!([scan(2), scan(1), scan(0)], [Scan::ExactF32, Scan::ExactF16, Scan::Sketch]);
        assert_eq!(SearchPlan::within(Scan::ExactF16, 0, 400).form, Form::F16);
        // A larger index scores a 32nd of its rows exactly.
        assert_eq!(SearchPlan::within(Scan::Sketch, 0, 400).candidates, CANDIDATES);
        assert_eq!(SearchPlan::within(Scan::Sketch, 0, 1 << 20).candidates, 1 << 15);
    }
}
