//! Sound: 16 kHz mono cut into 30-second windows that overlap by 10 seconds. The model listens to 30 seconds at a
//! time; the overlap keeps a sentence at a window's edge whole in one of them. Silent windows are skipped.

use anyhow::Result;
use embedder::{AUDIO_RATE, AUDIO_WINDOW};

/// Windows start this far apart: 30 s windows overlapping by 10 s.
const HOP: usize = 20 * AUDIO_RATE;
/// A window quieter than this (RMS, full scale 1.0, about −50 dBFS) is silence.
pub const SILENCE: f32 = 0.003;

/// Cuts sound into windows as it's decoded, handing each over as (start in seconds, samples).
#[derive(Default)]
pub struct Windows {
    buffer: Vec<f32>,
    /// The sample number of buffer[0].
    start: usize,
    /// A window was cut, silent or not.
    cut: bool,
}

impl Windows {
    /// Windows of sound that starts `second` seconds into the file, a window's start: where an earlier item of the
    /// same file stopped (its windows came before).
    pub fn from(second: f64) -> Self {
        Self { start: (second * AUDIO_RATE as f64).round() as usize, cut: second > 0.0, ..Default::default() }
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
            window(self.start, &self.buffer[..AUDIO_WINDOW], on)?;
            self.buffer.drain(..HOP);
            self.start += HOP;
        }
        Ok(())
    }

    /// The end of the sound. The rest is a window on its own if nothing came before; else only when it holds a few
    /// seconds the last window didn't.
    pub fn finish(&mut self, on: &mut dyn FnMut(f64, &[f32]) -> Result<()>) -> Result<()> {
        let rest = std::mem::take(self);
        let uncovered = rest.buffer.len().saturating_sub(AUDIO_WINDOW - HOP);
        if (!rest.cut && rest.buffer.len() >= AUDIO_RATE) || (rest.cut && uncovered >= 3 * AUDIO_RATE) {
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
        let mut windows = Windows::default();
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
        let mut windows = Windows::from(40.0);
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
}
