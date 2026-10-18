//! Frames: which of a video's sampled frames are kept. A frame starts a new shot when its thumbnail (the 16 × 9 grey
//! one mac::signature makes) differs from the last one kept by at least `shot_change` (the mean difference, 0 to 255);
//! within one shot a frame is kept every `keep_every` seconds.

pub struct Shots {
    shot_change: f64,
    keep_every: f64,
    /// The last frame kept: its time and thumbnail.
    last: Option<(f64, Vec<u8>)>,
}

impl Shots {
    pub fn new(shot_change: f64, keep_every: f64) -> Self {
        Self { shot_change, keep_every, last: None }
    }

    /// Whether the frame at `at` seconds, with this thumbnail, is kept.
    pub fn keep(&mut self, at: f64, thumbnail: &[u8]) -> bool {
        let keep = match &self.last {
            None => true,
            Some((kept_at, kept)) => at - kept_at >= self.keep_every || difference(kept, thumbnail) >= self.shot_change,
        };
        if keep {
            self.last = Some((at, thumbnail.to_vec()));
        }
        keep
    }
}

fn difference(a: &[u8], b: &[u8]) -> f64 {
    let sum: u64 = a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum();
    sum as f64 / a.len().max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_kept_per_shot_and_every_so_often_within_one() {
        let mut shots = Shots::new(20.0, 10.0);
        let (dark, light) = ([10u8; 144], [200u8; 144]);
        let kept: Vec<f64> = [(0.0, dark), (2.0, dark), (4.0, light), (6.0, light), (14.0, light), (16.0, light)]
            .into_iter()
            .filter(|(at, thumbnail)| shots.keep(*at, thumbnail))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(kept, [0.0, 4.0, 14.0]);
    }
}
