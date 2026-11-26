//! The SPEC §16 targets and the report the load harness prints.
//!
//! Every row of the SPEC §16 table is a [`Target`]. A harness run records a [`Measurement`]
//! per target it could measure; [`Report::markdown`] renders target against measurement with
//! a hit or miss and the margin, plus the machine, the load average around the run and the
//! store. A target the run could not measure is listed as such with the reason, never
//! silently dropped.

use std::fmt::Write as _;
use std::time::Duration;

use super::stats::{Summary, ms};

/// How a target is judged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Goal {
    /// p50 and p99 latency ceilings.
    Latency {
        /// p50 ceiling.
        p50: Duration,
        /// p99 ceiling.
        p99: Duration,
    },
    /// A p99 latency ceiling.
    P99(Duration),
    /// A wall-clock ceiling for one operation (median of the samples).
    Within(Duration),
    /// A throughput floor with a p99 latency ceiling (hot-ref landing).
    Throughput {
        /// Minimum operations per second.
        per_second: f64,
        /// p99 ceiling of the per-operation time.
        p99: Duration,
    },
    /// A floor on a concurrent count.
    AtLeast(u64),
    /// Availability ratios (reads, writes) over a month: a single run reports its observed
    /// success ratios but cannot meet or miss the target.
    Availability {
        /// Read success ratio.
        reads: f64,
        /// Write success ratio.
        writes: f64,
    },
}

/// One SPEC §16 row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// Stable key used by measurements.
    pub key: &'static str,
    /// The operation, as SPEC §16 names it.
    pub operation: &'static str,
    /// The target, as SPEC §16 states it.
    pub text: &'static str,
    /// How it is judged.
    pub goal: Goal,
}

const fn msd(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Every SPEC §16 target, in table order, plus the harness's extra measurements that SPEC
/// §16 does not bound (git clone throughput, search latency) so the report shows them.
pub const TARGETS: &[Target] = &[
    Target {
        key: "create_changeset",
        operation: "CreateChangeset, 1 file changed, server time",
        text: "p50 ≤ 10 ms, p99 ≤ 50 ms, independent of repo size",
        goal: Goal::Latency {
            p50: msd(10),
            p99: msd(50),
        },
    },
    Target {
        key: "land_uncontended",
        operation: "Land, uncontended, 1-change stack",
        text: "p50 ≤ 30 ms, p99 ≤ 150 ms",
        goal: Goal::Latency {
            p50: msd(30),
            p99: msd(150),
        },
    },
    Target {
        key: "land_hot_ref",
        operation: "Land throughput, one hot ref, 1,000 concurrent landers",
        text: "≥ 100 changes/s, every lander lands within 10 s p99",
        goal: Goal::Throughput {
            per_second: 100.0,
            p99: msd(10_000),
        },
    },
    Target {
        key: "checkout_warm",
        operation: "Sparse checkout, 1,000 files, warm node cache",
        text: "≤ 1 s",
        goal: Goal::Within(msd(1_000)),
    },
    Target {
        key: "checkout_cold",
        operation: "Sparse checkout, 1,000 files, cold, same region",
        text: "≤ 3 s",
        goal: Goal::Within(msd(3_000)),
    },
    Target {
        key: "log",
        operation: "Log via API, 100-entry page",
        text: "p99 ≤ 200 ms",
        goal: Goal::P99(msd(200)),
    },
    Target {
        key: "blame",
        operation: "Blame via API, one file of 10k lines",
        text: "p99 ≤ 200 ms",
        goal: Goal::P99(msd(200)),
    },
    Target {
        key: "diff",
        operation: "Diff via API, one file of 10k lines",
        text: "p99 ≤ 200 ms",
        goal: Goal::P99(msd(200)),
    },
    Target {
        key: "event_delivery",
        operation: "Event delivery, commit to subscriber",
        text: "p99 ≤ 1 s",
        goal: Goal::P99(msd(1_000)),
    },
    Target {
        key: "sessions",
        operation: "Concurrent agent sessions with streams open",
        text: "≥ 10,000 per cell",
        goal: Goal::AtLeast(10_000),
    },
    Target {
        key: "availability",
        operation: "Availability",
        text: "reads 99.99%, writes 99.95%, per cell, per month",
        goal: Goal::Availability {
            reads: 0.9999,
            writes: 0.9995,
        },
    },
];

/// What a run measured for one target.
#[derive(Clone, Debug, PartialEq)]
pub enum Measured {
    /// Latency samples.
    Latency(Summary),
    /// Throughput and per-operation latency.
    Throughput {
        /// Operations per second over the run's wall clock.
        per_second: f64,
        /// Per-operation latency.
        latency: Summary,
    },
    /// A concurrent count reached, with the latency of the operation that proved it.
    Count {
        /// The count held open at once.
        count: u64,
        /// Delivery latency across the count, when measured.
        latency: Option<Summary>,
    },
    /// Observed success ratios of reads and writes during the run.
    Ratios {
        /// Successful reads / reads.
        reads: f64,
        /// Successful writes / writes.
        writes: f64,
        /// Requests counted.
        requests: u64,
    },
    /// Not measured, and why.
    NotMeasured(String),
}

/// One row of a report.
#[derive(Clone, Debug, PartialEq)]
pub struct Measurement {
    /// [`Target::key`].
    pub key: &'static str,
    /// The measurement.
    pub measured: Measured,
    /// Extra detail (batches, sizes, workload).
    pub note: String,
}

/// The judgement of one row.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Every bound met; `margin` is the smallest headroom as a fraction of the bound.
    Hit {
        /// Smallest headroom (0.25 = 25 % under the bound).
        margin: f64,
    },
    /// A bound missed; `by` is the largest overshoot as a fraction of the bound.
    Miss {
        /// Largest overshoot (0.10 = 10 % over the bound).
        by: f64,
    },
    /// Cannot be judged by one run (availability) or was not measured.
    NotJudged(String),
}

/// Headroom of `measured` under a ceiling `bound` (negative when over).
fn under(measured: f64, bound: f64) -> f64 {
    (bound - measured) / bound
}

/// Judges one measurement against its target.
pub fn judge(target: &Target, measured: &Measured) -> Outcome {
    let secs = |d: Duration| d.as_secs_f64();
    let margins: Vec<f64> = match (target.goal, measured) {
        (_, Measured::NotMeasured(why)) => return Outcome::NotJudged(why.clone()),
        (Goal::Latency { p50, p99 }, Measured::Latency(s)) => {
            vec![under(secs(s.p50), secs(p50)), under(secs(s.p99), secs(p99))]
        }
        (Goal::P99(p99), Measured::Latency(s)) => vec![under(secs(s.p99), secs(p99))],
        // Nothing delivered is a failure, not a zero latency.
        (
            Goal::P99(_),
            Measured::Count {
                latency: Some(s), ..
            },
        ) if s.count == 0 => return Outcome::Miss { by: 1.0 },
        (
            Goal::P99(p99),
            Measured::Count {
                latency: Some(s), ..
            },
        ) => {
            vec![under(secs(s.p99), secs(p99))]
        }
        (Goal::Within(limit), Measured::Latency(s)) => vec![under(secs(s.p50), secs(limit))],
        (
            Goal::Throughput { per_second, p99 },
            Measured::Throughput {
                per_second: got,
                latency,
            },
        ) => vec![
            (got - per_second) / per_second,
            under(secs(latency.p99), secs(p99)),
        ],
        (Goal::AtLeast(n), Measured::Count { count, .. }) => {
            vec![(*count as f64 - n as f64) / n as f64]
        }
        (Goal::Availability { .. }, Measured::Ratios { .. }) => {
            return Outcome::NotJudged(
                "a monthly SLO; one run reports its observed success ratios".into(),
            );
        }
        _ => return Outcome::NotJudged("measurement does not match the target's kind".into()),
    };
    let worst = margins.iter().copied().fold(f64::INFINITY, f64::min);
    if worst >= 0.0 {
        Outcome::Hit { margin: worst }
    } else {
        Outcome::Miss { by: -worst }
    }
}

/// Where and how a run happened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunInfo {
    /// Date (UTC, as given by the caller).
    pub date: String,
    /// Machine description (CPU, cores, memory, OS).
    pub machine: String,
    /// The metadata store (`sqlite`, `postgres`) and its durability settings.
    pub store: String,
    /// Build profile (`release`).
    pub profile: String,
    /// 1-minute load average before the run.
    pub load_before: Option<f64>,
    /// 1-minute load average after the run.
    pub load_after: Option<f64>,
    /// Free-form notes (workload sizes, caveats).
    pub notes: Vec<String>,
}

/// A harness report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Run information.
    pub info: RunInfo,
    /// Measurements, any order.
    pub rows: Vec<Measurement>,
    /// Extra measurements outside SPEC §16 (name, value).
    pub extra: Vec<(String, String)>,
}

fn pct(f: f64) -> String {
    format!("{:.0} %", f * 100.0)
}

fn measured_cells(m: &Measured) -> (String, String, String) {
    match m {
        Measured::Latency(s) => (ms(s.p50), ms(s.p99), format!("{} samples", s.count)),
        Measured::Throughput {
            per_second,
            latency,
        } => (
            ms(latency.p50),
            ms(latency.p99),
            format!("{per_second:.0} lands/s; {} landers", latency.count),
        ),
        Measured::Count { count, latency } => match latency {
            Some(s) => (ms(s.p50), ms(s.p99), format!("{count} streams open")),
            None => ("–".into(), "–".into(), format!("{count} streams open")),
        },
        Measured::Ratios {
            reads,
            writes,
            requests,
        } => (
            "–".into(),
            "–".into(),
            format!(
                "observed reads {:.4} %, writes {:.4} % over {requests} requests",
                reads * 100.0,
                writes * 100.0
            ),
        ),
        Measured::NotMeasured(_) => ("–".into(), "–".into(), "not measured".into()),
    }
}

impl Report {
    /// The measurement for a target key, when present.
    pub fn get(&self, key: &str) -> Option<&Measurement> {
        self.rows.iter().find(|m| m.key == key)
    }

    /// Every target judged, in SPEC order; missing measurements count as not measured.
    pub fn outcomes(&self) -> Vec<(&'static Target, Outcome)> {
        TARGETS
            .iter()
            .map(|t| {
                let o = match self.get(t.key) {
                    Some(m) => judge(t, &m.measured),
                    None => Outcome::NotJudged("not run".into()),
                };
                (t, o)
            })
            .collect()
    }

    /// The report as Markdown: run information, then the target table.
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        let i = &self.info;
        let load = |l: Option<f64>| l.map_or_else(|| "unknown".to_string(), |v| format!("{v:.2}"));
        let _ = writeln!(out, "- Date: {}", i.date);
        let _ = writeln!(out, "- Machine: {}", i.machine);
        let _ = writeln!(out, "- Store: {}", i.store);
        let _ = writeln!(out, "- Build: {}", i.profile);
        let _ = writeln!(
            out,
            "- Load average (1 min): {} before, {} after",
            load(i.load_before),
            load(i.load_after)
        );
        for n in &i.notes {
            let _ = writeln!(out, "- {n}");
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "| Operation | Target | p50 | p99 | Measured | Result | Notes |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|");
        for (t, outcome) in self.outcomes() {
            let m = self.get(t.key);
            let (p50, p99, what) = m.map_or_else(
                || ("–".into(), "–".into(), "not run".into()),
                |m| measured_cells(&m.measured),
            );
            let result = match &outcome {
                Outcome::Hit { margin } => format!("hit ({} margin)", pct(*margin)),
                Outcome::Miss { by } => format!("**miss** (by {})", pct(*by)),
                Outcome::NotJudged(why) => format!("not judged: {why}"),
            };
            let note = m.map(|m| m.note.as_str()).unwrap_or("");
            let _ = writeln!(
                out,
                "| {} | {} | {p50} | {p99} | {what} | {result} | {note} |",
                t.operation, t.text
            );
        }
        if !self.extra.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "| Extra measurement | Value |");
            let _ = writeln!(out, "|---|---|");
            for (k, v) in &self.extra {
                let _ = writeln!(out, "| {k} | {v} |");
            }
        }
        out
    }
}

/// The 1-minute load average of this machine, when the OS reports it (`/proc/loadavg` on
/// Linux, `sysctl -n vm.loadavg` on macOS).
pub fn load_average() -> Option<f64> {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        return s.split_whitespace().next()?.parse().ok();
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|w| w.parse::<f64>().is_ok())?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lat(p50: u64, p99: u64) -> Summary {
        Summary {
            count: 10,
            p50: msd(p50),
            p90: msd(p99),
            p99: msd(p99),
            max: msd(p99),
            mean: msd(p50),
        }
    }

    fn target(key: &str) -> &'static Target {
        TARGETS.iter().find(|t| t.key == key).unwrap()
    }

    #[test]
    fn every_spec_row_has_a_target() {
        assert_eq!(TARGETS.len(), 11);
        let keys: std::collections::BTreeSet<_> = TARGETS.iter().map(|t| t.key).collect();
        assert_eq!(keys.len(), TARGETS.len());
    }

    #[test]
    fn hits_and_misses_carry_margins() {
        let t = target("create_changeset");
        match judge(t, &Measured::Latency(lat(5, 25))) {
            Outcome::Hit { margin } => assert!((margin - 0.5).abs() < 1e-9),
            o => panic!("{o:?}"),
        }
        match judge(t, &Measured::Latency(lat(12, 25))) {
            Outcome::Miss { by } => assert!((by - 0.2).abs() < 1e-9),
            o => panic!("{o:?}"),
        }
        let hot = target("land_hot_ref");
        let m = Measured::Throughput {
            per_second: 77.0,
            latency: lat(6_000, 12_900),
        };
        match judge(hot, &m) {
            Outcome::Miss { by } => assert!((by - 0.29).abs() < 1e-9),
            o => panic!("{o:?}"),
        }
        let s = target("sessions");
        assert!(matches!(
            judge(
                s,
                &Measured::Count {
                    count: 12_000,
                    latency: None
                }
            ),
            Outcome::Hit { .. }
        ));
        let e = target("event_delivery");
        assert!(matches!(
            judge(
                e,
                &Measured::Count {
                    count: 10,
                    latency: Some(lat(100, 900))
                }
            ),
            Outcome::Hit { .. }
        ));
        assert_eq!(
            judge(
                e,
                &Measured::Count {
                    count: 10,
                    latency: Some(Summary::default())
                }
            ),
            Outcome::Miss { by: 1.0 }
        );
        assert!(matches!(
            judge(
                target("availability"),
                &Measured::Ratios {
                    reads: 1.0,
                    writes: 1.0,
                    requests: 5
                }
            ),
            Outcome::NotJudged(_)
        ));
        assert!(matches!(
            judge(t, &Measured::NotMeasured("x".into())),
            Outcome::NotJudged(_)
        ));
    }

    #[test]
    fn markdown_lists_every_target() {
        let r = Report {
            info: RunInfo {
                date: "2026-10-19".into(),
                machine: "test".into(),
                store: "sqlite".into(),
                profile: "release".into(),
                load_before: Some(12.5),
                load_after: None,
                notes: vec!["n".into()],
            },
            rows: vec![Measurement {
                key: "log",
                measured: Measured::Latency(lat(10, 20)),
                note: "100 entries".into(),
            }],
            extra: vec![("git clone".into(), "1 MB/s".into())],
        };
        let md = r.markdown();
        for t in TARGETS {
            assert!(md.contains(t.operation), "{}", t.operation);
        }
        assert!(md.contains("12.50 before, unknown after"));
        assert!(md.contains("hit (90 % margin)"));
        assert!(md.contains("not judged: not run"));
        assert!(md.contains("| git clone | 1 MB/s |"));
    }
}
