//! Sound: 16 kHz mono cut into 30-second windows that overlap by 10 seconds. The model listens to 30 seconds at a
//! time; the overlap keeps a sentence at a window's edge whole in one of them. Silent windows are skipped, and where
//! speech was heard (the speech round comes first), a window is kept only every SPOKEN_EVERY seconds: what's said is
//! found by its passages, and its sound adds little to them. Music, nature and the stretches between what's said keep
//! every window.

use anyhow::Result;
use embedder::{AUDIO_RATE, AUDIO_WINDOW};

/// Windows start this far apart: 30 s windows overlapping by 10 s.
const HOP: usize = 20 * AUDIO_RATE;
/// A window quieter than this (RMS, full scale 1.0, about −50 dBFS) is silence.
pub const SILENCE: f32 = 0.003;

/// A window at least this much of which speech was heard in is spoken.
const SPOKEN: f64 = 0.5;

/// What's written down is speech, not singing, when its passages are this dense with words, half of them at least.
/// Measured: three podcasts and two narrated videos 2.25 to 3.39 words a second (medians), an opera aria whose singing
/// was written down as garbled French and Italian 1.29. In a language written without spaces between words it never
/// is, and every window is kept.
const SPEECH_WORDS_PER_SECOND: f64 = 1.8;

/// Where speech was heard in a file, from what was written down of it (each passage's start, end and words): the
/// passages' stretches, or none when it was sung.
pub fn speech(passages: &[(f64, f64, usize)]) -> Vec<(f64, f64)> {
    let mut rates: Vec<f64> = passages
        .iter()
        .filter(|(start, end, _)| end > start)
        .map(|(start, end, words)| *words as f64 / (end - start))
        .collect();
    rates.sort_by(f64::total_cmp);
    if rates.get(rates.len() / 2).is_none_or(|median| *median < SPEECH_WORDS_PER_SECOND) {
        return Vec::new();
    }
    passages.iter().map(|(start, end, _)| (*start, *end)).collect()
}

/// Spoken windows are kept this many seconds apart, at least.
pub const SPOKEN_EVERY: f64 = 120.0;

/// Cuts sound into windows as it's decoded, handing each over as (start in seconds, samples).
#[derive(Default)]
pub struct Windows {
    buffer: Vec<f32>,
    /// The sample number of buffer[0].
    start: usize,
    /// A window was cut, silent or not.
    cut: bool,
    /// Where speech was heard, (start, end) in seconds, in order.
    spoken: Vec<(f64, f64)>,
    /// Where the last spoken window kept starts.
    kept: Option<f64>,
}

impl Windows {
    /// Windows of sound that starts `second` seconds into the file, a window's start: where an earlier item of the
    /// same file stopped (its windows came before). `spoken` is where speech was heard in it.
    pub fn from(second: f64, spoken: Vec<(f64, f64)>) -> Self {
        let start = (second * AUDIO_RATE as f64).round() as usize;
        Self { start, cut: second > 0.0, spoken, ..Default::default() }
    }

    /// Where the next window starts, in seconds.
    pub fn next(&self) -> f64 {
        self.start as f64 / AUDIO_RATE as f64
    }

    /// More sound: the windows it fills.
    pub fn push(&mut self, samples: &[f32], on: &mut dyn FnMut(f64, &[f32]) -> Result<()>) -> Result<()> {
        self.buffer.extend_from_slice(samples);
        while self.buffer.len() >= AUDIO_WINDOW {
            self.cut = true;
            if self.keeps(self.start, AUDIO_WINDOW) {
                window(self.start, &self.buffer[..AUDIO_WINDOW], on)?;
            }
            self.buffer.drain(..HOP);
            self.start += HOP;
        }
        Ok(())
    }

    /// Whether the window of `length` samples from `start` is kept: it isn't spoken, or the last spoken one kept is
    /// SPOKEN_EVERY before it.
    fn keeps(&mut self, start: usize, length: usize) -> bool {
        let (from, to) = (start as f64 / AUDIO_RATE as f64, (start + length) as f64 / AUDIO_RATE as f64);
        let heard: f64 = self.spoken.iter().map(|(start, end)| (end.min(to) - start.max(from)).max(0.0)).sum();
        if heard < SPOKEN * (to - from) {
            return true;
        }
        if self.kept.is_some_and(|kept| from - kept < SPOKEN_EVERY) {
            return false;
        }
        self.kept = Some(from);
        true
    }

    /// The end of the sound. The rest is a window on its own if nothing came before; else only when it holds a few
    /// seconds the last window didn't.
    pub fn finish(&mut self, on: &mut dyn FnMut(f64, &[f32]) -> Result<()>) -> Result<()> {
        let mut rest = std::mem::take(self);
        let uncovered = rest.buffer.len().saturating_sub(AUDIO_WINDOW - HOP);
        let enough = (!rest.cut && rest.buffer.len() >= AUDIO_RATE) || (rest.cut && uncovered >= 3 * AUDIO_RATE);
        if enough && rest.keeps(rest.start, rest.buffer.len()) {
            window(rest.start, &rest.buffer, on)?;
        }
        Ok(())
    }
}

fn window(start: usize, samples: &[f32], on: &mut dyn FnMut(f64, &[f32]) -> Result<()>) -> Result<()> {
    if rms(samples) < SILENCE {
        return Ok(());
    }
    on(start as f64 / AUDIO_RATE as f64, samples)
}

/// How loud sound is: its root mean square.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The windows `seconds` of sound make, fed in chunks of a second: (start, seconds long).
    fn windows(sound: impl Fn(usize) -> f32, seconds: usize) -> Vec<(f64, f64)> {
        heard(sound, seconds, Vec::new())
    }

    /// The same, with speech heard in `spoken`.
    fn heard(sound: impl Fn(usize) -> f32, seconds: usize, spoken: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
        let mut windows = Windows::from(0.0, spoken);
        let mut cut = Vec::new();
        let mut on = |start: f64, samples: &[f32]| {
            cut.push((start, samples.len() as f64 / AUDIO_RATE as f64));
            Ok(())
        };
        for second in 0..seconds {
            let samples: Vec<f32> = (0..AUDIO_RATE).map(|i| sound(second * AUDIO_RATE + i)).collect();
            windows.push(&samples, &mut on).unwrap();
        }
        windows.finish(&mut on).unwrap();
        cut
    }

    #[test]
    fn sound_is_cut_into_overlapping_windows() {
        let loud = |_| 0.5;
        assert_eq!(windows(loud, 70), [(0.0, 30.0), (20.0, 30.0), (40.0, 30.0)]);
        // Five seconds the last window didn't hear are a window of their own.
        assert_eq!(windows(loud, 75)[3], (60.0, 15.0));
        assert_eq!(windows(loud, 5), [(0.0, 5.0)]);
    }

    #[test]
    fn windows_go_on_where_an_earlier_stretch_stopped() {
        let mut windows = Windows::from(40.0, Vec::new());
        let cut = std::cell::RefCell::new(Vec::new());
        let mut on = |start: f64, _: &[f32]| {
            cut.borrow_mut().push(start);
            Ok(())
        };
        windows.push(&vec![0.5; 55 * AUDIO_RATE], &mut on).unwrap();
        assert_eq!((cut.borrow().clone(), windows.next()), (vec![40.0, 60.0], 80.0));
        windows.finish(&mut on).unwrap();
        assert_eq!(*cut.borrow(), [40.0, 60.0, 80.0]);
    }

    #[test]
    fn silence_is_skipped() {
        assert!(windows(|_| 0.0, 70).is_empty());
        // Sound only after the first 40 seconds.
        let late = |i: usize| if i >= 40 * AUDIO_RATE { 0.5 } else { 0.0 };
        assert_eq!(windows(late, 70), [(20.0, 30.0), (40.0, 30.0)]);
    }

    #[test]
    fn speech_is_told_from_singing_by_its_words_a_second() {
        // Two passages of talk, 2.5 words a second, and one of 1 a second: talk.
        assert_eq!(speech(&[(0.0, 10.0, 25), (10.0, 20.0, 25), (20.0, 30.0, 10)]).len(), 3);
        // An aria: a word a second.
        assert!(speech(&[(0.0, 20.0, 20), (25.0, 40.0, 18)]).is_empty());
        assert!(speech(&[]).is_empty());
    }

    #[test]
    fn where_speech_was_heard_a_window_is_kept_every_two_minutes() {
        let loud = |_| 0.5;
        // Talk from 0 to 150 s, then music: the spoken windows two minutes apart, all the others.
        let starts: Vec<f64> = heard(loud, 230, vec![(0.0, 70.0), (71.0, 150.0)]).iter().map(|w| w.0).collect();
        assert_eq!(starts, [0.0, 120.0, 140.0, 160.0, 180.0, 200.0]);
        // A window less than half of which is speech isn't spoken.
        assert_eq!(heard(loud, 70, vec![(10.0, 20.0)]).len(), 3);
    }
}
