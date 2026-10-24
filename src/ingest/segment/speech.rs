//! Speech: what's said, written down word by word with their times, cut into passages where sentences end. A passage
//! takes sentences until it holds enough to mean something; a pause ends it sooner, and with no sentence ending in
//! sight it ends at a length.

use crate::adapters::speech::Spoken;

/// A passage ends with the first sentence that brings it to MIN_PASSAGE characters, at MAX_PASSAGE without one, or
/// where a pause of more than MAX_GAP seconds comes.
const MIN_PASSAGE: usize = 160;
const MAX_PASSAGE: usize = 600;
const MAX_GAP: f64 = 4.0;

/// Words joined into passages.
pub fn passages(words: Vec<Spoken>) -> Vec<Spoken> {
    let mut passages: Vec<Spoken> = Vec::new();
    for word in words {
        match passages.last_mut() {
            Some(last) if !full(last) && word.start - last.end <= MAX_GAP => {
                last.text.push(' ');
                last.text.push_str(&word.text);
                last.end = word.end;
            }
            _ => passages.push(word),
        }
    }
    passages
}

/// Whether a passage takes no more words.
fn full(passage: &Spoken) -> bool {
    let length = passage.text.len();
    length >= MAX_PASSAGE || length >= MIN_PASSAGE && passage.text.ends_with(['.', '?', '!', '…'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words of `text`, a third of a second each from `start`.
    fn said(start: f64, text: &str) -> Vec<Spoken> {
        let at = |i: usize| start + i as f64 / 3.0;
        text.split(' ')
            .enumerate()
            .map(|(i, word)| Spoken { start: at(i), end: at(i + 1), text: word.into() })
            .collect()
    }

    #[test]
    fn passages_end_with_the_sentence_that_makes_them_long_enough() {
        let mut words = said(0.0, "Hi. Today we talk about Debian, and how to install it on an old laptop.");
        words.extend(said(30.0, "After a long pause, a new passage."));
        let sentence = "Debian runs well on a laptop that sat in a drawer for years, as it turns out.";
        words.extend(said(33.0, &[sentence; 3].join(" ")));
        let passages = passages(words);
        let texts: Vec<&str> = passages.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Hi. Today we talk about Debian, and how to install it on an old laptop.",
                &format!("After a long pause, a new passage. {sentence} {sentence}"),
                sentence,
            ]
        );
        assert_eq!((passages[0].start, passages[1].start), (0.0, 30.0));
        assert!(passages[1].end > 33.0);
    }

    #[test]
    fn speech_without_sentences_is_cut_at_a_length() {
        let passages = passages(said(0.0, &["and then"; 100].join(" ")));
        // The word that reaches the length is the last.
        assert_eq!(passages.len(), 2);
        assert!((MAX_PASSAGE..MAX_PASSAGE + 5).contains(&passages[0].text.len()));
    }
}
