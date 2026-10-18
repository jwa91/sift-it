//! Prose: paragraphs, and in Markdown and plain text the sections its headings start (`#` headings, and headings
//! underlined with `=` or `-`). A heading stays with what follows it, so no passage ends on one; a heading right under
//! another one stays with it too.

use super::passages::Cutter;

/// The breaks: a heading, which starts a section, and a paragraph, after a blank line.
pub const HEADING: u8 = 2;
pub const PARAGRAPH: u8 = 1;

pub struct Prose {
    /// Headings are read (else the text is paragraphs only: a page, a document).
    headings: bool,
    /// The last line goes on: it came in parts.
    in_line: bool,
    /// A blank line came before this one (or nothing did).
    blank: bool,
    /// Inside a fenced code block, where `#` starts a comment, not a heading.
    fenced: bool,
    /// The section so far is its heading: what follows stays with it.
    bare: bool,
    /// A paragraph's first line, held until the next line says whether it's a heading (underlined).
    held: Option<String>,
}

impl Prose {
    pub fn new(headings: bool) -> Self {
        Self { headings, in_line: false, blank: true, fenced: false, bare: false, held: None }
    }

    pub fn line(&mut self, text: &str, ended: bool, cutter: &mut Cutter) {
        if std::mem::replace(&mut self.in_line, !ended) {
            return cutter.push(text, ended, 0);
        }
        let trimmed = text.trim();
        if let Some(held) = self.held.take() {
            if is_underline(trimmed) {
                let strength = self.heading();
                cutter.push(&held, true, strength);
                return cutter.push(text, ended, 0);
            }
            let strength = self.paragraph();
            cutter.push(&held, true, strength);
        }
        if trimmed.is_empty() {
            self.blank = true;
            return cutter.push(text, ended, 0);
        }
        let starts_paragraph = std::mem::replace(&mut self.blank, false);
        if !self.headings {
            return cutter.push(text, ended, if starts_paragraph { PARAGRAPH } else { 0 });
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.fenced = !self.fenced;
        } else if !self.fenced && is_heading(text) {
            let strength = self.heading();
            return cutter.push(text, ended, strength);
        }
        if !starts_paragraph {
            self.bare = false;
            return cutter.push(text, ended, 0);
        }
        if ended && !self.fenced && trimmed.len() < 200 {
            // The line under it may make it a heading.
            self.held = Some(text.to_string());
            return;
        }
        let strength = self.paragraph();
        cutter.push(text, ended, strength);
    }

    /// The end of the text.
    pub fn finish(&mut self, cutter: &mut Cutter) {
        if let Some(held) = self.held.take() {
            let strength = self.paragraph();
            cutter.push(&held, true, strength);
        }
    }

    /// The break before a heading: none right under another heading.
    fn heading(&mut self) -> u8 {
        if std::mem::replace(&mut self.bare, true) { 0 } else { HEADING }
    }

    /// The break before a paragraph: none right under a heading.
    fn paragraph(&mut self) -> u8 {
        if std::mem::replace(&mut self.bare, false) { 0 } else { PARAGRAPH }
    }
}

/// A `#` heading: one to six of them, then a space or nothing.
fn is_heading(line: &str) -> bool {
    let line = line.trim_start();
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && line[hashes..].chars().next().is_none_or(|c| c == ' ' || c == '\t')
}

/// A line of `=` or `-` that underlines the heading above it.
fn is_underline(line: &str) -> bool {
    line.len() >= 3 && (line.bytes().all(|b| b == b'=') || line.bytes().all(|b| b == b'-'))
}

#[cfg(test)]
mod tests {
    use super::super::passages::{Form, Passage, passages};
    use crate::config::Settings;

    fn sections(text: &str) -> Vec<Passage> {
        let settings = Settings { passage_chars: 60, max_passage_chars: 120, ..Settings::BASE };
        passages(text, Form::Markdown, &settings)
    }

    #[test]
    fn sections_start_at_headings_and_keep_them() {
        let text = "Intro line.\n\n# One\n\nFirst section, with enough words to fill it.\n\n\
                    Two\n===\nSecond section, its heading underlined.\n\n## Three\n### Three a\nThird.";
        let texts: Vec<String> = sections(text).into_iter().map(|p| p.text).collect();
        assert_eq!(
            texts,
            [
                "Intro line.",
                "# One\n\nFirst section, with enough words to fill it.",
                "Two\n===\nSecond section, its heading underlined.",
                "## Three\n### Three a\nThird.",
            ]
        );
    }

    #[test]
    fn a_hash_in_a_code_block_or_without_a_space_starts_no_section() {
        let text = "# Setup\n\n```sh\n# install it\nbrew install x\n```\n\n#hashtag and more";
        assert_eq!(sections(text).len(), 1);
    }
}
