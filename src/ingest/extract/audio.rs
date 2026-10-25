//! Recordings, and a video's sound, through AVFoundation. Their words are their names and tags (title, artist, album),
//! with how long they are. For the sound round, the sound decoded to 16 kHz mono, in one go: every five minutes, what
//! it found is written, and it stops there when the run wants its lane. Only a recording that was stopped is read
//! from partway, since AVFoundation's seek in a VBR MP3 can land a fraction of a second off (docs/audio-decoding.md).

use crate::adapters::macos::av;
use crate::ingest::read::{Content, Ended, From};
use crate::ingest::rounds::{Found, SLICE};
use anyhow::{Result, anyhow};
use serde_json::json;
use std::path::Path;

/// The words round: its tags and length, and whether there's sound in it, for its other rounds.
pub fn words(path: &Path) -> Result<Ended> {
    let duration = av::duration(path);
    let mut ended = Ended::whole(json!({ "duration": duration, "tags": av::tags(path) }), 1.0);
    ended.found = Found { duration, sound: av::has_audio(path), ..Found::default() };
    Ok(ended)
}

/// Where an item of sound stops early, at a checkpoint: the next one goes on from there.
#[derive(Debug)]
struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the item stops here")
    }
}
impl std::error::Error for Stopped {}

/// The sound round: the windows that start from where it got to (`from.done` seconds) on, with a checkpoint every
/// SLICE seconds of them. Where speech was heard (`from.spoken`), fewer of them.
pub fn sound(path: &Path, from: &From, content: &mut Content) -> Result<Ended> {
    content.sound = crate::ingest::segment::sound::Windows::from(from.done, from.spoken.clone());
    let mut until = from.done + SLICE;
    let read = av::read_samples(path, from.done, &mut |samples| {
        content.sound(samples)?;
        if content.sound.next() >= until {
            if !content.checkpoint(content.sound.next())? {
                return Err(anyhow!(Stopped));
            }
            until += SLICE;
        }
        Ok(())
    });
    match read {
        Err(error) if error.is::<Stopped>() => {
            Ok(Ended { done: content.sound.next(), resume: Some(content.resume()), ..Ended::default() })
        }
        Err(error) => Err(error),
        Ok(seconds) => {
            content.end_sound()?;
            Ok(Ended::whole(json!({ "windows": content.windows }), seconds))
        }
    }
}
