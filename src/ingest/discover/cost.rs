//! What reading a file costs, about: what orders the queue (cheapest first), and what `sift-it status` and
//! `sift-it estimate` say is left. The costs are measured ones (`sift-it bench-embed` measures a Mac's own): text at
//! about 10,000 tokens a second, a photo in 0.15 s, a screenshot (picture, text and OCR) in 0.5 s, 30 s of sound in
//! 0.25 s. Writing down what's said is measured with the speech helper: 78 times as fast as the sound plays.

use super::crawl::Found;
use crate::adapters::macos::{self, av};
use crate::config::is_pdf;
use protocol::Kind;

const TOKENS_PER_SECOND: f64 = 10_000.0;
/// Text that's only cut into passages, not embedded.
const CUT_BYTES_PER_SECOND: f64 = 100e6;
const PHOTO: f64 = 0.15;
const SCREENSHOT: f64 = 0.5;
/// Reading the text on a screenshot, without embedding it.
const OCR: f64 = 0.35;
/// A PDF page: about 300 tokens of text, and reading it.
const PDF_PAGE: f64 = 300.0 / TOKENS_PER_SECOND + 0.004;
const PDF_PAGE_TEXT: f64 = 0.004;
/// Recordings: a 30 s window every 20 s, at 0.25 s each. Video adds the frames it keeps, at 0.07 s each: it's looked
/// at every 2 s, and a frame is kept when the shot changed, which in the demo's footage is one every 2.4 s.
const SOUND_PER_SECOND: f64 = 0.25 / 20.0;
const FRAMES_PER_SECOND: f64 = 0.07 / 2.5;
/// Opening a recording for its tags and length.
const OPEN: f64 = 0.02;
/// textutil starting.
const TEXTUTIL: f64 = 0.05;
/// Writing down what's said: a 32-minute podcast in 24 s with nothing else running, 78 times as fast as it plays.
const SPEECH_PER_SECOND: f64 = 1.0 / 78.0;

/// About how long writing down what's said in `seconds` of sound takes. Beside the reading, the two share the GPU: the
/// sum of both is about how long they take together.
pub fn speech(seconds: f64) -> f64 {
    seconds * SPEECH_PER_SECOND
}

/// About how long reading a file takes, in seconds, at its depth: what orders the queue (cheapest first) and what
/// `sift-it status` says is left. Writing down speech comes after, and isn't in it.
pub fn cost(file: &Found) -> f64 {
    let embeds = file.settings.embeds();
    let size = file.size as f64;
    match file.kind {
        // Plain text: about 4 bytes a token.
        Kind::Plaintext | Kind::Structured | Kind::Code if embeds => size / 4.0 / TOKENS_PER_SECOND,
        Kind::Plaintext | Kind::Structured | Kind::Code => size / CUT_BYTES_PER_SECOND,
        Kind::Document if is_pdf(&file.path) => {
            let pages = objc2::rc::autoreleasepool(|_| macos::pdf_pages(&file.path)).unwrap_or(1) as f64;
            pages * if embeds { PDF_PAGE } else { PDF_PAGE_TEXT }
        }
        // Other documents are mostly packaging (zip, markup): count a tenth of them as text, and textutil's start.
        Kind::Document if embeds => TEXTUTIL + size / 40.0 / TOKENS_PER_SECOND,
        Kind::Document => TEXTUTIL + size / 10.0 / CUT_BYTES_PER_SECOND,
        Kind::Image if embeds => PHOTO,
        Kind::Image => 0.0,
        Kind::Screenshot if embeds => SCREENSHOT,
        Kind::Screenshot => OCR,
        Kind::Audio if embeds => av::duration(&file.path) * SOUND_PER_SECOND,
        Kind::Video if embeds => {
            let sound = if file.settings.sound { SOUND_PER_SECOND } else { 0.0 };
            av::duration(&file.path) * (sound + FRAMES_PER_SECOND)
        }
        Kind::Audio | Kind::Video => OPEN,
    }
}
