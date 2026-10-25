//! Reading a file, for one round: extracted (extract/), cut into segments (segment/) and prepared for the model
//! (prepare.rs), as a stream of pieces to a sink, so a long video never sits in memory whole. Content joins the three:
//! an extractor hands it what it decodes, by its type. A round that goes in items reads one item: from where the last
//! one ended to where this one does (rounds::SLICE).

use super::rounds::{self, Found};
use super::segment::frames::Shots;
use super::segment::passages::{self, Form, Text};
use super::segment::pictures;
use super::segment::sound::Windows;
use super::{extract, prepare};
use crate::adapters::macos::icloud;
use crate::config::Settings;
use crate::config::is_pdf;
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use embedder::AUDIO_RATE;
use embedder::imaging::Image;
use objc2_core_graphics::CGImage;
use protocol::{Kind, Round, Segment};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

/// What the model is shown of a piece, or when.
pub enum Payload {
    /// Keyword index only.
    None,
    /// Text the text round embeds, with what it was cut from (a heading, a signature, a header row), or nothing.
    Text(String),
    /// A picture the pictures round embeds: a photo, a screenshot, a scanned page with little text on it.
    Later,
    Image(Image),
    /// 16 kHz mono samples, at most 30 s.
    Audio(Vec<f32>),
}

/// Where an item of a round starts: how much of the file's work in it is done, and where it goes on (Work).
#[derive(Debug, Clone, Default)]
pub struct From {
    pub done: f64,
    pub resume: Option<serde_json::Value>,
    /// The pages whose pictures wait for the pictures round (a PDF's scanned pages with little text).
    pub pages: Vec<usize>,
    /// For the sound round, where speech was heard in it: (start, end) in seconds.
    pub spoken: Vec<(f64, f64)>,
}

/// How reading an item ended: what was learned about the file, how much of its work in the round is done now, where
/// the next item goes on, and whether that was the last. For words, what decides its other rounds, and those rounds.
#[derive(Debug, Clone, Default)]
pub struct Ended {
    pub info: serde_json::Value,
    pub done: f64,
    pub resume: Option<serde_json::Value>,
    pub finished: bool,
    pub found: Found,
    pub rounds: Vec<(Round, f64)>,
}

impl Ended {
    /// The whole of a file's work in a round, read in one item.
    pub fn whole(info: serde_json::Value, done: f64) -> Self {
        Self { info, done, finished: true, ..Default::default() }
    }
}

/// A segment, prepared: what the index keeps of it, and what the model is shown.
pub struct Piece {
    pub segment: Segment,
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub text: Option<String>,
    pub payload: Payload,
}

pub type Sink<'a> = &'a mut dyn FnMut(Piece) -> Result<()>;

/// Where a long item says how far it has got: what it handed over so far is written, with where it goes on. It goes
/// on unless this says to stop (the run wants its lane), and the next item goes on from there.
pub type Checkpoint<'a> = &'a mut dyn FnMut(&Ended) -> Result<bool>;

/// Goes up when the way a kind is read changes in a way that matters for what's indexed, or it learns to read files
/// it couldn't: the files it read (or failed to) are read again.
pub fn version(kind: Kind) -> u32 {
    match kind {
        Kind::Structured | Kind::Image => 1,
        Kind::Plaintext | Kind::Document | Kind::Screenshot | Kind::Code => 2,
        // Where speech was heard, the sound round keeps fewer windows (segment/sound.rs).
        Kind::Audio | Kind::Video => 3,
    }
}

/// What decides how a file is read: its kind's version and the reading values it uses at its depth. When it changes,
/// the file is read again. Whether what's said in it is written down isn't in it: that comes after reading.
pub fn fingerprint(path: &Path, kind: Kind, settings: &Settings) -> String {
    let words = match kind {
        Kind::Plaintext | Kind::Structured | Kind::Code | Kind::Document | Kind::Screenshot => {
            json!([settings.passage_chars, settings.max_passage_chars])
        }
        Kind::Image | Kind::Audio | Kind::Video => json!(null),
    };
    let meaning = settings.embeds().then(|| match kind {
        Kind::Plaintext | Kind::Structured | Kind::Code | Kind::Audio => json!(null),
        Kind::Document if is_pdf(path) => json!(settings.text_tokens),
        Kind::Document => json!(null),
        Kind::Image => json!(settings.tokens),
        Kind::Screenshot => json!(settings.text_tokens),
        Kind::Video => json!([settings.tokens, settings.shot_change, settings.keep_every_s, settings.sound]),
    });
    let digest = Sha256::digest(format!("{}/{}/{}", kind.as_str(), version(kind), json!([words, meaning])));
    digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads an item of a file's work in `round` (words, OCR, pictures or sound) into pieces, streamed to `sink`, saying
/// how far a long one has got at `checkpoint`. A file's words say which rounds it passes through next
/// (rounds::after_words). A file only in iCloud isn't read: reading it fails rather than download it, and some
/// frameworks would say it's empty (AVFoundation, a recording of no length).
pub fn read(
    round: Round,
    path: &Path,
    kind: Kind,
    settings: &Settings,
    from: &From,
    sink: Sink,
    checkpoint: Checkpoint,
) -> Result<Ended> {
    if icloud::cloud_only_at(path) {
        bail!("it's only in iCloud");
    }
    let mut content = Content::new(path, kind, settings, from, sink, checkpoint);
    let mut ended = extract::read(round, path, kind, settings, from, &mut content)?;
    if round == Round::Words {
        ended.rounds = rounds::after_words(kind, settings, &ended.found);
    }
    Ok(ended)
}

/// Where an extractor hands what it decodes, by its type. Each is cut by its segmenter, prepared for the model, and
/// sent on as pieces. What was sent is counted, for what the index learns about the file.
pub struct Content<'a> {
    settings: &'a Settings,
    sink: Sink<'a>,
    checkpoint: Checkpoint<'a>,
    /// The file's lines, cut by their form.
    lines: Text,
    pub sound: Windows,
    pub shots: Shots,
    /// Passages of text, windows of sound and frames sent.
    pub passages: usize,
    pub windows: usize,
    pub frames: usize,
}

impl<'a> Content<'a> {
    /// Content for an item that goes on from `from`: after the frame kept last, with the windows and frames sent
    /// before counted.
    fn new(
        path: &Path,
        kind: Kind,
        settings: &'a Settings,
        from: &From,
        sink: Sink<'a>,
        checkpoint: Checkpoint<'a>,
    ) -> Self {
        let resume = from.resume.as_ref();
        let count = |key: &str| resume.and_then(|resume| resume[key].as_u64()).unwrap_or(0) as usize;
        let kept = resume.and_then(|resume| {
            let at = resume["kept"][0].as_f64()?;
            Some((at, BASE64.decode(resume["kept"][1].as_str()?).ok()?))
        });
        Self {
            settings,
            sink,
            checkpoint,
            lines: Text::new(Form::of(kind, path), settings),
            sound: Windows::default(),
            shots: Shots::new(settings.shot_change, settings.keep_every_s).after(kept),
            passages: 0,
            windows: count("windows"),
            frames: count("frames"),
        }
    }

    /// How far a long item has got (`done`, in its round's unit): whether it goes on.
    pub fn checkpoint(&mut self, done: f64) -> Result<bool> {
        let at = Ended { done, resume: Some(self.resume()), ..Ended::default() };
        (self.checkpoint)(&at)
    }

    /// Where the next item goes on: the frame kept last, and the windows and frames sent so far.
    pub fn resume(&self) -> serde_json::Value {
        let kept = self.shots.last().map(|(at, thumbnail)| json!([at, BASE64.encode(thumbnail)]));
        json!({ "kept": kept, "windows": self.windows, "frames": self.frames })
    }

    /// A line of text, or a part of a long one (`ended`: the line ends with it). A passage's place is its lines.
    pub fn line(&mut self, text: &str, ended: bool) -> Result<()> {
        self.lines.line(text, ended);
        self.send_cut()
    }

    /// The end of the lines. Returns how many there were.
    pub fn end_lines(&mut self) -> Result<usize> {
        self.lines.finish();
        self.send_cut()?;
        Ok(self.lines.lines())
    }

    fn send_cut(&mut self) -> Result<()> {
        let cut: Vec<passages::Passage> = self.lines.ready().collect();
        for passage in cut {
            let (first, last) = (passage.first_line as f64, passage.last_line as f64);
            self.passage(Segment::Passage, Some(first), Some(last), passage)?;
        }
        Ok(())
    }

    /// A whole text of a form at one place in the file (a page's text layer, a notebook cell's source, text read off
    /// a picture): its passages keep that place.
    pub fn text(&mut self, segment: Segment, loc: Option<f64>, text: &str, form: Form) -> Result<()> {
        for passage in passages::passages(text, form, self.settings) {
            self.passage(segment, loc, None, passage)?;
        }
        Ok(())
    }

    /// A photo or a screenshot, whose vector the pictures round makes: at a depth that embeds.
    pub fn picture_later(&mut self) -> Result<()> {
        if self.settings.embeds() {
            self.send(Segment::Picture, None, None, Payload::Later)?;
        }
        Ok(())
    }

    /// The text read off a screenshot: passages of its own when there's enough of it, else words with its picture.
    /// Returns whether it was enough.
    pub fn read_off(&mut self, text: &str) -> Result<bool> {
        let cut = pictures::cut(text, self.settings.embeds(), false);
        if cut.words {
            self.send(Segment::Picture, None, Some(text.to_string()), Payload::None)?;
        }
        if cut.passages {
            self.text(Segment::Ocr, None, text, Form::Prose)?;
        }
        Ok(pictures::long(text))
    }

    /// The text read off a scanned page: passages, when there's enough of it; else the page is what it shows, which
    /// the pictures round embeds, with the words found on it.
    pub fn read_off_page(&mut self, page: usize, text: &str) -> Result<()> {
        let loc = Some(page as f64);
        let cut = pictures::cut(text, self.settings.embeds(), true);
        if cut.passages {
            return self.text(Segment::Ocr, loc, text, Form::Prose);
        }
        let words = cut.words.then(|| text.to_string());
        self.send(Segment::Picture, loc, words, Payload::Later)
    }

    /// What a picture shows, for the pictures round: a photo (`loc` None) or a scanned page, fitted to `budget` tokens.
    pub fn shown(&mut self, loc: Option<f64>, image: &CGImage, budget: u32) -> Result<()> {
        self.send(Segment::Picture, loc, None, prepare::picture(image, budget)?)
    }

    /// A video's frame at `at` seconds, with its thumbnail (macos::signature) to tell shots apart.
    pub fn frame(&mut self, at: f64, image: &CGImage, thumbnail: &[u8]) -> Result<()> {
        if !self.shots.keep(at, thumbnail) {
            return Ok(());
        }
        self.frames += 1;
        self.send(Segment::Frame, Some(at), None, prepare::picture(image, self.settings.tokens)?)
    }

    /// Sound as it's decoded, 16 kHz mono.
    pub fn sound(&mut self, samples: &[f32]) -> Result<()> {
        let Self { sound, sink, windows, .. } = self;
        sound.push(samples, &mut |start, window| send_window(sink, windows, start, window))
    }

    /// The end of the sound.
    pub fn end_sound(&mut self) -> Result<()> {
        let Self { sound, sink, windows, .. } = self;
        sound.finish(&mut |start, window| send_window(sink, windows, start, window))
    }

    fn passage(
        &mut self,
        segment: Segment,
        loc: Option<f64>,
        loc_end: Option<f64>,
        passage: passages::Passage,
    ) -> Result<()> {
        self.passages += 1;
        let payload = prepare::text(passage.context, &passage.text, self.settings);
        (self.sink)(Piece { segment, loc, loc_end, text: Some(passage.text), payload })
    }

    fn send(&mut self, segment: Segment, loc: Option<f64>, text: Option<String>, payload: Payload) -> Result<()> {
        (self.sink)(Piece { segment, loc, loc_end: None, text, payload })
    }
}

fn send_window(sink: &mut Sink, windows: &mut usize, start: f64, samples: &[f32]) -> Result<()> {
    *windows += 1;
    let end = start + samples.len() as f64 / AUDIO_RATE as f64;
    sink(Piece {
        segment: Segment::Sound,
        loc: Some(start),
        loc_end: Some(end),
        text: None,
        payload: prepare::sound(samples),
    })
}
