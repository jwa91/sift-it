//! Presenting: a ranked file as a result. Of its segments that score about the same, it opens at the one with the
//! most of the query's words, and in text and code at the first line with one of them. Its excerpt is the text around
//! the first of them.

use super::rank::{Candidate, Ranked};
use crate::store::Store;
use anyhow::Result;
use protocol::{Hit, Kind, Segment};
use rusqlite::OptionalExtension;
use std::path::Path;

/// A ranked file as a result, at its segment where it opens (None when the segment is gone meanwhile).
pub fn result(store: &Store, file: &Ranked, words: &[String]) -> Result<Option<Hit>> {
    let chosen = choose(store, &file.segments, words)?;
    let Some(mut hit) = hit(store, chosen.segment, words)? else { return Ok(None) };
    hit.score = file.score;
    hit.lift = chosen.lift;
    hit.cosine = chosen.cosine;
    hit.meaning = file.meaning;
    hit.words = file.words;
    hit.strong = file.strong;
    Ok(Some(hit))
}

/// Of a file's segments that score about the same, the one with the most of the query's telling words.
fn choose<'a>(store: &Store, segments: &'a [Candidate], words: &[String]) -> Result<&'a Candidate> {
    if segments.len() == 1 || words.is_empty() {
        return Ok(&segments[0]);
    }
    let mut best = (&segments[0], 0usize);
    for candidate in segments {
        let text = store.segment_text(candidate.segment)?.unwrap_or_default().to_lowercase();
        let found = words.iter().filter(|word| telling(word)).filter(|word| text.contains(word.as_str())).count();
        if found > best.1 || (found == best.1 && candidate.score > best.0.score) {
            best = (candidate, found);
        }
    }
    Ok(best.0)
}

/// A word that tells passages apart: short ones ("the", "for") are in every passage; numbers count whatever their
/// length.
fn telling(word: &str) -> bool {
    word.chars().count() > 3 || word.chars().any(|c| c.is_numeric())
}

/// A segment as a result, unscored: its file, its place, and its excerpt around the words.
pub fn hit(store: &Store, segment_id: i64, words: &[String]) -> Result<Option<Hit>> {
    let mut statement = store.conn.prepare_cached(
        "SELECT f.path, f.kind, f.modified, f.size, f.info, s.segment, s.loc, s.loc_end, s.text
         FROM segments s JOIN files f ON f.id = s.file_id WHERE s.id = ?1",
    )?;
    let hit: Option<Hit> = statement
        .query_row([segment_id], |row| {
            let path: String = row.get(0)?;
            let text: Option<String> = row.get(8)?;
            Ok(Hit {
                name: Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                path,
                kind: Kind::parse(&row.get::<_, String>(1)?).unwrap_or(Kind::Plaintext),
                modified: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                info: row
                    .get::<_, Option<String>>(4)?
                    .and_then(|info| serde_json::from_str(&info).ok())
                    .unwrap_or(serde_json::Value::Null),
                segment: Segment::parse(&row.get::<_, String>(5)?).unwrap_or(Segment::Passage),
                segment_id,
                loc: row.get(6)?,
                loc_end: row.get(7)?,
                excerpt: text.map(|text| excerpt(&text, words)),
                score: 0.0,
                lift: None,
                cosine: None,
                meaning: false,
                words: false,
                strong: false,
            })
        })
        .optional()?;
    let Some(mut hit) = hit else { return Ok(None) };
    // In text and code, open at the first line with one of the query's telling words, not at the passage's top.
    // (A notebook's places are cells, not lines.)
    let notebook = hit.path.ends_with(".ipynb");
    if let (Segment::Passage, Kind::Code | Kind::Plaintext, Some(first), false) =
        (hit.segment, hit.kind, hit.loc, notebook)
        && let Some(offset) = focus_line(store, segment_id, words)?
    {
        hit.loc = Some(first + offset as f64);
    }
    Ok(Some(hit))
}

/// The first line of a segment (from 0) with one of the telling words.
fn focus_line(store: &Store, segment_id: i64, words: &[String]) -> Result<Option<usize>> {
    let telling: Vec<&String> = words.iter().filter(|word| telling(word)).collect();
    if telling.is_empty() {
        return Ok(None);
    }
    Ok(store.segment_text(segment_id)?.and_then(|text| {
        text.lines().position(|line| {
            let line = line.to_lowercase();
            telling.iter().any(|word| line.contains(word.as_str()))
        })
    }))
}

/// About 300 characters of the text, around the first of the query's words in it when there is one.
fn excerpt(text: &str, words: &[String]) -> String {
    const LENGTH: usize = 300;
    let text = text.trim();
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths; only trust a position that lands on a boundary in the original.
    let found = words
        .iter()
        .filter(|word| word.len() > 2)
        .filter_map(|word| lower.find(word.as_str()))
        .min()
        .filter(|&position| text.is_char_boundary(position));
    let start = match found {
        Some(position) if position > LENGTH / 3 => {
            let mut start = position - LENGTH / 3;
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            // Begin at a word (after the whole space character: some are more than one byte).
            text[start..]
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, c)| start + i + c.len_utf8())
                .unwrap_or(start)
        }
        _ => 0,
    };
    let mut end = (start + LENGTH).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(text[start..end].trim());
    if end < text.len() {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_survives_wide_spaces() {
        let text = format!("{}\u{3000}word\u{00A0}more {}", "あ".repeat(60), "pet ".repeat(50));
        assert!(excerpt(&text, &["pet".to_string()]).contains("pet"));
        let text = format!("{}\u{00A0}{} pet", "x".repeat(150), "y".repeat(40));
        assert!(excerpt(&text, &["pet".to_string()]).contains("pet"));
    }

    #[test]
    fn excerpt_centres_on_the_words() {
        let text = format!("{} the pet clause is here {}", "filler ".repeat(80), "more ".repeat(80));
        let excerpt = excerpt(&text, &["pet".to_string()]);
        assert!(excerpt.contains("pet clause"));
        assert!(excerpt.starts_with('…'));
    }
}
