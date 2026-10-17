//! Text files and documents, cut into passages at paragraph boundaries. Text is read and cut as it streams in, so a
//! file of any size takes the memory of a passage or two.

use super::{Piece, Sink, text_payload, title_of};
use crate::config::Settings;
use crate::store::Segment;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

pub fn read_plain(path: &Path, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    emit_passages(Lines::new(std::fs::File::open(path)?)?, &title_of(path), settings, sink)
}

/// rtf, doc, docx, odt, html, webarchive: macOS's textutil reads them all.
pub fn read_document(path: &Path, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    let mut child = Command::new("/usr/bin/textutil")
        .args(["-convert", "txt", "-stdout", "-encoding", "UTF-8"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("can't run textutil")?;
    let stdout = child.stdout.take().expect("piped");
    // The pipe closes when this returns, so textutil ends even when reading stopped early.
    let read = Lines::new(stdout)
        .map_err(anyhow::Error::from)
        .and_then(|lines| emit_passages(lines, &title_of(path), settings, sink));
    let output = child.wait_with_output()?;
    let info = read?;
    if !output.status.success() {
        bail!("textutil couldn't read it: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(info)
}

/// Text's passages, as they're cut, each embedded with `title`.
pub fn emit_passages<R: Read>(
    mut lines: Lines<R>,
    title: &str,
    settings: &Settings,
    sink: Sink,
) -> Result<serde_json::Value> {
    let mut cutter = Cutter::new(settings);
    let mut count = 0;
    let mut emit = |cutter: &mut Cutter| -> Result<()> {
        for passage in cutter.ready() {
            count += 1;
            sink(Piece {
                segment: Segment::Passage,
                loc: Some(passage.first_line as f64),
                loc_end: Some(passage.last_line as f64),
                payload: text_payload(title, &passage.text, settings),
                text: Some(passage.text),
            })?;
        }
        Ok(())
    };
    while let Some((line, ended)) = lines.next_line()? {
        cutter.push(&line, ended);
        emit(&mut cutter)?;
    }
    cutter.finish();
    emit(&mut cutter)?;
    Ok(serde_json::json!({ "passages": count, "lines": cutter.line }))
}

/// A text's lines, decoded as they come: UTF-8, or UTF-16 with a byte order mark. A line that isn't UTF-8 is read as
/// Latin-1. A line longer than LINE_PART comes in parts, so even a file that's one long line streams.
pub struct Lines<R> {
    reader: BufReader<R>,
    /// Some(big endian) for UTF-16.
    utf16: Option<bool>,
    /// The start of a character a part ended in the middle of.
    carry: Vec<u8>,
}

const LINE_PART: u64 = 64 << 10;

impl<R: Read> Lines<R> {
    pub fn new(reader: R) -> std::io::Result<Self> {
        let mut reader = BufReader::with_capacity(LINE_PART as usize, reader);
        let start = reader.fill_buf()?;
        let (bom, utf16) = if start.starts_with(&[0xEF, 0xBB, 0xBF]) {
            (3, None)
        } else if start.starts_with(&[0xFF, 0xFE]) {
            (2, Some(false))
        } else if start.starts_with(&[0xFE, 0xFF]) {
            (2, Some(true))
        } else {
            (0, None)
        };
        reader.consume(bom);
        Ok(Self { reader, utf16, carry: Vec::new() })
    }

    /// The next line or part of one, without its line break, and whether the line ends with it. None at the end.
    pub fn next_line(&mut self) -> std::io::Result<Option<(String, bool)>> {
        let (mut text, ended) = match self.utf16 {
            None => {
                let mut bytes = std::mem::take(&mut self.carry);
                let read = (&mut self.reader).take(LINE_PART).read_until(b'\n', &mut bytes)?;
                if read == 0 && bytes.is_empty() {
                    return Ok(None);
                }
                let newline = bytes.last() == Some(&b'\n');
                // A line ends at a line break, or where the text does (a read that stops short).
                let ended = newline || (read as u64) < LINE_PART;
                if newline {
                    bytes.pop();
                } else if !ended
                    && let Err(error) = std::str::from_utf8(&bytes)
                    && error.error_len().is_none()
                {
                    // A part that ends in the middle of a character: the rest of it comes with the next part.
                    self.carry = bytes.split_off(error.valid_up_to());
                }
                let text = match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(error) => error.as_bytes().iter().map(|&b| b as char).collect(),
                };
                (text, ended)
            }
            Some(big) => {
                let mut units = Vec::new();
                let mut ended = false;
                let mut pair = [0u8; 2];
                while units.len() < LINE_PART as usize / 2 {
                    match self.reader.read_exact(&mut pair) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(error) => return Err(error),
                    }
                    let unit = if big { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) };
                    if unit == u16::from(b'\n') {
                        ended = true;
                        break;
                    }
                    units.push(unit);
                }
                if units.is_empty() && !ended {
                    return Ok(None);
                }
                let at_end = !ended && self.reader.fill_buf()?.is_empty();
                (String::from_utf16_lossy(&units), ended || at_end)
            }
        };
        if ended && text.ends_with('\r') {
            text.pop();
        }
        Ok(Some((text, ended)))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub text: String,
    pub first_line: usize,
    pub last_line: usize,
}

/// Cuts text into passages of about `passage_chars` characters and at most `max_passage_chars`, at blank lines where
/// it can, then at line ends, sentence ends, and spaces. Lines count from 1.
pub fn passages(text: &str, settings: &Settings) -> Vec<Passage> {
    let mut cutter = Cutter::new(settings);
    for line in text.lines() {
        cutter.push(line, true);
    }
    cutter.finish();
    cutter.ready().collect()
}

/// Cuts text into passages as its lines come in. A paragraph (a run of non-blank lines) longer than the maximum is cut
/// as soon as it is, so what's held stays below a passage and a paragraph's piece.
pub struct Cutter {
    target: usize,
    max: usize,
    /// The line last pushed (from 1), and whether it's still to be continued.
    line: usize,
    in_line: bool,
    /// The paragraph being read: what's left of it after any pieces cut off, the line that starts at, and whether a
    /// piece was cut off.
    paragraph: Option<Passage>,
    cut: bool,
    /// The passage being joined from paragraphs.
    open: Option<Passage>,
    done: Vec<Passage>,
}

impl Cutter {
    pub fn new(settings: &Settings) -> Self {
        let max = settings.max_passage_chars.max(1);
        let target = settings.passage_chars.min(max);
        Self { target, max, line: 0, in_line: false, paragraph: None, cut: false, open: None, done: Vec::new() }
    }

    /// A line, or a part of one: `ended` says whether the line ends with it.
    pub fn push(&mut self, text: &str, ended: bool) {
        let continued = std::mem::replace(&mut self.in_line, !ended);
        if !continued {
            self.line += 1;
            if text.trim().is_empty() && ended {
                self.end_paragraph();
                return;
            }
        }
        let line = self.line;
        // Lines are kept without their trailing spaces: only a line's last part has them taken off.
        let text = if ended { text.trim_end() } else { text };
        match self.paragraph.as_mut() {
            Some(paragraph) => {
                if !continued {
                    paragraph.text.push('\n');
                }
                paragraph.text.push_str(text);
                paragraph.last_line = line;
            }
            None => self.paragraph = Some(Passage { text: text.to_string(), first_line: line, last_line: line }),
        }
        while self.paragraph.as_ref().is_some_and(|paragraph| paragraph.text.len() > self.max) {
            self.cut_piece();
        }
    }

    /// The end of the text.
    pub fn finish(&mut self) {
        self.end_paragraph();
        self.done.extend(self.open.take());
    }

    /// The passages cut so far.
    pub fn ready(&mut self) -> std::vec::Drain<'_, Passage> {
        self.done.drain(..)
    }

    /// Cuts the paragraph's first piece off, at most `max` long.
    fn cut_piece(&mut self) {
        let paragraph = self.paragraph.as_mut().expect("a paragraph");
        let cut = cut_point(&paragraph.text, self.max);
        let rest = paragraph.text.split_off(cut);
        let piece = std::mem::replace(&mut paragraph.text, rest);
        let first = paragraph.first_line;
        let lines = piece.matches('\n').count();
        paragraph.first_line += lines;
        self.cut = true;
        if !piece.trim().is_empty() {
            self.join(Passage { text: piece.trim().to_string(), first_line: first, last_line: first + lines });
        }
    }

    fn end_paragraph(&mut self) {
        let Some(mut paragraph) = self.paragraph.take() else { return };
        // The last piece of a paragraph that was cut is trimmed like the others.
        if std::mem::take(&mut self.cut) {
            if paragraph.text.trim().is_empty() {
                return;
            }
            paragraph.last_line = paragraph.first_line + paragraph.text.matches('\n').count();
            paragraph.text = paragraph.text.trim().to_string();
        }
        self.join(paragraph);
    }

    /// Adds a paragraph (or a piece of one) to the open passage, or starts a new one when it doesn't fit. Paragraphs
    /// are joined by as many line breaks as are between them in the file: a match's line is the passage's first line
    /// plus its line within the passage.
    fn join(&mut self, paragraph: Passage) {
        match self.open.as_mut() {
            Some(passage) => {
                let gap = paragraph.first_line.saturating_sub(passage.last_line).max(1);
                if passage.text.len() + gap + paragraph.text.len() <= self.target {
                    passage.text.push_str(&"\n".repeat(gap));
                    passage.text.push_str(&paragraph.text);
                    passage.last_line = paragraph.last_line;
                } else {
                    self.done.push(std::mem::replace(passage, paragraph));
                }
            }
            None => self.open = Some(paragraph),
        }
    }
}

/// Where to cut a long text, at most `max` in: the last line end in the second half, else the last sentence end,
/// else the last space, else anywhere (on a character boundary, and past the first character).
fn cut_point(text: &str, max: usize) -> usize {
    let mut limit = max;
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    if limit == 0 {
        return text.chars().next().map_or(text.len(), char::len_utf8);
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

    const MAX_PASSAGE: usize = Settings::BASE.max_passage_chars;

    fn passages(text: &str) -> Vec<Passage> {
        super::passages(text, &Settings::BASE)
    }

    /// What streaming `bytes` through Lines and a Cutter makes.
    fn streamed(bytes: &[u8]) -> Vec<Passage> {
        let mut lines = Lines::new(bytes).unwrap();
        let mut cutter = Cutter::new(&Settings::BASE);
        while let Some((line, ended)) = lines.next_line().unwrap() {
            cutter.push(&line, ended);
        }
        cutter.finish();
        cutter.ready().collect()
    }

    #[test]
    fn short_paragraphs_join_into_one_passage() {
        let text = "First line\nsecond line\n\nAnother paragraph.\n";
        let passages = passages(text);
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0].first_line, 1);
        assert_eq!(passages[0].last_line, 4);
    }

    #[test]
    fn a_line_in_a_passage_is_its_line_in_the_file() {
        // Search finds a match's line as the passage's first line plus the line within it.
        let text = format!("Introduction.{}needle is here.\n", "\n".repeat(11));
        let passage = &passages(&text)[0];
        let within = passage.text.lines().position(|line| line.contains("needle")).unwrap();
        assert_eq!(passage.first_line + within, 12);
        assert_eq!(passage.last_line, 12);
    }

    #[test]
    fn long_text_is_cut_near_the_target() {
        let paragraph = "word ".repeat(100);
        let text = vec![paragraph; 10].join("\n\n");
        let cut = passages(&text);
        assert!(cut.len() >= 4);
        assert!(cut.iter().all(|p| p.text.len() <= MAX_PASSAGE));
        let one = "x".repeat(5000);
        assert!(passages(&one).iter().all(|p| p.text.len() <= MAX_PASSAGE));
        let tiny = Settings { passage_chars: 0, max_passage_chars: 0, ..Settings::BASE };
        assert_eq!(super::passages("één", &tiny).len(), 3);
    }

    #[test]
    fn streamed_text_is_cut_as_a_whole_one() {
        let text = format!(
            "Title\r\n\r\n{}\n\n\nx{}\nend  ",
            "a sentence of words. ".repeat(300),
            "é".repeat(LINE_PART as usize) // one line longer than a part, split inside a character
        );
        assert_eq!(streamed(text.as_bytes()), passages(&text.replace('\r', "")));
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(streamed(&utf16), passages(&text.replace('\r', "")));
        // A line that isn't UTF-8 is Latin-1.
        assert_eq!(streamed(b"caf\xe9\nok")[0].text, "café\nok");
    }

    #[test]
    fn numbers_are_detected() {
        assert!(mostly_numbers("2024-01-01,12.5,13.2,99\n2024-01-02,12.7,13.9,98\n2024-01-03,11.1,10.0,97"));
        assert!(!mostly_numbers("The meeting on 3 March 2024 moved to room 12."));
    }
}
