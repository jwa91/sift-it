//! What reading a file takes, about, round by round, before it's read: what orders the words round's queue (cheapest
//! first), and what `sift-it estimate` says each round will take. The rates are measured ones, on an M5 MacBook Air
//! (each constant says where from); `sift-it bench-embed` measures a Mac's own. Once a round runs, progress.rs measures
//! how fast it goes instead.

use super::crawl::Found;
use crate::adapters::macos::{self, av};
use crate::config::{Settings, is_pdf};
use crate::ingest::rounds;
use crate::ingest::segment::sound::SPOKEN_EVERY;
use protocol::{Kind, Round};

/// The words round: text cut into passages.
const CUT_BYTES_PER_SECOND: f64 = 100e6;
/// The words round: a PDF page's text layer.
const PDF_PAGE_TEXT: f64 = 0.004;
/// The words round: textutil starting, for a document that isn't a PDF.
const TEXTUTIL: f64 = 0.05;
/// The words round: opening a picture, a recording or a video for its tags, its length, its metadata.
const OPEN: f64 = 0.02;

/// The text round: the text model reads about this many tokens a second, a passage's title and context included
/// (PROMPT), at about 4 characters a token: `sift-it bench-embed` embeds 64 passages of 976 characters in 0.95 s
/// together, and in 1.18 s one at a time (the text round's batches of 16 are in between).
const TOKENS_PER_SECOND: f64 = 15_000.0;
const PROMPT: f64 = 20.0;
const CHARS_PER_TOKEN: f64 = 4.0;
/// A passage ends at a sentence or a paragraph before `passage_chars`: the demo folder's texts' average 0.84 of it.
const PASSAGE_FILL: f64 = 0.84;
/// A PDF page with a text layer is a passage, of about 1,240 characters in the demo's PDFs.
const PAGE_CHARS: f64 = 1240.0;
/// What's said comes to a passage every 18 s, of about 230 characters (the demo's recordings and videos).
const SPEECH_PASSAGE_S: f64 = 18.0;
const SPEECH_CHARS: f64 = 230.0;
/// What OCR finds on a picture: 2.5 passages of about 1,240 characters (362 PDF pages drawn as screenshots).
const OCR_PASSAGES: f64 = 2.5;

/// The OCR round: a screenshot (Vision's text recognition), and a scanned page, drawn larger, with more text on it
/// (0.53 s for each of the 362 pages).
const OCR: f64 = 0.35;
const OCR_PAGE: f64 = 0.53;
/// A scanned page is a PDF page with no text layer. The estimate looks at this many pages of each PDF for how many are.
const SAMPLED_PAGES: usize = 4;

/// The pictures round: a photo, a screenshot or a scan, read and embedded (118 ms of it embedding at 140 tokens); and a
/// video, looked at every 2 s and its frame embedded when the shot changed (54 ms at 70 tokens). Its frames take the
/// most decoding: on the demo folder at `auto`, the pictures round took 829 s, of which the 4.4 hours of video about
/// 620 (the text round's 9,558 passages went in between, and 428 photos).
const PICTURE: f64 = 0.15;
const FRAMES_PER_SECOND: f64 = 0.039;

/// The sound round: all of the sound decoded, about 600 times as fast as it plays, and a window embedded every 20 s
/// in 0.2 s (`sift-it bench-embed`: 197 ms), or where speech was heard every 120 s (segment/sound.rs). Before it's
/// read, a recording whose speech is written down is taken for talk when it's 10 minutes long or more: on the demo
/// folder, all 34 such recordings were, and 33 of the 37 shorter ones were music or other sound. The full grid comes
/// to the demo folder's 30.6 hours of sound in 1,286 s (it took 1,277).
const SOUND_DECODE: f64 = 1.0 / 600.0;
const SOUND_WINDOW: f64 = 0.2;
const TALK_LONGER: f64 = 600.0;

/// The speech round: the demo's three podcasts (1.8 hours) written down in 24.2 s, 270 times as fast as they play.
/// Beside the GPU's rounds it went 140 to 160 times as fast on the demo folder, which the GPU's rounds outlast.
const SPEECH_PER_SECOND: f64 = 1.0 / 270.0;

/// What a file brings a round, about: how much, in the round's unit (protocol::Round::unit), and how long that takes
/// with the round to itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share {
    pub round: Round,
    pub total: f64,
    pub seconds: f64,
}

/// What reading a file's words takes, about: what orders the words round's queue. Nothing of a file only in iCloud is
/// read for it: a PDF counts as one page.
pub fn cost(file: &Found) -> f64 {
    let pages = || match file.cloud {
        true => 1,
        false => objc2::rc::autoreleasepool(|_| macos::pdf_pages(&file.path)).unwrap_or(1),
    };
    words(file, pages)
}

fn words(file: &Found, pages: impl FnOnce() -> usize) -> f64 {
    let size = file.size as f64;
    match file.kind {
        Kind::Plaintext | Kind::Structured | Kind::Code => size / CUT_BYTES_PER_SECOND,
        Kind::Document if is_pdf(&file.path) => pages() as f64 * PDF_PAGE_TEXT,
        // Other documents are mostly packaging (zip, markup): a tenth of them is text.
        Kind::Document => TEXTUTIL + size / 10.0 / CUT_BYTES_PER_SECOND,
        Kind::Image | Kind::Screenshot | Kind::Audio | Kind::Video => OPEN,
    }
}

/// What a file brings each round, at its depth: what its words are, and then each round it passes through. It looks
/// into the file as little as it can: a recording's or a video's length and whether it has sound, and a few of a
/// PDF's pages for whether they're scans. Not for a file only in iCloud: looking into it would download it.
pub fn shares(file: &Found) -> Vec<Share> {
    let settings = &file.settings;
    let mut found = rounds::Found::default();
    let mut text_pages = 0.0;
    let mut pages = 1;
    match file.kind {
        Kind::Audio => {
            found.duration = av::duration(&file.path);
            found.sound = found.duration > 0.0;
        }
        Kind::Video => {
            found.duration = av::duration(&file.path);
            found.sound = av::has_audio(&file.path);
        }
        Kind::Document if is_pdf(&file.path) => {
            let (all, scanned) = crate::ingest::extract::pdf::scans(&file.path, SAMPLED_PAGES).unwrap_or((1, 0.0));
            pages = all;
            found.scanned = (all as f64 * scanned).round() as usize;
            text_pages = (all - found.scanned) as f64;
        }
        _ => {}
    }
    let mut shares = vec![Share { round: Round::Words, total: 1.0, seconds: words(file, || pages) }];
    let mut passages = Passages::default();
    for (round, total) in rounds::after_words(file.kind, settings, &found) {
        let seconds = match round {
            Round::Ocr if file.kind == Kind::Document => total * OCR_PAGE,
            Round::Ocr => total * OCR,
            Round::Pictures if file.kind == Kind::Video => found.duration * FRAMES_PER_SECOND,
            Round::Pictures => total * PICTURE,
            Round::Sound => {
                let talk = file.kind == Kind::Audio
                    && found.duration >= TALK_LONGER
                    && rounds::transcribes(file.kind, settings);
                let hop = if talk { SPOKEN_EVERY } else { 20.0 };
                total * (SOUND_DECODE + SOUND_WINDOW / hop)
            }
            Round::Speech => total * SPEECH_PER_SECOND,
            Round::Words | Round::Text => 0.0,
        };
        match round {
            Round::Ocr => passages.add(total * OCR_PASSAGES, PAGE_CHARS),
            Round::Speech => passages.add(total / SPEECH_PASSAGE_S, SPEECH_CHARS),
            _ => {}
        }
        shares.push(Share { round, total, seconds });
    }
    if settings.embeds() {
        let size = file.size as f64;
        match file.kind {
            Kind::Plaintext | Kind::Code => passages.text(size, settings, 1.0),
            // A record is kept whole, so a passage of them runs past `passage_chars`: 1.75 times it in the demo's.
            Kind::Structured => passages.text(size, settings, 1.75 / PASSAGE_FILL),
            Kind::Document if is_pdf(&file.path) => passages.add(text_pages, PAGE_CHARS),
            Kind::Document => passages.text(size / 10.0, settings, 1.0),
            _ => {}
        }
        if passages.count > 0.0 {
            let seconds = (passages.chars / CHARS_PER_TOKEN + passages.count * PROMPT) / TOKENS_PER_SECOND;
            shares.insert(1, Share { round: Round::Text, total: passages.count, seconds });
        }
    }
    shares
}

/// The passages a file brings the text round, and their characters.
#[derive(Default)]
struct Passages {
    count: f64,
    chars: f64,
}

impl Passages {
    fn add(&mut self, count: f64, chars: f64) {
        self.count += count;
        self.chars += count * chars;
    }

    /// Text of `chars` characters, cut at `passage_chars`, its passages `longer` than most.
    fn text(&mut self, chars: f64, settings: &Settings, longer: f64) {
        self.count += (chars / (settings.passage_chars as f64 * PASSAGE_FILL * longer)).ceil();
        self.chars += chars;
    }
}
