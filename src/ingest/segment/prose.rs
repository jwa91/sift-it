//! Prose: text cut into passages at paragraph boundaries, of about `passage_chars` characters and at most
//! `max_passage_chars`. Text is cut as its lines come in, so a file of any size takes the memory of a passage or two.

use crate::config::Settings;

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

    /// How many lines were pushed.
    pub fn lines(&self) -> usize {
        self.line
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_PASSAGE: usize = Settings::BASE.max_passage_chars;

    fn passages(text: &str) -> Vec<Passage> {
        super::passages(text, &Settings::BASE)
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
}
