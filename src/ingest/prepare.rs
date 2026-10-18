//! Preparing: a segment as the model's input. Text is read with its file's title in the document prompt; code with its
//! path inside its project as the title, since the model reads "src/net/retry.rs" as much as the code. Text that's
//! mostly numbers isn't embedded: a vector of it means nothing. A picture is fitted to its token budget, and sound
//! goes as it is.

use super::Payload;
use super::discover::crawl::is_project;
use crate::adapters::macos;
use crate::config::Settings;
use anyhow::Result;
use embedder::imaging;
use objc2_core_graphics::CGImage;
use protocol::Kind;
use std::path::Path;

/// The title a file's text is embedded with: its file name without the extension, or for code its path inside its
/// project.
pub fn title(path: &Path, kind: Kind) -> String {
    if kind == Kind::Code {
        return project_path(path);
    }
    path.file_stem().map(|s| s.to_string_lossy().replace(['_', '-'], " ")).unwrap_or_default()
}

/// The path from the project's root (the nearest folder up that's a project, as the crawl tells), or the file name.
fn project_path(path: &Path) -> String {
    let mut folder = path.parent();
    while let Some(candidate) = folder {
        if is_project(candidate) {
            let root_name = candidate.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let inside = path.strip_prefix(candidate).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            return format!("{root_name}/{inside}");
        }
        folder = candidate.parent();
    }
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// A text, after its context (what it was cut from: a heading, a signature, a header row), in its document prompt, at
/// a depth that embeds; else it goes into the keyword index only.
pub fn text(title: &str, context: &str, text: &str, settings: &Settings) -> Payload {
    if !settings.embeds() || mostly_numbers(text) {
        return Payload::None;
    }
    if context.is_empty() {
        Payload::Text(embedder::prompts::document(Some(title), text))
    } else {
        Payload::Text(embedder::prompts::document(Some(title), &format!("{context}\n{text}")))
    }
}

/// A passage of what's said, in its document prompt (it's only written down at a depth that embeds).
pub fn spoken(title: &str, text: &str) -> String {
    embedder::prompts::document(Some(title), text)
}

/// A picture, fitted to `budget` tokens.
pub fn picture(image: &CGImage, budget: u32) -> Result<Payload> {
    Ok(Payload::Image(imaging::fit(&macos::pixels(image)?, budget)?))
}

/// A window of sound.
pub fn sound(samples: &[f32]) -> Payload {
    Payload::Audio(samples.to_vec())
}

/// Text where digits outnumber letters (a table, a log of figures): searched by its exact words only.
fn mostly_numbers(text: &str) -> bool {
    let (mut letters, mut digits) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_alphabetic() {
            letters += 1;
        } else if c.is_numeric() {
            digits += 1;
        }
    }
    digits > 20 && digits * 2 > letters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_detected() {
        assert!(mostly_numbers("2024-01-01,12.5,13.2,99\n2024-01-02,12.7,13.9,98\n2024-01-03,11.1,10.0,97"));
        assert!(!mostly_numbers("The meeting on 3 March 2024 moved to room 12."));
    }
}
