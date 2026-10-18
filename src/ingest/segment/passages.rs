//! Passages: text cut where its form breaks, into passages of about `passage_chars` characters and at most
//! `max_passage_chars`. A form (prose.rs, code.rs, records.rs) says how strong the break before each line is: a
//! heading, a definition or a record breaks more than a paragraph or a blank line does. What lies between two breaks
//! of a strength is a part. A part that fits the maximum stays whole; a longer one is cut up at its next weaker breaks,
//! down to a paragraph or a row, which is cut at line ends, sentences or spaces into pieces of about the passage size.
//! Parts next to each other are joined up to the passage size, and so are the pieces of a part cut up, but not with
//! what's around it. Those pieces are read with the part's first line (a heading, a signature, a key), and a table's
//! rows with its header row: the model sees what they belong to.
//!
//! Text is cut as its lines come in, so a file of any size takes the memory of a passage or two.

use super::code::{self, Code};
use super::prose::{self, Prose};
use super::records::{self, Json, Rows};
use crate::config::Settings;
use crate::config::extension_of;
use protocol::Kind;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub text: String,
    pub first_line: usize,
    pub last_line: usize,
    /// What the model reads before the passage: the first line of each part it was cut from, or a table's header
    /// row. Empty for most.
    pub context: String,
}

/// The forms text comes in, each cut its own way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Form {
    /// Paragraphs: a page, a document, text read off a picture.
    Prose,
    /// Paragraphs and the sections headings start: Markdown, and plain text, which often uses its headings.
    Markdown,
    Code,
    Yaml,
    Toml,
    /// CSV and TSV.
    Rows,
    Json,
    JsonLines,
}

impl Form {
    /// The form of a file of `kind` at `path`.
    pub fn of(kind: Kind, path: &Path) -> Form {
        match kind {
            Kind::Plaintext => Form::Markdown,
            Kind::Code => Form::Code,
            Kind::Structured => match extension_of(path).as_str() {
                "csv" | "tsv" => Form::Rows,
                "json" => Form::Json,
                "jsonl" | "ndjson" => Form::JsonLines,
                "toml" => Form::Toml,
                _ => Form::Yaml,
            },
            _ => Form::Prose,
        }
    }
}

/// Text of one form, cut into passages as its lines come in.
pub struct Text {
    form: Reader,
    cutter: Cutter,
}

/// What reads the breaks in a form's lines.
enum Reader {
    Prose(Prose),
    Code(Code),
    Rows(Rows),
    Json(Json),
}

impl Text {
    pub fn new(form: Form, settings: &Settings) -> Self {
        let (reader, top) = match form {
            Form::Prose => (Reader::Prose(Prose::new(false)), prose::PARAGRAPH),
            Form::Markdown => (Reader::Prose(Prose::new(true)), prose::HEADING),
            Form::Code => (Reader::Code(Code::new(code::Style::Code)), code::OUTER),
            Form::Yaml => (Reader::Code(Code::new(code::Style::Yaml)), code::OUTER),
            Form::Toml => (Reader::Code(Code::new(code::Style::Toml)), code::OUTER),
            Form::Rows => (Reader::Rows(Rows::default()), records::ROW),
            Form::Json => (Reader::Json(Json::new(1)), records::RECORD),
            Form::JsonLines => (Reader::Json(Json::new(0)), records::RECORD),
        };
        Self { form: reader, cutter: Cutter::new(settings, top) }
    }

    /// A line, or a part of a long one (`ended`: the line ends with it).
    pub fn line(&mut self, text: &str, ended: bool) {
        let cutter = &mut self.cutter;
        match &mut self.form {
            Reader::Prose(prose) => prose.line(text, ended, cutter),
            Reader::Code(code) => code.line(text, ended, cutter),
            Reader::Rows(rows) => rows.line(text, ended, cutter),
            Reader::Json(json) => json.line(text, ended, cutter),
        }
    }

    /// The end of the text.
    pub fn finish(&mut self) {
        if let Reader::Prose(prose) = &mut self.form {
            prose.finish(&mut self.cutter);
        }
        self.cutter.finish();
    }

    /// The passages cut so far.
    pub fn ready(&mut self) -> std::vec::Drain<'_, Passage> {
        self.cutter.done.drain(..)
    }

    /// How many lines there were.
    pub fn lines(&self) -> usize {
        self.cutter.line
    }
}

/// A whole text of one form, cut into passages. Lines count from 1.
pub fn passages(text: &str, form: Form, settings: &Settings) -> Vec<Passage> {
    let mut cut = Text::new(form, settings);
    for line in text.lines() {
        cut.line(line, true);
    }
    cut.finish();
    cut.ready().collect()
}

/// A line, or a piece of one, kept until its part is written: its line, whether it starts that line, how strong the
/// break before it is, and its place among the bits (for what comes before what, where pieces share a line).
struct Bit {
    text: String,
    line: usize,
    starts: bool,
    strength: u8,
    order: usize,
}

/// Cuts text into passages at the breaks a form says it has, as its lines come in.
pub struct Cutter {
    target: usize,
    max: usize,
    /// The strongest break the form makes.
    top: u8,
    /// The line last pushed (from 1), whether it goes on, and how many bits were pushed.
    line: usize,
    in_line: bool,
    pushed: usize,
    /// The part being read: its bits, its length as text, and the strength of the breaks that end it (0 until the
    /// text starts).
    part: Vec<Bit>,
    length: usize,
    level: u8,
    /// The parts being read cut up, outermost first: the strength of the breaks that end each, and the line its
    /// pieces are read with, with the order of its bit. The outermost is the whole text, which no break ends.
    cut_up: Vec<(u8, Option<(usize, String)>)>,
    /// The passage being joined from parts.
    open: Option<Passage>,
    done: Vec<Passage>,
}

impl Cutter {
    /// A cutter for a form whose strongest break is `top`.
    pub fn new(settings: &Settings, top: u8) -> Self {
        let max = settings.max_passage_chars.max(1);
        let target = settings.passage_chars.min(max);
        Self {
            target,
            max,
            top,
            line: 0,
            in_line: false,
            pushed: 0,
            part: Vec::new(),
            length: 0,
            level: 0,
            cut_up: vec![(top + 1, None)],
            open: None,
            done: Vec::new(),
        }
    }

    /// The line last pushed is one every passage after it is read with: a table's header row.
    pub fn header(&mut self, text: &str) {
        self.cut_up[0].1 = Some((self.pushed, title(text)));
    }

    /// A line, or a piece of one (`ended`: the line ends with it), and how strong the break before it is (0: none).
    pub fn push(&mut self, text: &str, ended: bool, strength: u8) {
        let starts = !std::mem::replace(&mut self.in_line, !ended);
        if starts {
            self.line += 1;
        }
        // Lines are kept without their trailing spaces: only a line's last piece has them taken off.
        let text = if ended { text.trim_end() } else { text };
        self.pushed += 1;
        let bit = Bit {
            text: text.to_string(),
            line: self.line,
            starts,
            strength: strength.min(self.top),
            order: self.pushed,
        };
        if self.level > 0 {
            return self.take(bit);
        }
        // The text's first line says what its first parts are. Before a break at the top, they're never whole: text
        // before a document's first heading is read as paragraphs.
        if !text.trim().is_empty() {
            let level = bit.strength.clamp(1, self.top);
            self.cut_up.extend((level + 1..=self.top).rev().map(|level| (level, None)));
            self.level = level;
        }
        self.add(bit);
        self.fit();
    }

    /// The end of the text.
    pub fn finish(&mut self) {
        self.end_part(self.top);
        self.done.extend(self.open.take());
    }

    /// A bit, which ends the part before it when its break is strong enough.
    fn take(&mut self, bit: Bit) {
        if bit.strength >= self.level {
            self.end_part(bit.strength);
        }
        self.add(bit);
        self.fit();
    }

    fn add(&mut self, bit: Bit) {
        self.length += bit.text.len() + usize::from(bit.starts && !self.part.is_empty());
        match self.part.last_mut() {
            // A piece of the same line with no break before it goes with the one before.
            Some(last) if !bit.starts && bit.strength == 0 => last.text.push_str(&bit.text),
            _ => self.part.push(bit),
        }
    }

    /// The part ends at a break of `strength`, and so does each part being cut up that a break that strong ends.
    fn end_part(&mut self, strength: u8) {
        let bits = std::mem::take(&mut self.part);
        self.length = 0;
        if let Some(passage) = self.passage(&bits) {
            self.join(passage);
        }
        let mut ended = false;
        while self.cut_up.last().is_some_and(|(level, _)| *level <= strength) {
            self.cut_up.pop();
            ended = true;
        }
        if ended {
            // The pieces of a part that was cut up aren't joined with what follows it.
            self.done.extend(self.open.take());
        }
        self.level = self.cut_up.last().map_or(self.top, |(level, _)| level - 1);
    }

    /// Cuts the part while it's longer than the maximum: at its next weaker breaks, or when it has none (a paragraph,
    /// a row), into pieces of about the passage size.
    fn fit(&mut self) {
        while self.length > self.max {
            if self.level > 1 {
                self.cut_up_part();
            } else {
                self.cut_pieces();
            }
        }
    }

    /// The part, too long to be whole, is read again as its parts at the next weaker breaks, with its first line that
    /// says something (not a lone bracket) as their context.
    fn cut_up_part(&mut self) {
        // Nor with what comes before it.
        self.done.extend(self.open.take());
        let bits = std::mem::take(&mut self.part);
        self.length = 0;
        let first = bits
            .iter()
            .find(|bit| bit.text.chars().any(char::is_alphanumeric))
            .map(|bit| (bit.order, title(&bit.text)));
        // A part cut up again at a still weaker break has the same first line: it's context once.
        let known = self.cut_up.iter().any(|(_, title)| title.as_ref().map(|t| t.0) == first.as_ref().map(|f| f.0));
        self.cut_up.push((self.level, if known { None } else { first }));
        self.level -= 1;
        // Its first line starts the first of its parts: the break before it ended the part before.
        let mut bits = bits.into_iter();
        for bit in bits.by_ref() {
            let blank = bit.text.trim().is_empty();
            self.add(bit);
            if !blank {
                break;
            }
        }
        for bit in bits {
            self.take(bit);
        }
    }

    /// Cuts pieces at most the passage size long off the front of the part, until what's left fits the maximum: no
    /// piece is a scrap.
    fn cut_pieces(&mut self) {
        let mut bits = std::mem::take(&mut self.part);
        // Blank lines the text starts with aren't a piece's.
        bits.drain(..bits.iter().position(|bit| !bit.text.trim().is_empty()).unwrap_or(0));
        let mut whole = joined(&bits);
        let (mut line, mut order) = (bits[0].line, bits[0].order);
        while whole.len() > self.max {
            let cut = cut_point(&whole, self.target);
            let rest = whole.split_off(cut);
            let piece = std::mem::replace(&mut whole, rest);
            let lines = piece.matches('\n').count();
            if !piece.trim().is_empty() {
                let context = self.context(order);
                self.join(Passage {
                    text: piece.trim().to_string(),
                    first_line: line,
                    last_line: line + lines,
                    context,
                });
            }
            // What's left comes after the part's first line.
            (line, order) = (line + lines, order + 1);
            whole = whole.trim_start_matches([' ', '\t']).to_string();
        }
        self.length = whole.len();
        self.part = vec![Bit { text: whole, line, starts: true, strength: 0, order }];
    }

    /// A part as a passage, without the blank lines around it.
    fn passage(&self, bits: &[Bit]) -> Option<Passage> {
        let blank = |bit: &Bit| bit.text.trim().is_empty();
        let first = bits.iter().position(|bit| !blank(bit))?;
        let last = bits.iter().rposition(|bit| !blank(bit))?;
        Some(Passage {
            text: joined(&bits[first..=last]).trim_end().to_string(),
            first_line: bits[first].line,
            last_line: bits[last].line,
            context: self.context(bits[first].order),
        })
    }

    /// What a passage starting with the bit of this order is read with: the first lines of the parts it was cut from,
    /// where they come before it.
    fn context(&self, order: usize) -> String {
        let titles: Vec<&str> = self
            .cut_up
            .iter()
            .filter_map(|(_, title)| title.as_ref())
            .filter(|(at, _)| *at < order)
            .map(|(_, title)| title.as_str())
            .collect();
        titles.join("\n")
    }

    /// Adds a part (or a piece of one) to the open passage, or starts a new one when it doesn't fit. Parts are joined
    /// by as many line breaks as are between them in the file, so a match's line is the passage's first line plus its
    /// line within the passage; pieces of one line by a space.
    fn join(&mut self, passage: Passage) {
        match self.open.as_mut() {
            Some(open) => {
                let gap = passage.first_line.saturating_sub(open.last_line);
                if open.text.len() + gap.max(1) + passage.text.len() <= self.target {
                    open.text.push_str(&if gap == 0 { " ".to_string() } else { "\n".repeat(gap) });
                    open.text.push_str(&passage.text);
                    open.last_line = passage.last_line;
                } else {
                    self.done.push(std::mem::replace(open, passage));
                }
            }
            None => self.open = Some(passage),
        }
    }
}

/// Bits as text: the lines they start on their own lines.
fn joined(bits: &[Bit]) -> String {
    let mut text = String::new();
    for bit in bits {
        if bit.starts && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&bit.text);
    }
    text
}

/// A line as context: trimmed, and at most about 200 characters.
fn title(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(200) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
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
        super::passages(text, Form::Prose, &Settings::BASE)
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
        assert_eq!(super::passages("één", Form::Prose, &tiny).len(), 3);
    }

    #[test]
    fn a_part_that_fits_stays_whole_and_a_longer_one_is_cut_with_its_first_line_as_context() {
        let settings = Settings { passage_chars: 100, max_passage_chars: 200, ..Settings::BASE };
        // Three sections: one short, one between the passage size and the maximum, one longer than the maximum.
        let long = (1..=8).map(|n| format!("Paragraph {n} of the long section.")).collect::<Vec<_>>().join("\n\n");
        let text = format!("# Short\nA line.\n\n# Middle\n{}\n\n# Long\n{long}\n", "word ".repeat(30).trim());
        let cut = super::passages(&text, Form::Markdown, &settings);
        assert_eq!(cut[0].text, "# Short\nA line.");
        assert!(cut[1].text.starts_with("# Middle\n") && cut[1].text.len() > 100);
        assert!(cut[2].text.starts_with("# Long\n") && cut[2].context.is_empty());
        assert!(cut.len() > 3 && cut[3..].iter().all(|p| p.context == "# Long" && p.text.starts_with("Paragraph")));
        assert!(cut.iter().all(|p| p.text.len() <= 200));
    }
}
