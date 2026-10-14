//! Text files and documents, cut into passages at paragraph boundaries.

use super::{Payload, Piece, Sink, title_of};
use crate::embedder::prompts;
use crate::store::Segment;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// A passage aims for this many characters (about 300 tokens of English), and is cut at MAX_PASSAGE at the latest.
/// Shorter passages point closer to the words that matched; longer ones carry more context per vector.
pub const TARGET_PASSAGE: usize = 1200;
pub const MAX_PASSAGE: usize = 2000;

pub fn read_plain(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path)?;
    let text = decode(&bytes);
    emit_passages(path, &text, sink)
}

/// rtf, doc, docx, odt, html, webarchive: macOS's textutil reads them all.
pub fn read_document(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let output = Command::new("/usr/bin/textutil")
        .args(["-convert", "txt", "-stdout", "-encoding", "UTF-8"])
        .arg(path)
        .output()
        .context("can't run textutil")?;
    if !output.status.success() {
        bail!("textutil couldn't read it: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    emit_passages(path, &String::from_utf8_lossy(&output.stdout), sink)
}

fn emit_passages(path: &Path, text: &str, sink: Sink) -> Result<serde_json::Value> {
    let title = title_of(path);
    let passages = passages(text);
    let count = passages.len();
    for passage in passages {
        let payload = if mostly_numbers(&passage.text) {
            Payload::None
        } else {
            Payload::Text(prompts::document(Some(&title), &passage.text))
        };
        sink(Piece {
            segment: Segment::Passage,
            loc: Some(passage.first_line as f64),
            loc_end: Some(passage.last_line as f64),
            text: Some(passage.text),
            payload,
        })?;
    }
    Ok(serde_json::json!({ "passages": count, "lines": text.lines().count() }))
}

/// UTF-8, or UTF-16 with a byte order mark, or Latin-1 as the last resort.
pub fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |rest: &[u8], big: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| if big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, true);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub text: String,
    pub first_line: usize,
    pub last_line: usize,
}

/// Cuts text into passages of about TARGET_PASSAGE characters, at blank lines where it can, then at line ends,
/// sentence ends, and spaces. Lines count from 1.
pub fn passages(text: &str) -> Vec<Passage> {
    passages_sized(text, TARGET_PASSAGE)
}

/// The same, aiming for `target` characters a passage.
pub fn passages_sized(text: &str, target: usize) -> Vec<Passage> {
    // Paragraphs: runs of non-blank lines, with their line numbers.
    let mut paragraphs: Vec<Passage> = Vec::new();
    let mut current: Option<Passage> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() {
            paragraphs.extend(current.take());
            continue;
        }
        match current.as_mut() {
            Some(paragraph) => {
                paragraph.text.push('\n');
                paragraph.text.push_str(line.trim_end());
                paragraph.last_line = number;
            }
            None => {
                current = Some(Passage { text: line.trim_end().to_string(), first_line: number, last_line: number })
            }
        }
    }
    paragraphs.extend(current);

    let mut passages: Vec<Passage> = Vec::new();
    let mut open: Option<Passage> = None;
    for paragraph in paragraphs.into_iter().flat_map(split_long) {
        match open.as_mut() {
            Some(passage) if passage.text.len() + paragraph.text.len() + 2 <= target => {
                passage.text.push_str("\n\n");
                passage.text.push_str(&paragraph.text);
                passage.last_line = paragraph.last_line;
            }
            _ => {
                passages.extend(open.take());
                open = Some(paragraph);
            }
        }
    }
    passages.extend(open);
    passages
}

/// A paragraph longer than MAX_PASSAGE, cut into pieces of at most that.
fn split_long(paragraph: Passage) -> Vec<Passage> {
    if paragraph.text.len() <= MAX_PASSAGE {
        return vec![paragraph];
    }
    let mut pieces = Vec::new();
    let mut rest = paragraph.text.as_str();
    let mut line = paragraph.first_line;
    while !rest.is_empty() {
        let cut = if rest.len() <= MAX_PASSAGE { rest.len() } else { cut_point(rest) };
        let (piece, tail) = rest.split_at(cut);
        let lines = piece.matches('\n').count();
        if !piece.trim().is_empty() {
            pieces.push(Passage { text: piece.trim().to_string(), first_line: line, last_line: line + lines });
        }
        line += lines;
        rest = tail;
    }
    pieces
}

/// Where to cut a long text, at most MAX_PASSAGE in: the last line end in the second half, else the last sentence
/// end, else the last space, else anywhere (on a character boundary).
fn cut_point(text: &str) -> usize {
    let mut limit = MAX_PASSAGE;
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    let window = &text[..limit];
    let half = limit / 2;
    for pattern in ["\n", ". ", "。", "? ", "! ", " "] {
        if let Some(position) = window.rfind(pattern)
            && position >= half
        {
            return position + pattern.len();
        }
    }
    limit
}

/// Text where digits outnumber letters (a table, a log of figures): searched by its exact words only.
pub fn mostly_numbers(text: &str) -> bool {
    let (mut letters, mut digits) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_alphabetic() {
            letters += 1;
        } else if c.is_numeric() {
            digits += 1;
        }
    }
    digits > 20 && digits * 2 > letters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paragraphs_join_into_one_passage() {
        let text = "First line\nsecond line\n\nAnother paragraph.\n";
        let passages = passages(text);
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0].first_line, 1);
        assert_eq!(passages[0].last_line, 4);
    }

    #[test]
    fn long_text_is_cut_near_the_target() {
        let paragraph = "word ".repeat(100);
        let text = vec![paragraph; 10].join("\n\n");
        let passages = passages(&text);
        assert!(passages.len() >= 4);
        assert!(passages.iter().all(|p| p.text.len() <= MAX_PASSAGE));
        let one = "x".repeat(5000);
        assert!(super::passages(&one).iter().all(|p| p.text.len() <= MAX_PASSAGE));
    }

    #[test]
    fn numbers_are_detected() {
        assert!(mostly_numbers("2024-01-01,12.5,13.2,99\n2024-01-02,12.7,13.9,98\n2024-01-03,11.1,10.0,97"));
        assert!(!mostly_numbers("The meeting on 3 March 2024 moved to room 12."));
    }
}
