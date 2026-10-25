//! `sift-it estimate <folder>...`: what indexing a folder means, before it starts: how many files of each kind, what
//! each round has to do and about how long it takes (ingest/discover/cost.rs), at the depth each file is read at, and
//! about how long it all takes, with the rounds side by side as the plan at `indexing.memory_mb` has them
//! (resources::Plan).

use crate::config::Config;
use crate::ingest::discover::{crawl, shares};
use crate::resources::{Conditions, Plan, Power, Pressure, Queue, Tool};
use protocol::Round;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct Estimate {
    pub folder: PathBuf,
    pub files: usize,
    /// What each round with work has to do, in order.
    pub rounds: Vec<RoundEstimate>,
    /// About how long it all takes, in seconds: the rounds on different chips side by side, as the plan has them.
    pub seconds: u64,
    pub bytes: u64,
    pub by_kind: BTreeMap<&'static str, usize>,
    /// The folder can't be read (missing, or macOS privacy settings).
    pub unreadable: bool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RoundEstimate {
    pub round: Round,
    /// How much it has, in its unit: files, passages, pictures (a video's frames looked at), seconds of sound.
    pub total: u64,
    pub unit: &'static str,
    /// About how long it takes with its chip to itself.
    pub seconds: u64,
}

pub fn estimate(config: &Config, folder: PathBuf) -> Estimate {
    let mut single = config.clone();
    single.sources.paths = vec![folder.clone()];
    let crawled = crawl::crawl(&single);
    let mut by_kind = BTreeMap::new();
    let mut totals: BTreeMap<Round, (f64, f64)> = BTreeMap::new();
    let mut bytes = 0;
    for file in &crawled.found {
        bytes += file.size;
        *by_kind.entry(file.kind.as_str()).or_default() += 1;
        for share in shares(file) {
            let (total, seconds) = totals.entry(share.round).or_default();
            *total += share.total;
            *seconds += share.seconds;
        }
    }
    let work: Vec<(Round, f64)> = totals.iter().map(|(round, (_, seconds))| (*round, *seconds)).collect();
    let charger = Conditions { power: Power::Charger, pressure: Pressure::Normal };
    let seconds = side_by_side(&work, |queues, held| Plan::of(&config.indexing, charger, queues, &[], held));
    Estimate {
        folder,
        files: crawled.found.len(),
        rounds: totals
            .into_iter()
            .map(|(round, (total, seconds))| RoundEstimate {
                round,
                total: total.round() as u64,
                unit: round.unit(),
                seconds: seconds.round() as u64,
            })
            .collect(),
        seconds: seconds.round() as u64,
        bytes,
        by_kind,
        unreadable: !crawled.unavailable.is_empty(),
    }
}

/// How long the rounds' work takes (each round's seconds with its chip to itself), with the rounds that `plan` runs
/// side by side, made again each time a round is done, as a run does.
fn side_by_side(work: &[(Round, f64)], plan: impl Fn(&[Queue], &[Tool]) -> Plan) -> f64 {
    let mut left: BTreeMap<Round, f64> = work.iter().copied().filter(|(_, seconds)| *seconds > 0.0).collect();
    let (mut held, mut elapsed) = (Vec::new(), 0.0);
    while !left.is_empty() {
        let queues: Vec<Queue> = left.keys().map(|&round| Queue { round, new: false, old: true }).collect();
        let now = plan(&queues, &held);
        let speeds = speeds(&now);
        let Some(step) = speeds.iter().map(|(round, speed)| left[round] / speed).min_by(f64::total_cmp) else {
            break;
        };
        for (round, speed) in speeds {
            let rest = left[&round] - speed * step;
            if rest <= 1e-9 {
                left.remove(&round);
            } else {
                left.insert(round, rest);
            }
        }
        elapsed += step;
        held = now.tools;
    }
    elapsed
}

/// How fast each round the plan runs goes, against with its chip to itself. The GPU takes one round at a time, the
/// text round first (as the run does). The Neural Engine takes OCR and speech together, each slower: OCR took 0.94 s
/// a screenshot beside speech and 0.53 s alone, speech went 249 times as fast as the sound plays beside OCR and 269
/// alone. The words go as many files at once as the plan has readers for them.
fn speeds(plan: &Plan) -> Vec<(Round, f64)> {
    let runs: Vec<Round> = plan.runs.iter().map(|running| running.round).collect();
    let gpu = [Round::Text, Round::Pictures, Round::Sound].into_iter().find(|round| runs.contains(round));
    let shared = runs.contains(&Round::Ocr) && runs.contains(&Round::Speech);
    runs.iter()
        .filter_map(|&round| {
            let speed = match round {
                Round::Words => plan.lanes()[0] as f64,
                Round::Text | Round::Pictures | Round::Sound if Some(round) != gpu => return None,
                Round::Ocr if shared => 0.53 / 0.94,
                Round::Speech if shared => 249.0 / 269.0,
                _ => 1.0,
            };
            Some((round, speed))
        })
        .collect()
}

/// "about 12 minutes", "under a minute", "about 2 hours".
pub fn describe(seconds: f64) -> String {
    match seconds {
        s if s < 60.0 => "under a minute".into(),
        s if s < 90.0 => "about a minute".into(),
        s if s < 90.0 * 60.0 => format!("about {} minutes", (s / 60.0).round()),
        s => format!("about {:.0} hours", (s / 3600.0).round()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Budget, Indexing};
    use Round::*;

    /// How long `work` takes at `mb` MB.
    fn at(mb: u64, work: &[(Round, f64)]) -> f64 {
        let indexing = Indexing { memory_mb: Budget::Mb(mb), ..Indexing::default() };
        let charger = Conditions { power: Power::Charger, pressure: Pressure::Normal };
        side_by_side(work, |queues, held| Plan::of(&indexing, charger, queues, &[], held))
    }

    #[test]
    fn rounds_on_other_chips_run_side_by_side_as_the_budget_lets_them() {
        let work = [(Text, 10.0), (Pictures, 20.0), (Speech, 30.0)];
        // One tool at a time: one after the other.
        assert_eq!(at(1024, &work), 60.0);
        // The GPU's rounds one after the other, speech beside them.
        assert_eq!(at(8192, &work), 30.0);
        // OCR and speech share the Neural Engine: OCR goes at 0.56 of its speed until it's done, then speech alone.
        let shared = at(8192, &[(Ocr, 10.0), (Speech, 30.0)]);
        let ocr = 10.0 * 0.94 / 0.53;
        assert!((shared - (ocr + 30.0 - ocr * 249.0 / 269.0)).abs() < 1e-6, "{shared}");
    }

    #[test]
    fn an_estimate_says_what_each_round_has_to_do() {
        let estimate = Estimate {
            folder: "/Users/me/Documents".into(),
            files: 2,
            rounds: vec![
                RoundEstimate { round: Words, total: 2, unit: Words.unit(), seconds: 0 },
                RoundEstimate { round: Speech, total: 3600, unit: Speech.unit(), seconds: 16 },
            ],
            seconds: 16,
            bytes: 1234,
            by_kind: BTreeMap::from([("audio", 1), ("document", 1)]),
            unreadable: false,
        };
        assert_eq!(
            serde_json::to_string(&estimate).unwrap(),
            r#"{"folder":"/Users/me/Documents","files":2,"rounds":[{"round":"words","total":2,"unit":"files","seconds":0},{"round":"speech","total":3600,"unit":"seconds","seconds":16}],"seconds":16,"bytes":1234,"by_kind":{"audio":1,"document":1},"unreadable":false}"#
        );
    }
}
