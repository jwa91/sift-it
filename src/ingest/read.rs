//! Reading a file: extracted (extract/), cut into segments (segment/) and prepared for the model (prepare.rs), as a
//! stream of pieces to a sink, so a long video never sits in memory whole. Content joins the three: an extractor hands
//! it what it decodes, by its type.

use super::segment::frames::Shots;
use super::segment::passages::{self, Form, Text};
use super::segment::pictures;
use super::segment::sound::Windows;
use super::{extract, prepare};
use crate::config::is_pdf;
use crate::config::{Depth, Settings};
use anyhow::Result;
use embedder::AUDIO_RATE;
use embedder::imaging::Image;
use objc2_core_graphics::CGImage;
use protocol::{Kind, Segment};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

/// What the model is shown of a piece.
pub enum Payload {
    /// Keyword index only.
    None,
    /// Text, already framed with its document prompt.
    Text(String),
    Image(Image),
    /// 16 kHz mono samples, at most 30 s.
    Audio(Vec<f32>),
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

/// Goes up when the way a kind is read changes in a way that matters for what's indexed, or it learns to read files
/// it couldn't: the files it read (or failed to) are read again.
pub fn version(kind: Kind) -> u32 {
    match kind {
        Kind::Structured | Kind::Image => 1,
        Kind::Plaintext | Kind::Document | Kind::Screenshot | Kind::Audio | Kind::Video | Kind::Code => 2,
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

/// What's said in a file of this kind is written down.
pub fn transcribes(kind: Kind, settings: &Settings) -> bool {
    settings.depth == Depth::Speech && (kind == Kind::Audio || (kind == Kind::Video && settings.sound))
}

/// Reads a file into pieces, streamed to `sink`; returns what was learned about it (pages, duration, size) for the
/// index.
pub fn read(path: &Path, kind: Kind, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    extract::read(path, kind, settings, &mut Content::new(path, kind, settings, sink))
}

/// Where an extractor hands what it decodes, by its type. Each is cut by its segmenter, prepared for the model, and
/// sent on as pieces. What was sent is counted, for what the index learns about the file.
pub struct Content<'a> {
    settings: &'a Settings,
    /// What the file's text is embedded with (prepare::title).
    title: String,
    sink: Sink<'a>,
    /// The file's lines, cut by their form.
    lines: Text,
    sound: Windows,
    shots: Shots,
    /// Passages of text, windows of sound and frames sent.
    pub passages: usize,
    pub windows: usize,
    pub frames: usize,
}

impl<'a> Content<'a> {
    fn new(path: &Path, kind: Kind, settings: &'a Settings, sink: Sink<'a>) -> Self {
        Self {
            settings,
            title: prepare::title(path, kind),
            sink,
            lines: Text::new(Form::of(kind, path), settings),
            sound: Windows::default(),
            shots: Shots::new(settings.shot_change, settings.keep_every_s),
            passages: 0,
            windows: 0,
            frames: 0,
        }
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

    /// A picture: a photo (with no `text`: none is read), or a screenshot with the text read off it. Returns whether
    /// that text is long enough for passages of its own.
    pub fn picture(&mut self, image: &CGImage, text: Option<&str>) -> Result<bool> {
        // A screenshot is dense with small things.
        let budget = if text.is_some() { self.settings.text_tokens } else { self.settings.tokens };
        let text = text.unwrap_or_default();
        let cut = pictures::cut(text, self.settings.embeds(), false);
        if cut.picture {
            let words = cut.words.then(|| text.to_string());
            self.send(Segment::Picture, None, words, prepare::picture(image, budget)?)?;
        }
        if cut.passages {
            self.text(Segment::Ocr, None, text, Form::Prose)?;
        }
        Ok(pictures::long(text))
    }

    /// A scanned page, drawn, with the text read off it.
    pub fn scan(&mut self, page: usize, image: &CGImage, text: &str) -> Result<()> {
        let loc = Some(page as f64);
        let cut = pictures::cut(text, self.settings.embeds(), true);
        if cut.passages {
            return self.text(Segment::Ocr, loc, text, Form::Prose);
        }
        // Nothing to read: what the page shows, then.
        let words = cut.words.then(|| text.to_string());
        self.send(Segment::Picture, loc, words, prepare::picture(image, self.settings.text_tokens)?)
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
        let payload = prepare::text(&self.title, &passage.context, &passage.text, self.settings);
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
