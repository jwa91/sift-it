//! The rounds (docs/design.md, "Rounds"): indexing goes over the whole collection in rounds, each with one tool, words
//! first. This says which rounds a file passes through once its words are read, and how much it has for each, in the
//! round's unit (protocol::Round::unit), and how big an item of each round is. Pure.

use crate::config::{Depth, Settings};
use protocol::{Kind, Round};

/// A video is looked at every this many seconds (adapters/macos/av.rs), and the pictures round counts what it looks at.
pub const FRAME_STEP: f64 = 2.0;

/// An item of a video's pictures is at most this many seconds of it (150 frames looked at, a few seconds of the GPU),
/// and the sound round says how far it has got this often (15 windows): what's found so far is written, and the item
/// stops there when the run wants its lane. What runs can change between two items, or at such a checkpoint, and a
/// file stopped partway goes on from where it was.
pub const SLICE: f64 = 300.0;

/// An item of the OCR round is at most this many scanned pages (about 5 s).
pub const SLICE_PAGES: usize = 10;

/// What reading a file's words found out that decides its other rounds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Found {
    /// Seconds, for a recording or a video.
    pub duration: f64,
    /// A recording or video with sound in it.
    pub sound: bool,
    /// A PDF's pages without a text layer: scans.
    pub scanned: usize,
}

/// What's said in a file of this kind is written down.
pub fn transcribes(kind: Kind, settings: &Settings) -> bool {
    settings.depth == Depth::Speech && (kind == Kind::Audio || (kind == Kind::Video && settings.sound))
}

/// The rounds a file passes through after its words, each with how much it has. The text round isn't among them: it
/// comes with the passages that wait for it, whichever round wrote them. Nor are the pictures of a PDF's scanned pages
/// with little text on them: OCR finds those.
pub fn after_words(kind: Kind, settings: &Settings, found: &Found) -> Vec<(Round, f64)> {
    let embeds = settings.embeds();
    let mut rounds = Vec::new();
    match kind {
        Kind::Screenshot => {
            rounds.push((Round::Ocr, 1.0));
            if embeds {
                rounds.push((Round::Pictures, 1.0));
            }
        }
        Kind::Image if embeds => rounds.push((Round::Pictures, 1.0)),
        Kind::Document if found.scanned > 0 => rounds.push((Round::Ocr, found.scanned as f64)),
        Kind::Audio | Kind::Video if found.duration > 0.0 => {
            if kind == Kind::Video && embeds {
                rounds.push((Round::Pictures, (found.duration / FRAME_STEP).ceil()));
            }
            if found.sound && embeds && (kind == Kind::Audio || settings.sound) {
                rounds.push((Round::Sound, found.duration));
            }
            if found.sound && transcribes(kind, settings) {
                rounds.push((Round::Speech, found.duration));
            }
        }
        _ => {}
    }
    rounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(depth: Depth) -> Settings {
        Settings { depth, ..Settings::BASE }
    }

    #[test]
    fn a_files_kind_and_depth_decide_its_rounds() {
        let rounds = |kind, depth, found: Found| -> Vec<Round> {
            after_words(kind, &at(depth), &found).into_iter().map(|(round, _)| round).collect()
        };
        let none = Found::default();
        let film = Found { duration: 61.0, sound: true, scanned: 0 };
        assert_eq!(rounds(Kind::Plaintext, Depth::Speech, none), []);
        assert_eq!(rounds(Kind::Image, Depth::Meaning, none), [Round::Pictures]);
        assert_eq!(rounds(Kind::Image, Depth::Words, none), []);
        assert_eq!(rounds(Kind::Screenshot, Depth::Meaning, none), [Round::Ocr, Round::Pictures]);
        // Text on screenshots and scans is words: it's read at every depth.
        assert_eq!(rounds(Kind::Screenshot, Depth::Words, none), [Round::Ocr]);
        assert_eq!(rounds(Kind::Document, Depth::Words, Found { scanned: 3, ..none }), [Round::Ocr]);
        assert_eq!(rounds(Kind::Video, Depth::Speech, film), [Round::Pictures, Round::Sound, Round::Speech]);
        assert_eq!(rounds(Kind::Video, Depth::Meaning, film), [Round::Pictures, Round::Sound]);
        assert_eq!(rounds(Kind::Video, Depth::Speech, Found { sound: false, ..film }), [Round::Pictures]);
        assert_eq!(rounds(Kind::Audio, Depth::Words, film), []);
        let silent = Settings { sound: false, ..at(Depth::Speech) };
        let video: Vec<Round> = after_words(Kind::Video, &silent, &film).into_iter().map(|(r, _)| r).collect();
        assert_eq!(video, [Round::Pictures]);
    }

    #[test]
    fn each_round_counts_in_its_unit() {
        let film = Found { duration: 61.0, sound: true, scanned: 0 };
        assert_eq!(
            after_words(Kind::Video, &at(Depth::Speech), &film),
            [(Round::Pictures, 31.0), (Round::Sound, 61.0), (Round::Speech, 61.0)]
        );
        let scan = Found { scanned: 12, ..Found::default() };
        assert_eq!(after_words(Kind::Document, &at(Depth::Meaning), &scan), [(Round::Ocr, 12.0)]);
    }
}
