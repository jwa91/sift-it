//! Pictures with text on them: what a photo, a screenshot or a scanned page becomes. A photo is what it shows. A
//! screenshot is what it shows and its text: passages of its own when there's enough of it, else words with the
//! picture. A scanned page with enough text is that text; one with little is what it shows. At `words` depth, nothing
//! is shown to the model, and any text is passages.

/// Text read from a picture becomes passages of its own from this many characters on.
const MIN_TEXT: usize = 40;

/// What a picture with text read off it becomes.
#[derive(Debug, PartialEq)]
pub struct Cut {
    /// The picture is a segment: what it shows is embedded.
    pub picture: bool,
    /// The text goes with the picture, into the keyword index: there's some, too little for passages of its own.
    pub words: bool,
    /// The text is cut into passages of its own.
    pub passages: bool,
}

/// What a picture with `text` on it becomes, at a depth that `embeds` or not; `page` for a scanned page.
pub fn cut(text: &str, embeds: bool, page: bool) -> Cut {
    let long = long(text);
    let picture = embeds && !(page && long);
    Cut { picture, words: picture && !long && !text.trim().is_empty(), passages: long || !embeds }
}

/// Enough text for passages of its own.
pub fn long(text: &str) -> bool {
    text.chars().count() >= MIN_TEXT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_picture_with_text_becomes() {
        let (long, short) = ("a receipt: total due within thirty days, EUR 480", "OK");
        let cut = |picture, words, passages| Cut { picture, words, passages };
        // Photos and screenshots.
        assert_eq!(super::cut("", true, false), cut(true, false, false));
        assert_eq!(super::cut(short, true, false), cut(true, true, false));
        assert_eq!(super::cut(long, true, false), cut(true, false, true));
        assert_eq!(super::cut(short, false, false), cut(false, false, true));
        // Scanned pages.
        assert_eq!(super::cut(long, true, true), cut(false, false, true));
        assert_eq!(super::cut(short, true, true), cut(true, true, false));
        assert_eq!(super::cut(short, false, true), cut(false, false, true));
    }
}
