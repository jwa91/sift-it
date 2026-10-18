//! Text files, code and records: their lines, decoded as they come (UTF-8, UTF-16 with a byte order mark, or Latin-1),
//! so a file of any size streams.

use crate::ingest::Content;
use anyhow::Result;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub fn read(path: &Path, content: &mut Content) -> Result<serde_json::Value> {
    feed(Lines::new(std::fs::File::open(path)?)?, content)
}

/// Hands a text's lines over as they're decoded.
pub fn feed<R: Read>(mut lines: Lines<R>, content: &mut Content) -> Result<serde_json::Value> {
    while let Some((line, ended)) = lines.next_line()? {
        content.line(&line, ended)?;
    }
    let lines = content.end_lines()?;
    Ok(serde_json::json!({ "passages": content.passages, "lines": lines }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::ingest::segment::passages::{Form, Passage, Text, passages};

    /// What streaming `bytes` through Lines and the cutting makes.
    fn streamed(bytes: &[u8]) -> Vec<Passage> {
        let mut lines = Lines::new(bytes).unwrap();
        let mut text = Text::new(Form::Markdown, &Settings::BASE);
        while let Some((line, ended)) = lines.next_line().unwrap() {
            text.line(&line, ended);
        }
        text.finish();
        text.ready().collect()
    }

    #[test]
    fn streamed_text_is_cut_as_a_whole_one() {
        let text = format!(
            "Title\r\n\r\n{}\n\n\nx{}\nend  ",
            "a sentence of words. ".repeat(300),
            "é".repeat(LINE_PART as usize) // one line longer than a part, split inside a character
        );
        let whole = passages(&text.replace('\r', ""), Form::Markdown, &Settings::BASE);
        assert_eq!(streamed(text.as_bytes()), whole);
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(streamed(&utf16), whole);
        // A line that isn't UTF-8 is Latin-1.
        assert_eq!(streamed(b"caf\xe9\nok")[0].text, "café\nok");
    }
}
