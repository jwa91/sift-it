//! What sift-it's clients and its engine say to each other: the requests, their replies and the events, and the types
//! they're made of. docs/protocol.md describes the wire for clients in other languages.
//!
//! JSON lines, over the engine's Unix socket in the data folder, or over `sift-it serve`'s stdin and stdout. A request
//! is `{"id": …, "op": …, …}`, and its reply carries the same `id`: the op's reply, or a Failure. Events have no `id`.
//! The first line a client gets is the `ready` event with the engine's build. A client of another build stops that
//! engine and starts its own rather than talking to it: there's no negotiating, a mismatch is only noticed.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A request: its id (any JSON value, echoed in the reply) and what it asks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    #[serde(flatten)]
    pub op: Op,
}

/// What a request asks, and (after the arrow) what it's answered with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Files by what's in them → Results. An empty `q` lists recent files instead.
    Search(Search),
    /// Recently changed files, of the kinds asked for → Results.
    Recent(Search),
    /// A segment's whole text, or a file's (its text segments in order) → Text.
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_id: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// What the engine holds, and the indexer's progress → Status.
    Status,
    /// The settings, or the one at a dotted key (`search.mode`), set first when there's a value: what
    /// `sift-it config <key> <value>` does → Settings.
    Config {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// The folders indexed and the ones left out, changed first when there's an action → Folders.
    Folders {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<FolderAction>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// Lets the model and the vectors go, until a search by meaning needs them → Done.
    Unload,
    /// Stops the engine → Done.
    Stop,
    /// A preview's picture → Picture. `sift-it serve` makes it: drawing a page or a frame is reading code, which stays
    /// out of the engine.
    Preview(Preview),
    /// Ends `sift-it serve` once the requests before it are answered → Done. The engine goes on.
    Quit,
}

/// A search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Search {
    #[serde(default)]
    pub q: String,
    /// By words or by meaning (config.json's `search.mode` when not said).
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Only these kinds of file (all but code when empty).
    #[serde(default)]
    pub kinds: Vec<Kind>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Match the last word as a prefix (the user is still typing it).
    #[serde(default)]
    pub prefix_last: bool,
    /// By meaning even while the model loads: the reply waits for it (else it's by words meanwhile).
    #[serde(default)]
    pub wait: bool,
}

fn default_limit() -> usize {
    40
}

/// What to do with folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderAction {
    /// Index these folders.
    Add,
    /// Stop indexing these folders, and forget what was left out of them.
    Remove,
    /// Index these folders inside an indexed one again.
    Include,
    /// Leave these folders inside an indexed one out.
    Exclude,
}

/// A preview to draw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub path: String,
    #[serde(default)]
    pub kind: Drawn,
    #[serde(default)]
    pub loc: Option<f64>,
    #[serde(default)]
    pub loc_end: Option<f64>,
    /// The longest side, in pixels.
    #[serde(default = "default_size")]
    pub size: u32,
}

/// What a preview draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Drawn {
    #[default]
    Image,
    /// A page, `loc`.
    Pdf,
    /// The frame at `loc` seconds.
    Video,
    /// A recording's, with the stretch from `loc` to `loc_end` seconds highlighted.
    Waveform,
}

fn default_size() -> u32 {
    640
}

/// A search's results, best first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    /// Meaning was part of this search (false: words only).
    pub meaning: bool,
    pub ms: f64,
}

/// A file found, and where in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub kind: Kind,
    /// The segment that matched best: where the result opens.
    pub segment: Segment,
    pub segment_id: i64,
    /// Where in the file: a line, a page or a second, by the segment.
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
    /// What the reader found out about the file: pages, duration, size in pixels, tags.
    pub info: Value,
}

/// A segment's text, or a file's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: Option<String>,
}

/// What the engine holds, and how far the indexer is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub meaning: MeaningState,
    /// Why search by meaning is unavailable, when it is.
    pub error: Option<String>,
    /// How many vectors are mapped.
    pub vectors: usize,
    /// The form searches scan (what `auto` picked).
    pub scan: Scan,
    /// How much of the vectors is in memory, in bytes.
    pub resident: usize,
    pub stats: Stats,
    /// The background indexer's progress, as it last said it (None when it never ran for this data folder).
    pub indexer: Option<IndexerStatus>,
}

/// The settings (config.json), or the value at the key asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub config: Value,
}

/// The folders indexed, and the folders inside them left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Folders {
    pub folders: Vec<String>,
    pub excluded: Vec<String>,
}

/// A preview, drawn: the picture's file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Picture {
    pub image: String,
}

/// It's done.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub ok: bool,
}

impl Done {
    pub const OK: Done = Done { ok: true };
}

/// A request that failed: `ok` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub ok: bool,
    pub error: String,
}

impl Failure {
    pub fn new(error: impl Into<String>) -> Self {
        Self { ok: false, error: error.into() }
    }
}

/// What the engine says unasked, to every client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first line on connecting: the engine's build, and the state of search by meaning.
    Ready { version: String, build: String, index: String, meaning: MeaningState },
    /// Search by meaning loads, is ready, can't be had, or was let go.
    Meaning {
        state: MeaningState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// config.json can't be read: the settings stay as they were.
    Config { error: String },
    /// The background indexer's progress changed.
    Indexer(IndexerStatus),
}

/// Where search by meaning is: it loads on the first search that needs it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeaningState {
    #[default]
    Unloaded,
    Loading,
    Ready,
    /// The model isn't downloaded, or couldn't load: searches are by words.
    Unavailable,
}

/// The background indexer's progress: what it writes to status.json, and the engine passes on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexerStatus {
    pub pid: u32,
    pub state: IndexerState,
    /// What the work that's left waits for: a charger, memory, a model to download.
    pub detail: Option<String>,
    /// How far each round is, in their order: the rounds this index has had anything to do in.
    pub rounds: Vec<RoundStatus>,
    /// The file it did last.
    pub current: Option<String>,
    /// When it said this, in seconds since 1970.
    pub updated: f64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexerState {
    #[default]
    Starting,
    /// Comparing the folders with the index.
    Syncing,
    /// At work: its rounds say how far.
    Indexing,
    /// The work that's left waits (`detail` says for what), and nothing else can be done meanwhile.
    Waiting,
    Paused,
    /// Everything is done.
    Idle,
    Stopped,
}

/// A round of indexing: a pass over the whole collection with one tool (docs/design.md, "Rounds"). They're in their
/// order: words first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Round {
    /// Names, metadata and the text that's simply in files: no model.
    Words,
    /// What passages mean: the text model.
    Text,
    /// The text on screenshots and scanned pages: Vision's text recognition.
    Ocr,
    /// What photos, screenshots, scanned pages and video frames show: the text model with its vision encoder.
    Pictures,
    /// What recordings and a video's sound sound like: the text model with its audio encoder.
    Sound,
    /// What's said in recordings and videos: the speech model.
    Speech,
}

impl Round {
    pub const ALL: [Round; 6] = [Round::Words, Round::Text, Round::Ocr, Round::Pictures, Round::Sound, Round::Speech];

    pub fn as_str(self) -> &'static str {
        match self {
            Round::Words => "words",
            Round::Text => "text",
            Round::Ocr => "ocr",
            Round::Pictures => "pictures",
            Round::Sound => "sound",
            Round::Speech => "speech",
        }
    }

    pub fn parse(name: &str) -> Option<Round> {
        Round::ALL.into_iter().find(|round| round.as_str() == name)
    }

    /// What a round's `done` and `total` count: files, passages, pictures (a video's frames looked at, one every
    /// 2 s, among them), or seconds of sound.
    pub fn unit(self) -> &'static str {
        match self {
            Round::Words => "files",
            Round::Text => "passages",
            Round::Ocr | Round::Pictures => "pictures",
            Round::Sound | Round::Speech => "seconds",
        }
    }
}

/// How far a round is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundStatus {
    pub round: Round,
    pub state: RoundState,
    /// What's done of what's known so far, in the round's unit (Round::unit). The total grows while the rounds
    /// before find more: the text round's passages, as words, OCR and speech read them.
    pub done: f64,
    pub total: f64,
    /// Files with something left to do in it.
    pub files: usize,
    /// About how long the rest takes, at the speed it went while it ran: None until it has run a while.
    pub seconds_left: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundState {
    /// Nothing is left to do.
    #[default]
    Done,
    /// Its tool is loaded, and it's at work.
    Running,
    /// It waits for its turn: another round has its chip, or the budget has no room for its tool yet.
    Queued,
    /// It waits for a charger (`indexing.on_battery`), for memory, or for its model (the indexer's `detail` says).
    Waiting,
}

/// What's in the index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub files: i64,
    pub done: i64,
    pub pending: i64,
    pub failed: i64,
    /// Files only in iCloud whose work waits for their download: found by their names, or by what was read of them
    /// before macOS removed their download.
    pub cloud: i64,
    pub segments: i64,
    pub vectors: i64,
    pub by_kind: HashMap<String, i64>,
    /// The vectors of each kind's files.
    pub vectors_by_kind: HashMap<String, i64>,
    /// The index's size on disk.
    pub bytes: u64,
}

/// What a file is, for a search to filter by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Plain text and Markdown.
    Plaintext,
    /// PDFs and rich documents: rtf, doc, docx, odt, html, webarchive.
    Document,
    /// Records: csv, tsv, json, jsonl, yaml, toml.
    Structured,
    Image,
    /// A picture of a screen: read for its text as well as what it shows.
    Screenshot,
    Audio,
    Video,
    /// Source code (only with `sources.code`).
    Code,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::Plaintext,
        Kind::Document,
        Kind::Structured,
        Kind::Image,
        Kind::Screenshot,
        Kind::Audio,
        Kind::Video,
        Kind::Code,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Plaintext => "plaintext",
            Kind::Document => "document",
            Kind::Structured => "structured",
            Kind::Image => "image",
            Kind::Screenshot => "screenshot",
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Code => "code",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// What a segment is, which also says which of the model's inputs made its vector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Segment {
    /// A stretch of a text file or document.
    #[default]
    Passage,
    /// A PDF page's text (or, for a scanned page, the text read from it).
    Page,
    /// A picture, as a picture.
    Picture,
    /// The text in a picture or a scanned page (Apple's text recognition).
    Ocr,
    /// 30 s of sound.
    Sound,
    /// A video frame.
    Frame,
    /// What's said in a recording or video, written down: a sentence or a few, with their time.
    Speech,
}

impl Segment {
    pub const ALL: [Segment; 7] = [
        Segment::Passage,
        Segment::Page,
        Segment::Picture,
        Segment::Ocr,
        Segment::Sound,
        Segment::Frame,
        Segment::Speech,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Segment::Passage => "passage",
            Segment::Page => "page",
            Segment::Picture => "picture",
            Segment::Ocr => "ocr",
            Segment::Sound => "sound",
            Segment::Frame => "frame",
            Segment::Speech => "speech",
        }
    }

    pub fn parse(name: &str) -> Option<Segment> {
        Segment::ALL.into_iter().find(|segment| segment.as_str() == name)
    }
}

/// What a search is by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The full-text index alone: no model and no vectors, so it holds almost nothing in memory.
    Words,
    /// Meaning too: the text model and the vectors, loaded for the first such search. Words still count, since names
    /// and numbers are found by them.
    #[default]
    Meaning,
}

/// Which form of the vectors a search by meaning scans. Every form is written for every vector and mapped, so a
/// change costs nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scan {
    /// The most precise form whose scan fits `search.memory_mb`.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Every vector as it was made: exact scores.
    #[serde(rename = "exact-f32")]
    ExactF32,
    /// Every vector at half precision: half the memory, and nearly exact scores.
    #[serde(rename = "exact-f16")]
    ExactF16,
    /// Every vector's sketch, then the closest few thousand exactly: the least memory, exact scores for what's found.
    #[serde(rename = "sketch")]
    Sketch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(line: &str) -> Request {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn requests_read_as_documented() {
        let search = request(r#"{"id":1,"op":"search","q":"tax","kinds":["document"],"limit":5,"prefix_last":true}"#);
        let expected = Search {
            q: "tax".into(),
            mode: None,
            kinds: vec![Kind::Document],
            limit: 5,
            prefix_last: true,
            wait: false,
        };
        assert_eq!(search, Request { id: json!(1), op: Op::Search(expected) });
        // What's left out takes its default.
        let Op::Recent(recent) = request(r#"{"id":"a","op":"recent"}"#).op else { panic!() };
        assert_eq!(recent.limit, 40);
        assert_eq!(request(r#"{"id":2,"op":"status"}"#).op, Op::Status);
        assert_eq!(
            request(r#"{"id":3,"op":"text","segment_id":12}"#).op,
            Op::Text { segment_id: Some(12), path: None }
        );
        assert_eq!(
            request(r#"{"id":4,"op":"folders","action":"exclude","paths":["~/Old"]}"#).op,
            Op::Folders { action: Some(FolderAction::Exclude), paths: vec!["~/Old".into()] }
        );
        assert_eq!(
            request(r#"{"id":5,"op":"config","key":"search.mode","value":"words"}"#).op,
            Op::Config { key: Some("search.mode".into()), value: Some("words".into()) }
        );
        let Op::Preview(preview) = request(r#"{"id":6,"op":"preview","path":"/a.pdf","kind":"pdf","loc":3}"#).op else {
            panic!()
        };
        assert_eq!((preview.kind, preview.loc, preview.size), (Drawn::Pdf, Some(3.0), 640));
        assert!(serde_json::from_str::<Request>(r#"{"id":7,"op":"fly"}"#).is_err());
    }

    #[test]
    fn requests_write_what_they_read() {
        let ops = [
            Op::Status,
            Op::Unload,
            Op::Config { key: None, value: None },
            Op::Folders { action: None, paths: vec![] },
            Op::Text { segment_id: None, path: Some("/a.txt".into()) },
        ];
        for op in ops {
            let line = serde_json::to_string(&Request { id: json!(9), op: op.clone() }).unwrap();
            assert_eq!(request(&line).op, op, "{line}");
        }
        assert_eq!(serde_json::to_value(Op::Status).unwrap(), json!({"op": "status"}));
    }

    #[test]
    fn events_and_states_are_lowercase_words() {
        let loading = Event::Meaning { state: MeaningState::Loading, error: None };
        assert_eq!(serde_json::to_value(loading).unwrap(), json!({"event": "meaning", "state": "loading"}));
        let rounds = vec![RoundStatus {
            round: Round::Ocr,
            state: RoundState::Running,
            done: 3.0,
            total: 12.0,
            files: 9,
            seconds_left: None,
        }];
        let indexer = Event::Indexer(IndexerStatus { state: IndexerState::Indexing, rounds, ..Default::default() });
        let value = serde_json::to_value(indexer).unwrap();
        assert_eq!((value["event"].as_str(), value["state"].as_str()), (Some("indexer"), Some("indexing")));
        assert_eq!(
            value["rounds"][0],
            json!({"round": "ocr", "state": "running", "done": 3.0, "total": 12.0,
            "files": 9, "seconds_left": null})
        );
        assert!(Round::ALL.iter().all(|&round| Round::parse(round.as_str()) == Some(round)));
        assert!(Round::ALL.is_sorted());
        assert_eq!(serde_json::to_value(Scan::ExactF16).unwrap(), json!("exact-f16"));
        assert!(Segment::ALL.iter().all(|&segment| Segment::parse(segment.as_str()) == Some(segment)));
        assert!(Kind::ALL.iter().all(|&kind| Kind::parse(kind.as_str()) == Some(kind)));
    }
}
