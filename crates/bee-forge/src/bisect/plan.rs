//! The bisection plan: a pure function of the candidates' known outcomes.
//!
//! Candidates are the landed changesets after the last green one, oldest first; the last
//! candidate is the failing changeset the bisect started from. The plan keeps the interval
//! `[lo, hi]` that must contain the first failing candidate (`hi` is always known to fail).
//! Each round probes up to `parallelism` untested candidates spaced evenly through
//! `[lo, hi)`: with one probe per round that is binary search (`⌈log2 n⌉` rounds for `n`
//! candidates), with `k` probes the interval shrinks by a factor of `k + 1` per round.
//! Candidates that cannot be tested (the probe could not run) are skipped, as `git bisect
//! skip` does; when only skipped candidates separate `lo` and `hi` the plan is stuck.
//!
//! Nothing here reads clocks, randomness or hash-map order, so the same outcomes always give
//! the same probes and the same culprit.

use std::collections::{BTreeMap, BTreeSet};

/// What a probe (or a manual mark) said about one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The check passed: the culprit is later.
    Pass,
    /// The check failed: the culprit is this candidate or earlier.
    Fail,
    /// The candidate could not be tested.
    Skip,
}

impl Outcome {
    /// Stored form.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Skip => "skip",
        }
    }

    /// Parses the stored form.
    pub fn parse(s: &str) -> Option<Outcome> {
        match s {
            "pass" => Some(Outcome::Pass),
            "fail" => Some(Outcome::Fail),
            "skip" => Some(Outcome::Skip),
            _ => None,
        }
    }
}

/// Where the plan stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// More probes are needed.
    Open,
    /// The first failing candidate (index).
    Found(usize),
    /// Outcomes contradict each other (a later candidate passed after an earlier one failed):
    /// the failure is flaky or depends on something outside the history.
    Contradiction(String),
    /// Only untestable candidates remain between the last pass and the first fail.
    Stuck {
        /// First candidate that may be the culprit.
        lo: usize,
        /// Last candidate that may be the culprit (known to fail).
        hi: usize,
    },
}

/// The bisection state over `n` candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    n: usize,
    lo: usize,
    hi: usize,
    outcomes: BTreeMap<usize, Outcome>,
    contradiction: Option<String>,
}

impl Plan {
    /// A plan over `n ≥ 1` candidates whose last one is known to fail.
    pub fn new(n: usize) -> Plan {
        let n = n.max(1);
        let mut outcomes = BTreeMap::new();
        outcomes.insert(n - 1, Outcome::Fail);
        Plan {
            n,
            lo: 0,
            hi: n - 1,
            outcomes,
            contradiction: None,
        }
    }

    /// Number of candidates.
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether there are no candidates (never: the failing changeset is one).
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The interval that contains the culprit.
    pub fn interval(&self) -> (usize, usize) {
        (self.lo, self.hi)
    }

    /// Known outcomes by candidate index.
    pub fn outcomes(&self) -> &BTreeMap<usize, Outcome> {
        &self.outcomes
    }

    /// Records an outcome for candidate `i` (out-of-range indexes are ignored).
    pub fn record(&mut self, i: usize, o: Outcome) {
        if i >= self.n {
            return;
        }
        match o {
            Outcome::Pass => {
                if i >= self.hi {
                    self.contradiction = Some(format!(
                        "candidate {} passed but candidate {} failed earlier in history",
                        i + 1,
                        self.hi + 1
                    ));
                } else {
                    self.lo = self.lo.max(i + 1);
                }
            }
            Outcome::Fail => {
                if i < self.lo {
                    self.contradiction = Some(format!(
                        "candidate {} failed but candidate {} passed later in history",
                        i + 1,
                        self.lo
                    ));
                } else {
                    self.hi = self.hi.min(i);
                }
            }
            Outcome::Skip => {}
        }
        self.outcomes.insert(i, o);
    }

    /// Where the plan stands, given probes still running on `pending`.
    pub fn status(&self, pending: &BTreeSet<usize>) -> Status {
        if let Some(c) = &self.contradiction {
            return Status::Contradiction(c.clone());
        }
        if self.lo == self.hi {
            return Status::Found(self.hi);
        }
        if self.untested(pending).is_empty()
            && !pending.iter().any(|p| (self.lo..self.hi).contains(p))
        {
            return Status::Stuck {
                lo: self.lo,
                hi: self.hi,
            };
        }
        Status::Open
    }

    fn untested(&self, pending: &BTreeSet<usize>) -> Vec<usize> {
        (self.lo..self.hi)
            .filter(|i| !self.outcomes.contains_key(i) && !pending.contains(i))
            .collect()
    }

    /// The next round: up to `parallelism` untested candidates spaced evenly through the
    /// interval (empty while the plan is not open).
    pub fn next_round(&self, parallelism: usize, pending: &BTreeSet<usize>) -> Vec<usize> {
        if self.status(pending) != Status::Open {
            return Vec::new();
        }
        let free = self.untested(pending);
        let m = free.len();
        let p = parallelism.max(1).min(m);
        let mut out: Vec<usize> = Vec::new();
        for k in 1..=p {
            let pos = (k * m).div_ceil(p + 1).saturating_sub(1).min(m - 1);
            let i = free[pos];
            if !out.contains(&i) {
                out.push(i);
            }
        }
        out
    }

    /// Upper bound on the rounds a plan over `n` candidates needs with `parallelism` probes
    /// per round and no skipped candidates.
    pub fn max_rounds(n: usize, parallelism: usize) -> u32 {
        let base = parallelism.max(1) + 1;
        let mut size = n.saturating_sub(1);
        let mut rounds = 0;
        while size > 0 {
            size /= base;
            rounds += 1;
        }
        rounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a plan to the end against a culprit; returns (found, rounds).
    fn run(n: usize, culprit: usize, p: usize, skip: &[usize]) -> (Status, u32) {
        let mut plan = Plan::new(n);
        let none = BTreeSet::new();
        let mut rounds = 0;
        loop {
            match plan.status(&none) {
                Status::Open => {}
                s => return (s, rounds),
            }
            let probes = plan.next_round(p, &none);
            assert!(!probes.is_empty());
            rounds += 1;
            for i in probes {
                let o = if skip.contains(&i) {
                    Outcome::Skip
                } else if i >= culprit {
                    Outcome::Fail
                } else {
                    Outcome::Pass
                };
                plan.record(i, o);
            }
        }
    }

    #[test]
    fn binary_search_finds_every_culprit_within_the_bound() {
        for n in 1..=40 {
            for culprit in 0..n {
                for p in 1..=4 {
                    let (s, rounds) = run(n, culprit, p, &[]);
                    assert_eq!(s, Status::Found(culprit), "n={n} culprit={culprit} p={p}");
                    assert!(
                        rounds <= Plan::max_rounds(n, p),
                        "n={n} culprit={culprit} p={p} rounds={rounds}"
                    );
                }
            }
        }
    }

    #[test]
    fn eight_changes_with_the_sixth_bad_take_three_sequential_rounds() {
        let (s, rounds) = run(8, 5, 1, &[]);
        assert_eq!(s, Status::Found(5));
        assert_eq!(rounds, 3);
        let (s, rounds) = run(8, 5, 2, &[]);
        assert_eq!(s, Status::Found(5));
        assert_eq!(rounds, 2);
    }

    #[test]
    fn rounds_are_deterministic() {
        let a = Plan::new(8).next_round(1, &BTreeSet::new());
        let b = Plan::new(8).next_round(1, &BTreeSet::new());
        assert_eq!(a, b);
        assert_eq!(a, vec![3]);
        assert_eq!(Plan::new(8).next_round(3, &BTreeSet::new()), vec![1, 3, 5]);
    }

    #[test]
    fn skipped_candidates_are_routed_around_or_leave_the_plan_stuck() {
        let (s, _) = run(8, 5, 1, &[3]);
        assert_eq!(s, Status::Found(5));
        let (s, _) = run(8, 5, 1, &[4]);
        assert_eq!(s, Status::Stuck { lo: 4, hi: 5 });
    }

    #[test]
    fn contradictions_are_reported() {
        let mut plan = Plan::new(4);
        plan.record(1, Outcome::Fail);
        plan.record(2, Outcome::Pass);
        assert!(matches!(
            plan.status(&BTreeSet::new()),
            Status::Contradiction(_)
        ));
        let mut plan = Plan::new(4);
        plan.record(3, Outcome::Pass);
        assert!(matches!(
            plan.status(&BTreeSet::new()),
            Status::Contradiction(_)
        ));
    }

    #[test]
    fn a_single_candidate_is_found_at_once() {
        assert_eq!(Plan::new(1).status(&BTreeSet::new()), Status::Found(0));
    }

    #[test]
    fn pending_probes_are_not_reissued() {
        let plan = Plan::new(8);
        let pending: BTreeSet<usize> = [3].into_iter().collect();
        let next = plan.next_round(1, &pending);
        assert!(!next.contains(&3));
    }
}
