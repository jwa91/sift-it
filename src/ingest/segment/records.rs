//! Records: CSV and TSV at their rows, each passage read with the header row; JSON at the records of its outer list or
//! the keys of its outer object, and JSON Lines at its lines, whatever the line breaks (minified JSON too). A record
//! too long to be whole is cut at its own keys and items, then theirs.

use super::passages::Cutter;

/// The break before a row.
pub const ROW: u8 = 1;
/// The break before a record, and before the keys and items one and two levels in.
pub const RECORD: u8 = 3;

/// CSV and TSV.
#[derive(Default)]
pub struct Rows {
    /// The last line goes on: it came in parts.
    in_line: bool,
    /// Inside a quoted field, which can hold line breaks.
    quoted: bool,
    /// The header row was read.
    header: bool,
}

impl Rows {
    pub fn line(&mut self, text: &str, ended: bool, cutter: &mut Cutter) {
        let starts = !std::mem::replace(&mut self.in_line, !ended);
        let strength = if starts && !self.quoted && !text.trim().is_empty() { ROW } else { 0 };
        self.quoted ^= text.matches('"').count() % 2 == 1;
        cutter.push(text, ended, strength);
        if strength == ROW && !std::mem::replace(&mut self.header, true) {
            cutter.header(text);
        }
    }
}

/// JSON, and JSON Lines.
pub struct Json {
    /// The brackets a record is in: 1 in JSON (its outer list or object), 0 in JSON Lines.
    outer: usize,
    depth: usize,
    string: bool,
    escaped: bool,
    /// The break before the next value, once it starts.
    pending: u8,
    in_line: bool,
}

impl Json {
    pub fn new(outer: usize) -> Self {
        Self { outer, depth: 0, string: false, escaped: false, pending: RECORD, in_line: false }
    }

    pub fn line(&mut self, text: &str, ended: bool, cutter: &mut Cutter) {
        let starts = !std::mem::replace(&mut self.in_line, !ended);
        if starts && self.outer == 0 && self.depth == 0 && !self.string {
            self.pending = RECORD;
        }
        // The piece from `start` on, the break before it, and whether only its indentation has come yet (a break
        // there goes before the whole line).
        let (mut start, mut strength, mut leading) = (0, 0, starts);
        for (at, c) in text.char_indices() {
            if self.string {
                match c {
                    _ if self.escaped => self.escaped = false,
                    '\\' => self.escaped = true,
                    '"' => self.string = false,
                    _ => {}
                }
                continue;
            }
            if c.is_whitespace() {
                continue;
            }
            if self.pending > 0 && !matches!(c, '}' | ']' | ',' | ':') {
                // A value starts.
                if leading {
                    strength = self.pending;
                } else {
                    cutter.push(&text[start..at], false, strength);
                    (start, strength) = (at, self.pending);
                }
                self.pending = 0;
            }
            leading = false;
            match c {
                '"' => self.string = true,
                // A bracket stays with what's first inside it, and with what's last.
                '{' | '[' => {
                    self.depth += 1;
                    self.pending = 0;
                }
                '}' | ']' => {
                    self.depth = self.depth.saturating_sub(1);
                    self.pending = 0;
                }
                ',' => self.pending = self.strength(),
                _ => {}
            }
        }
        cutter.push(&text[start..], ended, strength);
    }

    /// The break before a value at this depth: a record's, then weaker the further in, none past two levels.
    fn strength(&self) -> u8 {
        match self.depth.checked_sub(self.outer) {
            Some(within) => RECORD.saturating_sub(within as u8),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::passages::{Form, Passage, passages};
    use crate::config::Settings;

    fn cut(text: &str, form: Form, passage: usize, max: usize) -> Vec<Passage> {
        passages(text, form, &Settings { passage_chars: passage, max_passage_chars: max, ..Settings::BASE })
    }

    #[test]
    fn rows_are_read_with_the_header() {
        let text = "name,city\nAda,London\n\"Grace\nHopper\",Arlington\nAlan,Wilmslow\n";
        let cut = cut(text, Form::Rows, 12, 30);
        let rows: Vec<(&str, &str)> = cut.iter().map(|p| (p.text.as_str(), p.context.as_str())).collect();
        assert_eq!(
            rows,
            [
                ("name,city", ""),
                ("Ada,London", "name,city"),
                ("\"Grace\nHopper\",Arlington", "name,city"),
                ("Alan,Wilmslow", "name,city")
            ]
        );
    }

    #[test]
    fn json_is_cut_at_its_records_however_it_is_laid_out() {
        let records = r#"[{"name": "Ada", "born": 1815}, {"name": "Grace", "born": 1906}, {"name": "Alan"}]"#;
        let pretty = "[\n  {\n    \"name\": \"Ada\",\n    \"born\": 1815\n  },\n  {\n    \"name\": \"Grace\",\n    \
                      \"born\": 1906\n  },\n  {\n    \"name\": \"Alan\"\n  }\n]\n";
        for text in [records, pretty] {
            let cut = cut(text, Form::Json, 30, 60);
            assert_eq!(cut.len(), 3, "{text}");
            assert!(cut[1].text.contains("Grace") && !cut[1].text.contains("Ada") && !cut[1].text.contains("Alan"));
        }
        // A line of each, and a key cut at its items when it's too long.
        let lines = "{\"a\": 1}\n{\"b\": [\"one\", \"two\", \"three\", \"four\"]}\n";
        let cut = cut(lines, Form::JsonLines, 10, 20);
        assert_eq!(cut[0].text, "{\"a\": 1}");
        assert!(cut[1..].iter().all(|p| p.first_line == 2 && p.text.len() <= 20));
        assert_eq!(cut[2].context, "{\"b\": [\"one\",");
    }
}
