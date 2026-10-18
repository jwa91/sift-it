//! Speech: what's said, written down sentence by sentence with their times, joined into passages. A short sentence
//! joins what follows closely, so each passage holds enough to mean something.

use crate::adapters::speech::Spoken;

/// Sentences shorter than this are joined to the next one (when it follows within MAX_GAP seconds).
const MIN_PASSAGE: usize = 160;
const MAX_GAP: f64 = 4.0;

/// Sentences joined into passages.
pub fn passages(spoken: Vec<Spoken>) -> Vec<Spoken> {
    let mut passages: Vec<Spoken> = Vec::new();
    for sentence in spoken {
        match passages.last_mut() {
            Some(last) if last.text.len() < MIN_PASSAGE && sentence.start - last.end <= MAX_GAP => {
                last.text.push(' ');
                last.text.push_str(&sentence.text);
                last.end = sentence.end;
            }
            _ => passages.push(sentence),
        }
    }
    passages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spoken(start: f64, end: f64, text: &str) -> Spoken {
        Spoken { start, end, text: text.into() }
    }

    #[test]
    fn short_sentences_join_what_follows_closely() {
        let joined = passages(vec![
            spoken(0.0, 2.0, "Hi."),
            spoken(2.5, 9.0, "Today we talk about Debian."),
            spoken(30.0, 32.0, "After a long pause."),
            spoken(32.5, 60.0, &"A long sentence about installing Ubuntu on a laptop. ".repeat(4)),
        ]);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].text, "Hi. Today we talk about Debian.");
        assert_eq!((joined[0].start, joined[0].end), (0.0, 9.0));
        assert_eq!(joined[1].start, 30.0);
    }
}
