//! Code: definitions, found by their indentation, so any language reads without a parser. A line at the outer level
//! starts a definition (a function, a class, an impl block), with the comments and attributes right above it, and a
//! closing bracket there ends one. A definition too long to be whole is cut at its inner definitions (the next level
//! in), then at blank lines. YAML reads the same way, its keys and items by their indentation, and TOML by its tables,
//! then their keys.

use super::passages::Cutter;

/// The breaks: a definition at the outer level, one a level in, and a blank line.
pub const OUTER: u8 = 3;
pub const INNER: u8 = 2;
pub const BLOCK: u8 = 1;

pub enum Style {
    Code,
    Yaml,
    Toml,
}

pub struct Code {
    style: Style,
    /// The last line goes on: it came in parts.
    in_line: bool,
    /// The indentation of the blocks the last line is in, outermost first.
    indents: Vec<usize>,
    /// A blank line came before this one.
    blank: bool,
    /// The level of the comment or attribute right above: the next line at that level stays with it.
    lead: Option<usize>,
    /// Inside a /* */ comment.
    comment: bool,
}

impl Code {
    pub fn new(style: Style) -> Self {
        Self { style, in_line: false, indents: Vec::new(), blank: false, lead: None, comment: false }
    }

    pub fn line(&mut self, text: &str, ended: bool, cutter: &mut Cutter) {
        if std::mem::replace(&mut self.in_line, !ended) {
            return cutter.push(text, ended, 0);
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.blank = true;
            self.lead = None;
            return cutter.push(text, ended, 0);
        }
        if self.comment {
            self.comment = !trimmed.contains("*/");
            self.blank = false;
            return cutter.push(text, ended, 0);
        }
        let level = self.level(text);
        let lead = self.is_lead(trimmed);
        let strength = if self.closes(trimmed) || self.lead == Some(level) {
            0
        } else {
            match level {
                0 if matches!(self.style, Style::Toml) && !trimmed.starts_with('[') => INNER,
                0 => OUTER,
                1 => INNER,
                _ if self.blank => BLOCK,
                _ => 0,
            }
        };
        self.lead = lead.then_some(level);
        self.blank = false;
        self.comment = trimmed.starts_with("/*") && !trimmed.contains("*/");
        cutter.push(text, ended, strength);
    }

    /// How many blocks in a line is, by its indentation (a tab counts as four spaces).
    fn level(&mut self, text: &str) -> usize {
        let width: usize = text.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum();
        while self.indents.last().is_some_and(|&indent| indent > width) {
            self.indents.pop();
        }
        if self.indents.last().is_none_or(|&indent| indent < width) {
            self.indents.push(width);
        }
        self.indents.len() - 1
    }

    /// A comment or an attribute: it goes with the line under it.
    fn is_lead(&self, line: &str) -> bool {
        match self.style {
            Style::Code => {
                ["//", "/*", "*", "#", "--", "@", ";", "%", "\"\"\"", "'''"].iter().any(|p| line.starts_with(p))
            }
            Style::Yaml | Style::Toml => line.starts_with('#'),
        }
    }

    /// A line that closes what's above it: a bracket, a tag, or a word some languages end blocks with.
    fn closes(&self, line: &str) -> bool {
        if line.starts_with(['}', ')', ']']) {
            return true;
        }
        if !matches!(self.style, Style::Code) {
            return false;
        }
        let word = line.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or_default();
        line.starts_with("</")
            || (["end", "fi", "done", "esac"].contains(&word) && !line[word.len()..].starts_with(':'))
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
    fn a_definition_is_a_passage_with_the_comments_above_it() {
        let text = "use std::io;\n\n/// Adds.\n#[inline]\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
                    // Subtracts.\nfn sub(\n    a: i32,\n) -> i32 {\n    -a\n}\n";
        let cut = cut(text, Form::Code, 40, 200);
        let firsts: Vec<usize> = cut.iter().map(|p| p.first_line).collect();
        assert_eq!(firsts, [1, 3, 9]);
        assert!(cut[1].text.starts_with("/// Adds.\n#[inline]\nfn add") && cut[1].text.ends_with('}'));
        assert_eq!(cut[2].last_line, 14);
    }

    #[test]
    fn a_long_definition_is_cut_at_its_inner_ones_with_its_first_line_as_context() {
        let methods: String = (1..=6).map(|n| format!("    def m{n}(self):\n        return {n}\n\n")).collect();
        let text = format!("class Numbers:\n    \"\"\"Small numbers.\"\"\"\n\n{methods}");
        let cut = cut(&text, Form::Code, 60, 120);
        assert!(cut[0].text.starts_with("class Numbers:") && cut[0].context.is_empty());
        assert!(cut[1..].iter().all(|p| p.text.starts_with("    def m") && p.context == "class Numbers:"));
        assert!(cut.iter().all(|p| p.text.len() <= 120));
    }

    #[test]
    fn toml_is_cut_at_its_tables_and_yaml_at_its_keys() {
        let toml = "# The app.\nname = \"x\"\n\n[server]\nport = 8080\nhosts = [\n  \"a\",\n]\n\n[db]\nurl = \"y\"\n";
        let firsts: Vec<usize> = cut(toml, Form::Toml, 10, 40).iter().map(|p| p.first_line).collect();
        assert_eq!(firsts, [1, 4, 10]);
        let yaml = "# Plants.\nbasil:\n  water: daily\n  sun: full\nmint:\n  water: weekly\n";
        let firsts: Vec<usize> = cut(yaml, Form::Yaml, 10, 60).iter().map(|p| p.first_line).collect();
        assert_eq!(firsts, [1, 5]);
    }
}
