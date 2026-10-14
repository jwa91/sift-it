//! `sift-it search` in a terminal: each result numbered (for `sift-it open <n>`), its name a link to the file, where
//! it matched, and a body that fits its kind: the passage with the query's words marked, the lines of code, a
//! recording's waveform with the matched stretch lit, a picture or video frame inline where the terminal shows pictures.

use crate::config;
use crate::crawl::Kind;
use crate::open::timestamp;
use crate::preview;
use crate::search::{Hit, Response, Searcher, words_of};
use crate::store::Segment;
use crate::term::{self, Images, Terminal};
use anyhow::Result;
use std::path::Path;

/// Pictures are shown this many lines tall.
const PICTURE_ROWS: u32 = 6;
/// Lines of a passage shown under a result.
const PASSAGE_LINES: usize = 3;
/// Lines of code shown under a result.
const CODE_LINES: usize = 5;

pub fn print(terminal: &Terminal, response: &Response, query: &str, searcher: &Searcher) -> Result<()> {
    let words = words_of(query.trim_start().strip_prefix("code:").unwrap_or(query));
    if response.hits.is_empty() {
        println!(
            "{}",
            terminal.dim(&format!(
                "Nothing stands out for “{}”. Describe it differently, or check the words in quotes.",
                query.trim()
            ))
        );
        return Ok(());
    }
    let mut weak_header = false;
    for (index, hit) in response.hits.iter().enumerate() {
        if !hit.strong && !weak_header {
            println!("{}", terminal.dim("── weaker matches ──"));
            println!();
            weak_header = true;
        }
        println!("{}", header(terminal, index + 1, hit));
        let body = body(terminal, hit, &words, searcher);
        print!("{body}");
        println!();
    }
    if !response.meaning {
        println!("{}", terminal.dim("Exact words only: the model isn't there (sift-it model download)."));
    }
    println!("{}", terminal.dim("↳ sift-it open <n> opens a result where it matched; --reveal shows it in Finder"));
    Ok(())
}

/// " 1  agreement.pdf · page 3                     ~/Documents/Housing"
fn header(terminal: &Terminal, number: usize, hit: &Hit) -> String {
    let path = Path::new(&hit.path);
    let place = place(hit);
    let left_plain =
        format!("{number:>2}  {}{}", hit.name, place.as_deref().map(|p| format!(" · {p}")).unwrap_or_default());
    let left = format!(
        "{}  {}{}",
        terminal.dim(&format!("{number:>2}")),
        terminal.file_link(&terminal.bold(&hit.name), path),
        place.map(|p| terminal.dim(&format!(" · {p}"))).unwrap_or_default()
    );
    let folder = folder(path);
    let room = terminal.width.saturating_sub(left_plain.chars().count() + 4);
    if room < 12 {
        return left;
    }
    let folder = if folder.chars().count() > room {
        format!("…{}", folder.chars().skip(folder.chars().count() - room + 1).collect::<String>())
    } else {
        folder
    };
    let gap = terminal.width.saturating_sub(left_plain.chars().count() + folder.chars().count()).max(2);
    format!("{left}{}{}", " ".repeat(gap.min(terminal.width)), terminal.dim(&folder))
}

/// Where in the file it matched, said shortly.
fn place(hit: &Hit) -> Option<String> {
    let notebook = hit.path.ends_with(".ipynb");
    match (hit.segment, hit.loc) {
        (Segment::Sound, Some(start)) => Some(match hit.loc_end {
            Some(end) => format!("{}–{}", timestamp(start), timestamp(end)),
            None => timestamp(start),
        }),
        (Segment::Frame, Some(at)) => Some(timestamp(at)),
        (Segment::Page | Segment::Ocr, Some(page)) if hit.kind == Kind::Pdf => Some(format!("page {page}")),
        (Segment::Picture, Some(page)) if hit.kind == Kind::Pdf => Some(format!("page {page}")),
        (Segment::Passage, Some(cell)) if notebook => Some(format!("cell {cell}")),
        (Segment::Passage, Some(line)) if line > 1.0 => Some(format!("line {line}")),
        _ => match hit.kind {
            Kind::Image => Some("picture".into()),
            Kind::Screenshot => Some("screenshot".into()),
            _ => None,
        },
    }
}

fn folder(path: &Path) -> String {
    let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let home = config::dirs_home().display().to_string();
    match folder.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => folder,
    }
}

const INDENT: &str = "    ";

fn push_line(out: &mut String, text: &str) {
    out.push_str(INDENT);
    out.push_str(text);
    out.push('\n');
}

/// A passage wrapped to the width, at most `lines` lines, with the query's words marked.
fn passage(terminal: &Terminal, text: &str, width: usize, lines: usize, words: &[String]) -> Vec<String> {
    term::wrap(text, width, lines).iter().map(|line| term::mark_words(terminal, line, words)).collect()
}

/// What goes under a result, already indented, ending in a line break.
fn body(terminal: &Terminal, hit: &Hit, words: &[String], searcher: &Searcher) -> String {
    let width = terminal.width.saturating_sub(INDENT.len() + 2).max(30);
    let mut out = String::new();
    let previews = config::previews_dir();
    let picture = |kind: &str, loc: Option<f64>| -> Option<String> {
        if terminal.images == Images::None {
            return None;
        }
        let thumbnail = preview::thumbnail(Path::new(&hit.path), kind, loc, 360, &previews).ok()?;
        terminal.picture(&thumbnail, PICTURE_ROWS).map(|p| format!("{INDENT}{p}"))
    };
    match hit.kind {
        Kind::Image | Kind::Screenshot => {
            if let Some(shown) = picture("image", None) {
                out.push_str(&shown);
            } else if let Some(about) = picture_facts(hit) {
                push_line(&mut out, &terminal.dim(&about));
            }
            let text = hit.excerpt.clone().or_else(|| {
                (hit.kind == Kind::Screenshot).then(|| searcher.file_text(&hit.path).ok().flatten()).flatten()
            });
            for text_line in text.map(|t| passage(terminal, &t, width, 2, words)).unwrap_or_default() {
                push_line(&mut out, &text_line);
            }
        }
        Kind::Audio => {
            if let Some(wave) = waveform(terminal, hit, width) {
                push_line(&mut out, &wave);
            }
        }
        Kind::Video => {
            if let Some(shown) = picture("video", hit.loc) {
                out.push_str(&shown);
            }
            match (hit.segment, hit.loc) {
                (Segment::Sound, _) => {
                    if let Some(wave) = waveform(terminal, hit, width) {
                        push_line(&mut out, &wave);
                    }
                }
                (_, Some(at)) => push_line(&mut out, &terminal.dim(&format!("seen at {}", timestamp(at)))),
                _ => {}
            }
        }
        Kind::Code => {
            let text = searcher.segment_text(hit.segment_id).ok().flatten().or_else(|| hit.excerpt.clone());
            for code_line in text.map(|t| code_lines(terminal, &t, hit.loc, words, width)).unwrap_or_default() {
                push_line(&mut out, &code_line);
            }
        }
        Kind::Text | Kind::Doc | Kind::Pdf => {
            for text_line in
                hit.excerpt.as_deref().map(|t| passage(terminal, t, width, PASSAGE_LINES, words)).unwrap_or_default()
            {
                push_line(&mut out, &text_line);
            }
        }
    }
    out
}

/// "3024 × 4032 · taken 2024-05-01", for a terminal that can't show the picture.
fn picture_facts(hit: &Hit) -> Option<String> {
    let width = hit.info.get("width")?.as_u64()?;
    let height = hit.info.get("height")?.as_u64()?;
    let taken = hit
        .info
        .get("taken")
        .and_then(|t| t.as_str())
        .map(|t| format!(" · taken {}", t.get(..10).unwrap_or(t).replace(':', "-")))
        .unwrap_or_default();
    Some(format!("{width} × {height}{taken}"))
}

/// "▂▃▅▇▆▃▂▁▂▅▇█▇▅▃   heard at 0:20–0:50 of 1:16"
fn waveform(terminal: &Terminal, hit: &Hit, width: usize) -> Option<String> {
    let path = Path::new(&hit.path);
    let columns = width.saturating_sub(34).clamp(16, 60);
    let levels = preview::levels(path, columns, &config::previews_dir()).ok()?;
    let levels = term::stretch(&levels);
    let start = hit.loc.unwrap_or(0.0);
    let highlight = preview::span(path, columns, start, hit.loc_end.unwrap_or(start + 30.0));
    let length = hit.info.get("duration").and_then(|d| d.as_f64()).map(|d| format!(" of {}", timestamp(d)));
    let heard = match hit.loc_end {
        Some(end) => format!("heard at {}–{}", timestamp(start), timestamp(end)),
        None => format!("heard at {}", timestamp(start)),
    };
    Some(format!(
        "{}   {}",
        term::sparkline(terminal, &levels, Some(highlight)),
        terminal.dim(&format!("{heard}{}", length.unwrap_or_default()))
    ))
}

/// A few lines of the code that matched, numbered, from the first line with the query's words.
fn code_lines(terminal: &Terminal, text: &str, loc: Option<f64>, words: &[String], width: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let telling = |line: &str| {
        let lower = line.to_lowercase();
        words.iter().any(|w| (w.chars().count() > 3 || w.chars().any(char::is_numeric)) && lower.contains(w.as_str()))
    };
    let focus = lines.iter().position(|l| telling(l)).unwrap_or(0);
    // The hit's line is the focus line (search moves it there); count the others from it.
    let first_number = loc.map(|l| l as i64 - focus as i64);
    lines
        .iter()
        .enumerate()
        .skip(focus)
        .take(CODE_LINES)
        .map(|(i, code)| {
            let number = first_number.map(|n| format!("{:>4} │ ", n + i as i64)).unwrap_or_default();
            let code: String = code.chars().take(width.saturating_sub(number.chars().count())).collect();
            format!("{}{}", terminal.dim(&number), term::mark_words(terminal, &code, words))
        })
        .collect()
}
