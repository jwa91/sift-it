//! What the terminal can show, and showing it: colour and bold, clickable links (OSC 8), and pictures inline (the kitty
//! graphics protocol in Ghostty and kitty, iTerm2's in iTerm2 and WezTerm). Anything a terminal can't do is left out,
//! never printed as garbage; output that isn't a terminal gets none of it.

use std::io::IsTerminal;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Images {
    None,
    Kitty,
    Iterm,
}

#[derive(Clone, Copy, Debug)]
pub struct Terminal {
    pub tty: bool,
    pub color: bool,
    pub links: bool,
    pub images: Images,
    pub width: usize,
}

impl Terminal {
    pub fn detect() -> Self {
        let tty = std::io::stdout().is_terminal();
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        let program = var("TERM_PROGRAM").to_lowercase();
        let term = var("TERM").to_lowercase();
        let ghostty = program == "ghostty" || term.contains("ghostty") || !var("GHOSTTY_RESOURCES_DIR").is_empty();
        let kitty = term.contains("kitty") || !var("KITTY_WINDOW_ID").is_empty();
        let iterm = program == "iterm.app" || !var("ITERM_SESSION_ID").is_empty();
        let wezterm = program == "wezterm" || !var("WEZTERM_EXECUTABLE").is_empty();
        let vscode = program == "vscode";
        let images = match var("SIFT_IT_IMAGES").as_str() {
            "0" | "off" | "no" => Images::None,
            _ if ghostty || kitty => Images::Kitty,
            _ if iterm || wezterm => Images::Iterm,
            _ => Images::None,
        };
        Self {
            tty,
            color: tty && std::env::var_os("NO_COLOR").is_none() && term != "dumb",
            links: tty && (ghostty || kitty || iterm || wezterm || vscode),
            images: if tty { images } else { Images::None },
            width: if tty { width().unwrap_or(100) } else { 100 },
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    /// The query's words in running text: bold yellow.
    pub fn mark(&self, text: &str) -> String {
        self.paint("1;33", text)
    }

    /// The stretch that matched, in a waveform: blue.
    pub fn accent(&self, text: &str) -> String {
        self.paint("34", text)
    }

    /// `text` as a link to the file (Cmd-click opens it, in terminals that do links).
    pub fn file_link(&self, text: &str, path: &Path) -> String {
        if !self.links {
            return text.to_string();
        }
        let url: String = format!("file://{}", path.display())
            .chars()
            .map(|c| if c == ' ' { "%20".to_string() } else { c.to_string() })
            .collect();
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    }

    /// A picture file, `rows` lines tall, at the start of the current line, followed by a line break. Nothing in a
    /// terminal that can't show pictures.
    pub fn picture(&self, path: &Path, rows: u32) -> Option<String> {
        use base64::Engine;
        let bytes = std::fs::read(path).ok()?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        match self.images {
            Images::None => None,
            Images::Kitty => {
                // PNG only (f=100), sent in chunks of 4096; the terminal keeps the aspect ratio for the given rows.
                let mut out = String::new();
                let chunks: Vec<&str> =
                    encoded.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
                for (i, chunk) in chunks.iter().enumerate() {
                    let more = if i + 1 < chunks.len() { 1 } else { 0 };
                    if i == 0 {
                        out.push_str(&format!("\x1b_Ga=T,f=100,q=2,r={rows},m={more};{chunk}\x1b\\"));
                    } else {
                        out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                    }
                }
                out.push('\n');
                Some(out)
            }
            Images::Iterm => Some(format!(
                "\x1b]1337;File=inline=1;height={rows};preserveAspectRatio=1;size={}:{encoded}\x07\n",
                bytes.len()
            )),
        }
    }
}

fn width() -> Option<usize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// Wraps text to `width` columns, at spaces where it can; at most `lines` lines (the last ends in … when cut).
pub fn wrap(text: &str, width: usize, lines: usize) -> Vec<String> {
    let width = width.max(20);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut cut = false;
    for word in words {
        let length = line.chars().count();
        if length > 0 && length + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
            if out.len() == lines {
                cut = true;
                break;
            }
        }
        if !line.is_empty() {
            line.push(' ');
        }
        // A word longer than the line: cut it.
        if word.chars().count() > width {
            line.push_str(&word.chars().take(width - 1).collect::<String>());
            line.push('…');
        } else {
            line.push_str(word);
        }
    }
    if !cut && !line.is_empty() {
        out.push(line);
    }
    if cut && let Some(last) = out.last_mut() {
        if last.chars().count() >= width {
            let kept: String = last.chars().take(width - 1).collect();
            *last = kept;
        }
        last.push('…');
    }
    out
}

/// The line with the query's words marked (a word counts when it starts with one of them: "pet" marks "pets").
pub fn mark_words(terminal: &Terminal, line: &str, words: &[String]) -> String {
    if words.is_empty() || !terminal.color {
        return line.to_string();
    }
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let lower = run.to_lowercase();
        let hit = words.iter().any(|w| w.chars().count() > 1 && lower.starts_with(w.as_str()));
        out.push_str(&if hit { terminal.mark(run) } else { run.clone() });
        run.clear();
    };
    for c in line.chars() {
        if c.is_alphanumeric() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Levels stretched from their quietest tenth to their loudest: eight block heights would flatten speech, which is loud
/// nearly throughout.
pub fn stretch(levels: &[f32]) -> Vec<f32> {
    let mut sorted = levels.to_vec();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted.get(sorted.len() / 10).copied().unwrap_or(0.0);
    let top = sorted.last().copied().unwrap_or(1.0);
    levels.iter().map(|l| ((l - floor) / (top - floor).max(1e-3)).clamp(0.0, 1.0)).collect()
}

/// A level from 0 to 1 as a block character, ▁ to █.
pub fn block(level: f32) -> char {
    ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][((level.clamp(0.0, 1.0) * 7.0).round() as usize).min(7)]
}

/// A waveform in block characters, one per level; the columns from `first` to `last` in the accent colour.
pub fn sparkline(terminal: &Terminal, levels: &[f32], highlight: Option<(usize, usize)>) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut lit = false;
    for (i, level) in levels.iter().enumerate() {
        let now = highlight.is_some_and(|(first, last)| i >= first && i <= last);
        if now != lit && !run.is_empty() {
            out.push_str(&if lit { terminal.accent(&run) } else { terminal.dim(&run) });
            run.clear();
        }
        lit = now;
        run.push(block(*level));
    }
    if !run.is_empty() {
        out.push_str(&if lit { terminal.accent(&run) } else { terminal.dim(&run) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Terminal {
        Terminal { tty: false, color: false, links: false, images: Images::None, width: 40 }
    }

    #[test]
    fn wraps_and_cuts() {
        let lines = wrap("one two three four five six seven eight nine ten eleven twelve", 20, 2);
        assert_eq!(lines, ["one two three four", "five six seven eigh…"]);
    }

    #[test]
    fn marks_words_by_prefix() {
        let terminal = Terminal { color: true, ..plain() };
        let marked = mark_words(&terminal, "Pets and a pet deposit.", &["pet".into()]);
        assert_eq!(marked.matches("\x1b[1;33m").count(), 2);
        assert_eq!(mark_words(&plain(), "Pets", &["pet".into()]), "Pets");
    }

    #[test]
    fn sparkline_has_a_block_per_level() {
        assert_eq!(sparkline(&plain(), &[0.0, 0.5, 1.0], Some((1, 1))), "▁▅█");
    }
}
