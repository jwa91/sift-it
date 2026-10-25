//! How far each round is (protocol::RoundStatus): what's done of what's known, in the round's unit, what state it's in
//! by the plan, and how long the rest takes at the speed it went while it ran. No rate is fixed: a fanless Mac slows as
//! it warms, and a round measures that as it goes.

use crate::index::RoundTotals;
use crate::resources::{Plan, Wait};
use protocol::{Round, RoundState, RoundStatus};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A round's speed is known once it has run this long.
const MEASURED: Duration = Duration::from_secs(10);

/// What each round did while it ran.
#[derive(Default)]
pub struct Progress {
    meters: HashMap<Round, Meter>,
}

#[derive(Default)]
struct Meter {
    /// How long it ran, before `since`.
    ran: Duration,
    /// Since when it runs, while it does.
    since: Option<Instant>,
    /// How much it did meanwhile, in its unit.
    units: f64,
}

impl Progress {
    /// What runs now: the clocks of the rounds the plan runs go, the others' stop.
    pub fn follow(&mut self, plan: &Plan, now: Instant) {
        for round in Round::ALL {
            let meter = self.meters.entry(round).or_default();
            match (plan.runs(round).is_some(), meter.since) {
                (true, None) => meter.since = Some(now),
                (false, Some(since)) => {
                    meter.ran += now - since;
                    meter.since = None;
                }
                _ => {}
            }
        }
    }

    /// A round did `units` of its work.
    pub fn did(&mut self, round: Round, units: f64) {
        self.meters.entry(round).or_default().units += units;
    }

    /// How fast a round went while it ran, in its unit a second, once it has run a while.
    fn rate(&self, round: Round, now: Instant) -> Option<f64> {
        let meter = self.meters.get(&round)?;
        let ran = meter.ran + meter.since.map_or(Duration::ZERO, |since| now - since);
        (ran >= MEASURED && meter.units > 0.0).then(|| meter.units / ran.as_secs_f64())
    }

    /// How far each round with work in this index is.
    pub fn status(&self, totals: &[RoundTotals], plan: &Plan, now: Instant) -> Vec<RoundStatus> {
        statuses(totals, plan, |round| self.rate(round, now))
    }
}

/// How far each round is, with its state by the plan and the time it has left at `rate`.
pub fn statuses(totals: &[RoundTotals], plan: &Plan, rate: impl Fn(Round) -> Option<f64>) -> Vec<RoundStatus> {
    totals
        .iter()
        .filter(|totals| totals.amount > 0.0 || totals.files > 0)
        .map(|totals| {
            let state = state(totals, plan);
            let left = (totals.amount - totals.done).max(0.0);
            let seconds_left = match state {
                RoundState::Done => None,
                _ => rate(totals.round).map(|rate| left / rate),
            };
            RoundStatus {
                round: totals.round,
                state,
                done: totals.done,
                total: totals.amount,
                files: totals.files,
                seconds_left,
            }
        })
        .collect()
}

/// A round's state: done with nothing left; running when the plan runs it; else waiting for its turn, or for what
/// keeps it from running (a charger, memory, its model).
fn state(totals: &RoundTotals, plan: &Plan) -> RoundState {
    if totals.files == 0 {
        return RoundState::Done;
    }
    if plan.runs(totals.round).is_some() {
        return RoundState::Running;
    }
    match plan.waits.iter().find(|(round, _)| *round == totals.round).map(|(_, wait)| *wait) {
        Some(Wait::Turn) | None => RoundState::Queued,
        Some(_) => RoundState::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Running;

    fn totals(round: Round, done: f64, amount: f64, files: usize) -> RoundTotals {
        RoundTotals { round, done, amount, files, new: 0, failed: 0 }
    }

    #[test]
    fn each_round_says_how_far_it_is_and_what_it_waits_for() {
        let plan = Plan {
            runs: vec![Running { round: Round::Text, new_only: false }],
            waits: vec![(Round::Pictures, Wait::Turn), (Round::Speech, Wait::Charger)],
            ..Default::default()
        };
        let all = [
            totals(Round::Words, 628.0, 628.0, 0),
            totals(Round::Text, 600.0, 1000.0, 12),
            totals(Round::Pictures, 50.0, 500.0, 400),
            totals(Round::Speech, 0.0, 3600.0, 3),
        ];
        let rates = |round| (round == Round::Text).then_some(40.0);
        let status = statuses(&all, &plan, rates);
        let states: Vec<_> = status.iter().map(|status| status.state).collect();
        assert_eq!(states, [RoundState::Done, RoundState::Running, RoundState::Queued, RoundState::Waiting]);
        assert_eq!(status[1].seconds_left, Some(10.0));
        assert_eq!(status[2].seconds_left, None);
    }

    #[test]
    fn a_rate_is_what_was_done_while_it_ran() {
        let mut progress = Progress::default();
        let start = Instant::now();
        let text = Plan { runs: vec![Running { round: Round::Text, new_only: false }], ..Default::default() };
        progress.follow(&text, start);
        progress.did(Round::Text, 300.0);
        // Not yet measured.
        assert_eq!(progress.rate(Round::Text, start + Duration::from_secs(5)), None);
        progress.follow(&Plan::default(), start + Duration::from_secs(20));
        // Time it didn't run doesn't count.
        assert_eq!(progress.rate(Round::Text, start + Duration::from_secs(100)), Some(15.0));
    }
}
