//! Resources: what the Mac has, and what indexing and searching may take of it. The Mac's facts go in (its memory,
//! read once, its cores, its power, the memory pressure macOS reports), and plans come out. A plan is a pure function of
//! the facts and the config, tested without a Mac of each size.
//!
//! - Plan: which rounds of indexing run side by side, each with its tools loaded, and how many files are read at once,
//!   by `indexing.memory_mb`, `indexing.on_battery` and the memory pressure. None of it changes what's written.
//! - SearchPlan: which form of the vectors a search scans, by `search.vectors` and `search.memory_mb`, and how many of
//!   them a search with sketches scores exactly.

use crate::adapters::macos::{self, power};
use crate::config::{self, Budget, OnBattery};
use crate::index::Form;
pub use macos::pressure::Pressure;
use protocol::{Round, Scan};

/// The share of the Mac's memory indexing may take with `indexing.memory_mb` at auto: a ceiling, which memory pressure
/// lowers (Plan).
const INGEST_SHARE: f64 = 0.25;

/// The share of the Mac's memory searches may read with `search.memory_mb` at auto.
const SEARCH_SHARE: f64 = 0.1;

/// What a reader holds, with what it has decoded.
const READER: u64 = 128 << 20;

/// OCR and speech share the Neural Engine well: side by side, they get more done than one after the other. Measured
/// (docs/design.md, "Tools and the budget"): 362 screenshots and six hours of podcasts took 242 s side by side and
/// 286 s one after the other on the Neural Engine alone, and 347 s against 385 s beside the GPU's rounds.
const NEURAL_ENGINE_SHARED: bool = true;

/// A budget in bytes: as set, or else `share` of the Mac's memory.
fn bytes(budget: Budget, share: f64) -> u64 {
    match budget {
        Budget::Mb(mb) => mb << 20,
        Budget::Auto => (macos::memory() as f64 * share) as u64,
    }
}

/// Where a tool runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Cpu,
    Gpu,
    NeuralEngine,
}

/// What a round loads to do its work. Each says its chip and what it holds at its peak, as measured (docs/design.md):
/// memory of its own (its footprint), the model file it maps and reads on every call, and what it holds on the Neural
/// Engine, which macOS counts to the process but leaves out of its footprint (`footprint` lists it apart, as "owned
/// physical footprint (neural)", and proc_pid_rusage as ri_neural_footprint). A tool that isn't built yet slots in by
/// saying the same of itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// The text model: 338 MB, and its 296 MB file mapped.
    TextModel,
    /// The vision encoder, beside the text model: 289 MB (`sift-it bench-embed`, the file in memory).
    VisionEncoder,
    /// The audio encoder, beside the text model: 321 MB.
    AudioEncoder,
    /// Vision's text recognition: 256 MB of its own and 171 MB on the Neural Engine (the indexer reading words held
    /// 86 MB, reading the text on 362 screenshots 340 MB).
    TextRecognition,
    /// The speech helper: 340 MB, 607 MB on the Neural Engine (its copy of the model), and the 32 MB it maps of what
    /// Core ML compiled for the Neural Engine. It holds about 110 MB writing down long recordings three minutes at a
    /// time; a run of short ones, each a chunk of a length of its own (the demo's videos, cheapest first), leaves
    /// FluidAudio holding a buffer of each for a while: 245 to 336 MB in all.
    SpeechModel,
}

impl Tool {
    pub fn chip(self) -> Chip {
        match self {
            Tool::TextModel | Tool::VisionEncoder | Tool::AudioEncoder => Chip::Gpu,
            Tool::TextRecognition | Tool::SpeechModel => Chip::NeuralEngine,
        }
    }

    /// What it holds at its peak, in bytes.
    pub fn memory(self) -> u64 {
        (match self {
            Tool::TextModel => 338 + 296,
            Tool::VisionEncoder => 289,
            Tool::AudioEncoder => 321,
            Tool::TextRecognition => 256 + 171,
            Tool::SpeechModel => 340 + 607 + 32,
        }) << 20
    }

    /// The tools a round loads.
    pub fn of(round: Round) -> &'static [Tool] {
        match round {
            Round::Words => &[],
            Round::Text => &[Tool::TextModel],
            Round::Ocr => &[Tool::TextRecognition],
            Round::Pictures => &[Tool::TextModel, Tool::VisionEncoder],
            Round::Sound => &[Tool::TextModel, Tool::AudioEncoder],
            Round::Speech => &[Tool::SpeechModel],
        }
    }
}

/// The chip a round's work is on: its tools', or the CPU's for words.
fn chip(round: Round) -> Chip {
    Tool::of(round).last().map_or(Chip::Cpu, |tool| tool.chip())
}

/// Whether two rounds can run at once. On different chips they always can. On the GPU, the text round's batches go
/// between the items of pictures or sound, since they share the text model; but pictures and sound take turns, since
/// two rounds sharing one chip's time finish neither sooner, and the earlier one should. On the Neural Engine, OCR and
/// speech run together (NEURAL_ENGINE_SHARED).
fn together(a: Round, b: Round) -> bool {
    match (chip(a), chip(b)) {
        (one, other) if one != other => true,
        (Chip::Gpu, _) => a == Round::Text || b == Round::Text,
        (Chip::NeuralEngine, _) => NEURAL_ENGINE_SHARED,
        (Chip::Cpu, _) => false,
    }
}

/// What the Mac runs on, for `indexing.on_battery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Charger,
    Battery,
    /// Low Power Mode, on battery or not.
    LowPowerMode,
}

/// The facts a plan follows as they change: power, and memory pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conditions {
    pub power: Power,
    pub pressure: Pressure,
}

/// The tools that aren't there now: a model not downloaded, a speech helper not installed.
pub fn missing() -> Vec<Tool> {
    let models = crate::models::status();
    let mut missing = Vec::new();
    if !models.text {
        missing.push(Tool::TextModel);
    }
    if !models.media {
        missing.extend([Tool::VisionEncoder, Tool::AudioEncoder]);
    }
    if crate::adapters::speech::unavailable().is_some() {
        missing.push(Tool::SpeechModel);
    }
    missing
}

/// What the Mac runs on now.
pub fn power() -> Power {
    if power::low_power_mode() {
        Power::LowPowerMode
    } else if power::on_battery() {
        Power::Battery
    } else {
        Power::Charger
    }
}

/// A round's work waiting, as a plan sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queue {
    pub round: Round,
    /// Some of it is new files' (the fast lane).
    pub new: bool,
    /// Some of it isn't.
    pub old: bool,
}

/// What a round's work waits for, when it can't run for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    /// Its turn: another round has its chip, or the budget has no room for its tool yet.
    Turn,
    /// A charger (`indexing.on_battery`, or Low Power Mode).
    Charger,
    /// Memory: macOS says it's critically short.
    Memory,
    /// Its model, which isn't there (not downloaded, or the speech helper can't start).
    Model,
}

/// A round in a plan: for all its work, or for new files' only (on battery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    pub round: Round,
    pub new_only: bool,
}

/// What runs side by side now, and what's loaded for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// The budget, in bytes.
    pub memory: u64,
    /// The rounds that run: new files' first, then those whose tools are loaded, then in order.
    pub runs: Vec<Running>,
    /// The rounds with work that waits, and for what.
    pub waits: Vec<(Round, Wait)>,
    /// The tools loaded: the runs', and the text model while there's room for it.
    pub tools: Vec<Tool>,
    /// Files read at once, by the words lane and the media lane together.
    pub readers: usize,
}

/// Whether a round reads files on a lane of its own (schedule.rs): words', and the GPU's pictures or sound. Each such
/// lane that runs has a reader at least.
fn reads(round: Round) -> bool {
    matches!(round, Round::Words | Round::Pictures | Round::Sound)
}

impl Plan {
    /// The plan for `queues` (the rounds with work), under `conditions`, with the tools in `missing` not there and
    /// those in `held` loaded now.
    pub fn of(
        indexing: &config::Indexing,
        conditions: Conditions,
        queues: &[Queue],
        missing: &[Tool],
        held: &[Tool],
    ) -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let budget = bytes(indexing.memory_mb, INGEST_SHARE);
        Self::within(budget, cores, indexing.on_battery, conditions, queues, missing, held)
    }

    /// The rounds with work are taken new files' first, then those whose tools are loaded (`held`), so a round works
    /// through its queue once its tools are loaded, then the rest in their order. Each joins when it can run beside
    /// those before it, and its tools fit in `budget` beside theirs: words always run, and the first that needs a tool
    /// too, whatever the budget, since indexing never stops for want of memory. The text model stays loaded while it
    /// fits beside them (the other rounds bring it passages), and goes when it doesn't. On a memory-pressure warning,
    /// only one tool is loaded at a time; when it's critical, nothing runs. On battery, `on_battery` says what goes on.
    /// Each lane that reads has a reader; more read at once with what's left of the budget, up to a third of the cores
    /// (two to four) in all.
    fn within(
        budget: u64,
        cores: usize,
        on_battery: OnBattery,
        conditions: Conditions,
        queues: &[Queue],
        missing: &[Tool],
        held: &[Tool],
    ) -> Self {
        let mut plan = Plan { memory: budget, ..Default::default() };
        let loaded = |round: Round| {
            let tools = Tool::of(round);
            !tools.is_empty() && tools.iter().all(|tool| held.contains(tool))
        };
        let mut queues: Vec<Queue> = queues.to_vec();
        queues.sort_by_key(|queue| (!queue.new, !loaded(queue.round), queue.round));
        // What power lets go on: all of a round's work, or only new files'.
        let all = |round: Round| match (conditions.power, on_battery) {
            (Power::Charger, _) | (Power::Battery, OnBattery::All) => true,
            (_, OnBattery::None) => false,
            _ => round == Round::Words,
        };
        let new_only = on_battery != OnBattery::None;
        let one_tool = conditions.pressure == Pressure::Warning;
        // The first reader is always counted: it reads for OCR and speech, too.
        let mut used = READER;
        let mut reading = 0;
        for queue in queues {
            let round = queue.round;
            let runs = all(round) || (queue.new && new_only);
            let wait = if conditions.pressure == Pressure::Critical {
                Some(Wait::Memory)
            } else if Tool::of(round).iter().any(|tool| missing.contains(tool)) {
                Some(Wait::Model)
            } else if !runs {
                Some(Wait::Charger)
            } else {
                None
            };
            if let Some(wait) = wait {
                plan.waits.push((round, wait));
                continue;
            }
            let extra: Vec<Tool> = Tool::of(round).iter().copied().filter(|tool| !plan.tools.contains(tool)).collect();
            let reader = if reads(round) && reading > 0 { READER } else { 0 };
            let more = extra.iter().map(|tool| tool.memory()).sum::<u64>() + reader;
            let beside = plan.runs.iter().all(|running| together(running.round, round));
            let fits = extra.is_empty() || plan.tools.is_empty() || (!one_tool && used + more <= budget);
            if !(beside && fits) {
                plan.waits.push((round, Wait::Turn));
                continue;
            }
            plan.runs.push(Running { round, new_only: !all(round) });
            plan.tools.extend(extra);
            used += more;
            reading += usize::from(reads(round));
            if queue.old && !all(round) {
                // The rest of it waits for a charger.
                plan.waits.push((round, Wait::Charger));
            }
        }
        let text = Tool::TextModel;
        if held.contains(&text) && !plan.tools.contains(&text) && !one_tool && used + text.memory() <= budget {
            plan.tools.push(text);
            used += text.memory();
        }
        let (least, most) = (reading.max(1), (cores / 3).clamp(2, 4));
        let more = if one_tool { 0 } else { (budget.saturating_sub(used) / READER) as usize };
        plan.readers = (least + more).min(most).max(least);
        plan
    }

    /// The readers of the words lane and of the media lane: all of them for the one that runs, and when both do, half
    /// each (the words lane's rounded up).
    pub fn lanes(&self) -> [usize; 2] {
        let words = self.runs(Round::Words).is_some();
        let media = self.runs.iter().any(|running| matches!(running.round, Round::Pictures | Round::Sound));
        match (words, media) {
            (true, true) => [self.readers - self.readers / 2, (self.readers / 2).max(1)],
            (true, false) => [self.readers, 0],
            (false, true) => [0, self.readers],
            (false, false) => [0, 0],
        }
    }

    /// Whether the plan lets this round's work of a file go on: the round runs, for all its work or new files'.
    pub fn lets(&self, round: Round, new: bool) -> bool {
        self.runs(round).is_some_and(|running| new || !running.new_only)
    }

    /// Whether `round` runs, for all of its work or new files'.
    pub fn runs(&self, round: Round) -> Option<Running> {
        self.runs.iter().copied().find(|running| running.round == round)
    }

    /// What the work left waits for, when nothing runs: the first round's reason.
    pub fn waits_for(&self) -> Option<Wait> {
        self.runs.is_empty().then(|| self.waiting()).flatten()
    }

    /// What some of the work waits for, besides its turn: the first round's reason.
    pub fn waiting(&self) -> Option<Wait> {
        self.waits.iter().map(|(_, wait)| *wait).find(|wait| *wait != Wait::Turn)
    }
}

impl Wait {
    /// What it waits for, in words, under these conditions.
    pub fn describe(self, power: Power) -> &'static str {
        match (self, power) {
            (Wait::Charger, Power::LowPowerMode) => "Waiting: Low Power Mode is on",
            (Wait::Charger, _) => "Waiting for a charger",
            (Wait::Memory, _) => "Waiting: the Mac is short of memory",
            (Wait::Model, _) => "Waiting for a model to download",
            (Wait::Turn, _) => "Waiting for its turn",
        }
    }
}

/// How many segments a search with sketches scores exactly, at least: those whose sketches come closest to the query.
/// Measured on Rust's books (7,400 vectors, all on one subject, so many come close): the closest 1,024 keep all of the
/// exact top 20 for 45 queries, the closest 256 keep 99.3%. Four times that is the margin for collections that differ.
const CANDIDATES: usize = 4096;

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

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;
    const CALM: Conditions = Conditions { power: Power::Charger, pressure: Pressure::Normal };

    fn queues(rounds: &[Round]) -> Vec<Queue> {
        rounds.iter().map(|&round| Queue { round, new: false, old: true }).collect()
    }

    /// The plan for these rounds with work at `mb` MB on ten cores, with the tools in `held` loaded.
    fn with(mb: u64, conditions: Conditions, queues: &[Queue], held: &[Tool]) -> Plan {
        Plan::within(mb * MB, 10, OnBattery::Words, conditions, queues, &[], held)
    }

    /// What runs, and the readers, for these rounds with work at `mb` MB on ten cores, with nothing loaded yet.
    fn plan(mb: u64, conditions: Conditions, queues: &[Queue]) -> (Vec<Round>, usize) {
        let plan = with(mb, conditions, queues, &[]);
        (rounds(&plan), plan.readers)
    }

    fn rounds(plan: &Plan) -> Vec<Round> {
        plan.runs.iter().map(|running| running.round).collect()
    }

    use Round::*;
    const EVERYTHING: [Round; 6] = Round::ALL;

    #[test]
    fn the_budget_decides_what_runs_side_by_side() {
        // A small budget: one tool at a time (words needs none), the rest waits its turn.
        assert_eq!(plan(768, CALM, &queues(&EVERYTHING)), (vec![Words, Text], 1));
        assert_eq!(plan(1024, CALM, &queues(&[Pictures, Sound, Speech])), (vec![Pictures], 1));
        // More: the GPU with one encoder, and OCR or speech beside it on the Neural Engine.
        assert_eq!(plan(1536, CALM, &queues(&[Text, Ocr, Pictures, Sound, Speech])), (vec![Text, Ocr, Pictures], 1));
        assert_eq!(plan(2048, CALM, &queues(&[Text, Pictures, Sound, Speech])), (vec![Text, Pictures, Speech], 1));
        // 4 GB: every chip at work. The GPU still takes pictures and sound in turn.
        assert_eq!(plan(4096, CALM, &queues(&EVERYTHING)), (vec![Words, Text, Ocr, Pictures, Speech], 3));
        assert_eq!(plan(4096, CALM, &queues(&[Sound, Speech])), (vec![Sound, Speech], 3));
        // Whatever the budget, the first tool loads.
        assert_eq!(plan(0, CALM, &queues(&[Speech])), (vec![Speech], 1));
    }

    #[test]
    fn new_files_go_first_and_take_their_chip() {
        let mut work = queues(&[Words, Text, Pictures, Sound, Speech]);
        work[3].new = true; // a new recording's sound
        let (runs, _) = plan(2560, CALM, &work);
        assert_eq!(runs, [Sound, Words, Text, Speech]);
    }

    #[test]
    fn memory_pressure_shrinks_the_plan_or_stops_it() {
        let warning = Conditions { pressure: Pressure::Warning, ..CALM };
        assert_eq!(plan(8192, warning, &queues(&EVERYTHING)), (vec![Words, Text], 1));
        // A round that needs no more than what's loaded goes along: text, with a new file's pictures.
        let mut work = queues(&[Pictures, Text, Speech]);
        work[0].new = true;
        assert_eq!(plan(8192, warning, &work), (vec![Pictures, Text], 1));
        let critical = Conditions { pressure: Pressure::Critical, ..CALM };
        let stopped = Plan::within(8192 * MB, 10, OnBattery::All, critical, &queues(&EVERYTHING), &[], &[]);
        assert!(stopped.runs.is_empty());
        assert_eq!(stopped.waits_for(), Some(Wait::Memory));
    }

    #[test]
    fn on_battery_words_and_new_files_go_on() {
        let battery = Conditions { power: Power::Battery, ..CALM };
        let mut work = queues(&[Words, Text, Pictures, Speech]);
        work[2].new = true;
        let words = Plan::within(4096 * MB, 10, OnBattery::Words, battery, &work, &[], &[]);
        let runs: Vec<Running> = words.runs.clone();
        assert_eq!(runs, [Running { round: Pictures, new_only: true }, Running { round: Words, new_only: false }]);
        assert!(words.waits.contains(&(Text, Wait::Charger)) && words.waits.contains(&(Pictures, Wait::Charger)));
        let all = Plan::within(4096 * MB, 10, OnBattery::All, battery, &work, &[], &[]);
        assert_eq!(all.runs.len(), 4);
        let none = Plan::within(4096 * MB, 10, OnBattery::None, battery, &work, &[], &[]);
        assert!(none.runs.is_empty());
        assert_eq!(none.waits_for(), Some(Wait::Charger));
        // Low Power Mode is `words` at most.
        let low = Conditions { power: Power::LowPowerMode, ..CALM };
        let low = Plan::within(4096 * MB, 10, OnBattery::All, low, &work, &[], &[]);
        assert_eq!(rounds(&low), [Pictures, Words]);
        // What it lets go on: words, and the new file's pictures.
        assert!(words.lets(Words, false) && words.lets(Pictures, true));
        assert!(!words.lets(Pictures, false) && !words.lets(Speech, true));
    }

    #[test]
    fn a_loaded_round_works_through_its_queue() {
        use Tool::*;
        // At 1 GB speech and the text model don't fit side by side. Before either is loaded, text goes first, as the
        // rounds go; once speech is loaded, it goes on through its queue, and the passages it brings wait their turn.
        let work = queues(&[Text, Speech]);
        assert_eq!(rounds(&with(1024, CALM, &work, &[])), [Text]);
        let speaking = with(1024, CALM, &work, &[SpeechModel]);
        assert_eq!(
            (rounds(&speaking), speaking.waits, speaking.tools),
            (vec![Speech], vec![(Text, Wait::Turn)], vec![SpeechModel])
        );
        assert_eq!(rounds(&with(1024, CALM, &queues(&[Text]), &[SpeechModel])), [Text]);
        // At 2 GB (an 8 GB Mac at auto), OCR and speech share the Neural Engine, and the text model doesn't fit beside
        // them: it waits for one of them to be done.
        let both = with(2048, CALM, &queues(&[Text, Ocr, Speech]), &[TextRecognition, SpeechModel]);
        assert_eq!((rounds(&both), both.tools), (vec![Ocr, Speech], vec![TextRecognition, SpeechModel]));
        assert_eq!(rounds(&with(2048, CALM, &queues(&[Text, Speech]), &[SpeechModel])), [Speech, Text]);
        // On the GPU, sound keeps it until its queue is done, and pictures wait.
        let sound = with(4096, CALM, &queues(&[Pictures, Sound]), &[TextModel, AudioEncoder]);
        assert_eq!(rounds(&sound), [Sound]);
        // A new file's work goes first all the same.
        let mut work = queues(&[Text, Speech]);
        work[0].new = true;
        assert_eq!(rounds(&with(1024, CALM, &work, &[SpeechModel])), [Text]);
    }

    #[test]
    fn the_text_model_counts_while_it_is_loaded() {
        use Tool::*;
        // It stays while it fits beside what runs (the rounds bring it passages)...
        let roomy = with(4096, CALM, &queues(&[Speech]), &[TextModel, SpeechModel]);
        assert_eq!(roomy.tools, [SpeechModel, TextModel]);
        // ...and goes when it doesn't, or on a memory-pressure warning.
        assert_eq!(with(1200, CALM, &queues(&[Speech]), &[TextModel, SpeechModel]).tools, [SpeechModel]);
        let warning = Conditions { pressure: Pressure::Warning, ..CALM };
        assert_eq!(with(8192, warning, &queues(&[Speech]), &[TextModel, SpeechModel]).tools, [SpeechModel]);
        // Nothing else that's loaded stays without its round.
        assert_eq!(with(8192, CALM, &queues(&[Text]), &[TextModel, AudioEncoder, SpeechModel]).tools, [TextModel]);
    }

    #[test]
    fn each_lane_that_reads_counts_its_readers() {
        // Words and pictures read on lanes of their own: the second lane's reader counts in the budget.
        let tight = with(1100, CALM, &queues(&[Words, Text, Pictures]), &[]);
        assert_eq!((rounds(&tight), tight.lanes()), (vec![Words, Text], [3, 0]));
        let roomy = with(4096, CALM, &queues(&[Words, Text, Pictures]), &[]);
        assert_eq!((rounds(&roomy), roomy.readers, roomy.lanes()), (vec![Words, Text, Pictures], 3, [2, 1]));
        // Whatever the budget, each has one.
        let least = with(0, CALM, &queues(&[Words, Pictures]), &[]);
        assert_eq!((rounds(&least), least.lanes()), (vec![Words, Pictures], [1, 1]));
    }

    #[test]
    fn a_round_without_its_model_waits_for_it() {
        let missing = [Tool::SpeechModel];
        let plan = Plan::within(4096 * MB, 10, OnBattery::Words, CALM, &queues(&[Words, Speech]), &missing, &[]);
        assert_eq!(plan.runs, [Running { round: Words, new_only: false }]);
        assert_eq!(plan.waits, [(Speech, Wait::Model)]);
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
