//! CVSS vector parsing and base-score computation for v3.0, v3.1 and v4.0.
//!
//! v3 follows the first.org specification equations (the v3.1 `Roundup` function for 3.1 and the
//! original `ceil(x * 10) / 10` for 3.0). v4.0 follows the MacroVector method: the six
//! equivalence classes select a table score (see `cvss4_table`), then the severity distance to the
//! next lower MacroVector adjusts it. Threat (`E`) and environmental metrics are honored for v4;
//! for v3 only the base score is computed.

use std::collections::BTreeMap;

use crate::cvss4_table::MACRO_SCORES;
use crate::error::{DepsError, Result};

/// A qualitative severity (`bee.v1.SecuritySeverity` minus `UNSPECIFIED`, plus `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Score 0.0, or no information.
    None,
    /// 0.1 to 3.9.
    Low,
    /// 4.0 to 6.9.
    Medium,
    /// 7.0 to 8.9.
    High,
    /// 9.0 to 10.0.
    Critical,
}

impl Severity {
    /// The qualitative rating for a score.
    pub fn from_score(score: f64) -> Severity {
        if score >= 9.0 {
            Severity::Critical
        } else if score >= 7.0 {
            Severity::High
        } else if score >= 4.0 {
            Severity::Medium
        } else if score > 0.0 {
            Severity::Low
        } else {
            Severity::None
        }
    }

    /// Parses a GitHub or OSV label (`LOW`, `MODERATE`, `MEDIUM`, `HIGH`, `CRITICAL`).
    pub fn from_label(label: &str) -> Option<Severity> {
        match label.trim().to_ascii_uppercase().as_str() {
            "NONE" => Some(Severity::None),
            "LOW" => Some(Severity::Low),
            "MODERATE" | "MEDIUM" => Some(Severity::Medium),
            "HIGH" => Some(Severity::High),
            "CRITICAL" => Some(Severity::Critical),
            _ => None,
        }
    }

    /// The `bee.v1.SecuritySeverity` enum number (`UNSPECIFIED` = 0 for [`Severity::None`]).
    pub fn proto_value(self) -> i32 {
        match self {
            Severity::None => 0,
            Severity::Low => 1,
            Severity::Medium => 2,
            Severity::High => 3,
            Severity::Critical => 4,
        }
    }

    /// Upper-case label.
    pub fn label(self) -> &'static str {
        match self {
            Severity::None => "NONE",
            Severity::Low => "LOW",
            Severity::Medium => "MEDIUM",
            Severity::High => "HIGH",
            Severity::Critical => "CRITICAL",
        }
    }
}

/// A parsed CVSS vector with its computed score.
#[derive(Clone, Debug, PartialEq)]
pub struct Cvss {
    /// `3.0`, `3.1` or `4.0`.
    pub version: String,
    /// The vector string as given.
    pub vector: String,
    /// The computed score, one decimal.
    pub score: f64,
    /// The qualitative rating of [`score`](Cvss::score).
    pub severity: Severity,
}

fn bad(v: &str, why: &str) -> DepsError {
    DepsError::invalid("", format!("invalid CVSS vector {v:?}: {why}"))
}

fn metrics(vector: &str) -> Result<(String, BTreeMap<String, String>)> {
    let mut parts = vector.trim().split('/');
    let head = parts.next().unwrap_or("");
    let version = head
        .strip_prefix("CVSS:")
        .ok_or_else(|| bad(vector, "missing CVSS: prefix"))?;
    let mut map = BTreeMap::new();
    for p in parts {
        let (k, v) = p
            .split_once(':')
            .ok_or_else(|| bad(vector, "metric without a value"))?;
        if map.insert(k.to_owned(), v.to_owned()).is_some() {
            return Err(bad(vector, "duplicate metric"));
        }
    }
    Ok((version.to_owned(), map))
}

/// Parses a CVSS v3.0, v3.1 or v4.0 vector and computes its score.
pub fn parse(vector: &str) -> Result<Cvss> {
    let (version, map) = metrics(vector)?;
    let score = match version.as_str() {
        "3.0" | "3.1" => score_v3(vector, &version, &map)?,
        "4.0" => score_v4(vector, &map)?,
        other => return Err(bad(vector, &format!("unsupported version {other}"))),
    };
    Ok(Cvss {
        version,
        vector: vector.trim().to_owned(),
        score,
        severity: Severity::from_score(score),
    })
}

fn pick<'a>(
    vector: &str,
    map: &'a BTreeMap<String, String>,
    key: &str,
    allowed: &[&str],
) -> Result<&'a str> {
    let v = map
        .get(key)
        .ok_or_else(|| bad(vector, &format!("missing metric {key}")))?;
    if allowed.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(bad(vector, &format!("invalid value for {key}")))
    }
}

fn roundup_31(x: f64) -> f64 {
    let i = (x * 100_000.0).round() as i64;
    if i % 10_000 == 0 {
        i as f64 / 100_000.0
    } else {
        ((i / 10_000) + 1) as f64 / 10.0
    }
}

fn roundup_30(x: f64) -> f64 {
    (x * 10.0).ceil() / 10.0
}

fn score_v3(vector: &str, version: &str, m: &BTreeMap<String, String>) -> Result<f64> {
    let av = match pick(vector, m, "AV", &["N", "A", "L", "P"])? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        _ => 0.2,
    };
    let ac = if pick(vector, m, "AC", &["L", "H"])? == "L" {
        0.77
    } else {
        0.44
    };
    let changed = pick(vector, m, "S", &["U", "C"])? == "C";
    let pr = match (pick(vector, m, "PR", &["N", "L", "H"])?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        _ => 0.5,
    };
    let ui = if pick(vector, m, "UI", &["N", "R"])? == "N" {
        0.85
    } else {
        0.62
    };
    let cia = |k: &str| -> Result<f64> {
        Ok(match pick(vector, m, k, &["H", "L", "N"])? {
            "H" => 0.56,
            "L" => 0.22,
            _ => 0.0,
        })
    };
    let iss = 1.0 - (1.0 - cia("C")?) * (1.0 - cia("I")?) * (1.0 - cia("A")?);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    let expl = 8.22 * av * ac * pr * ui;
    if impact <= 0.0 {
        return Ok(0.0);
    }
    let round = if version == "3.0" {
        roundup_30
    } else {
        roundup_31
    };
    let raw = if changed {
        (1.08 * (impact + expl)).min(10.0)
    } else {
        (impact + expl).min(10.0)
    };
    Ok(round(raw))
}

// ---------------------------------------------------------------------------------------------
// CVSS v4.0
// ---------------------------------------------------------------------------------------------

fn macro_score(key: &str) -> Option<f64> {
    MACRO_SCORES
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|i| f64::from(MACRO_SCORES[i].1) / 10.0)
}

struct V4<'a> {
    m: &'a BTreeMap<String, String>,
}

impl V4<'_> {
    /// The effective value of a metric: environmental overrides, then the base value, with the
    /// worst-case defaults for unset threat and requirement metrics.
    fn get(&self, metric: &str) -> char {
        let first = |s: &String| s.chars().next().unwrap_or('X');
        let sel = self.m.get(metric).map_or('X', first);
        match metric {
            "E" if sel == 'X' => return 'A',
            "CR" | "IR" | "AR" if sel == 'X' => return 'H',
            _ => {}
        }
        if let Some(mv) = self.m.get(&format!("M{metric}")) {
            let c = first(mv);
            if c != 'X' {
                return c;
            }
        }
        sel
    }
}

fn level(metric: &str, v: char) -> f64 {
    match (metric, v) {
        ("AV", 'N') | ("PR", 'N') | ("UI", 'N') | ("AC", 'L') | ("AT", 'N') => 0.0,
        ("AV", 'A') | ("PR", 'L') | ("UI", 'P') | ("AC", 'H') | ("AT", 'P') => 0.1,
        ("AV", 'L') | ("PR", 'H') | ("UI", 'A') => 0.2,
        ("AV", 'P') => 0.3,
        ("VC" | "VI" | "VA", 'H') => 0.0,
        ("VC" | "VI" | "VA", 'L') => 0.1,
        ("VC" | "VI" | "VA", 'N') => 0.2,
        ("SC", 'H') => 0.1,
        ("SC", 'L') => 0.2,
        ("SC", 'N') => 0.3,
        ("SI" | "SA", 'S') => 0.0,
        ("SI" | "SA", 'H') => 0.1,
        ("SI" | "SA", 'L') => 0.2,
        ("SI" | "SA", 'N') => 0.3,
        ("CR" | "IR" | "AR", 'H') => 0.0,
        ("CR" | "IR" | "AR", 'M') => 0.1,
        ("CR" | "IR" | "AR", 'L') => 0.2,
        _ => 0.0,
    }
}

fn max_composed(eq: u8, e: [u8; 6]) -> Vec<&'static str> {
    match eq {
        1 => match e[0] {
            0 => vec!["AV:N/PR:N/UI:N/"],
            1 => vec!["AV:A/PR:N/UI:N/", "AV:N/PR:L/UI:N/", "AV:N/PR:N/UI:P/"],
            _ => vec!["AV:P/PR:N/UI:N/", "AV:A/PR:L/UI:P/"],
        },
        2 => match e[1] {
            0 => vec!["AC:L/AT:N/"],
            _ => vec!["AC:H/AT:N/", "AC:L/AT:P/"],
        },
        3 => match (e[2], e[5]) {
            (0, 0) => vec!["VC:H/VI:H/VA:H/CR:H/IR:H/AR:H/"],
            (0, _) => vec![
                "VC:H/VI:H/VA:L/CR:M/IR:M/AR:H/",
                "VC:H/VI:H/VA:H/CR:M/IR:M/AR:M/",
            ],
            (1, 0) => vec![
                "VC:L/VI:H/VA:H/CR:H/IR:H/AR:H/",
                "VC:H/VI:L/VA:H/CR:H/IR:H/AR:H/",
            ],
            (1, _) => vec![
                "VC:L/VI:H/VA:L/CR:H/IR:M/AR:H/",
                "VC:L/VI:H/VA:H/CR:H/IR:M/AR:M/",
                "VC:H/VI:L/VA:H/CR:M/IR:H/AR:M/",
                "VC:H/VI:L/VA:L/CR:M/IR:H/AR:H/",
                "VC:L/VI:L/VA:H/CR:H/IR:H/AR:M/",
            ],
            _ => vec!["VC:L/VI:L/VA:L/CR:H/IR:H/AR:H/"],
        },
        4 => match e[3] {
            0 => vec!["SC:H/SI:S/SA:S/"],
            1 => vec!["SC:H/SI:H/SA:H/"],
            _ => vec!["SC:L/SI:L/SA:L/"],
        },
        _ => match e[4] {
            0 => vec!["E:A/"],
            1 => vec!["E:P/"],
            _ => vec!["E:U/"],
        },
    }
}

fn max_severity_depth(eq: &str, e: [u8; 6]) -> f64 {
    let n = match eq {
        "eq1" => [1, 4, 5][usize::from(e[0]).min(2)],
        "eq2" => [1, 2][usize::from(e[1]).min(1)],
        "eq3eq6" => match (e[2], e[5]) {
            (0, 0) => 7,
            (0, _) => 6,
            (1, 0) | (1, _) => 8,
            _ => 10,
        },
        _ => [6, 5, 4][usize::from(e[3]).min(2)],
    };
    f64::from(n) * 0.1
}

fn extract(vec: &str, metric: &str) -> char {
    let needle = format!("{metric}:");
    let mut start = 0;
    while let Some(i) = vec[start..].find(&needle) {
        let at = start + i;
        // The metric name must start a segment, so `C:` does not match inside `VC:`.
        if at == 0 || vec.as_bytes()[at - 1] == b'/' {
            return vec[at + needle.len()..].chars().next().unwrap_or('X');
        }
        start = at + 1;
    }
    'X'
}

fn score_v4(vector: &str, m: &BTreeMap<String, String>) -> Result<f64> {
    let allowed: [(&str, &str); 11] = [
        ("AV", "NALP"),
        ("AC", "LH"),
        ("AT", "NP"),
        ("PR", "NLH"),
        ("UI", "NPA"),
        ("VC", "HLN"),
        ("VI", "HLN"),
        ("VA", "HLN"),
        ("SC", "HLN"),
        ("SI", "HLN"),
        ("SA", "HLN"),
    ];
    for (k, vals) in allowed {
        let v = m
            .get(k)
            .ok_or_else(|| bad(vector, &format!("missing metric {k}")))?;
        if v.chars().count() != 1 || !vals.contains(v.as_str()) {
            return Err(bad(vector, &format!("invalid value for {k}")));
        }
    }
    let v = V4 { m };
    let g = |k: &str| v.get(k);
    if ["VC", "VI", "VA", "SC", "SI", "SA"]
        .iter()
        .all(|k| g(k) == 'N')
    {
        return Ok(0.0);
    }
    // EQ1..EQ6 digits.
    let (av, pr, ui) = (g("AV"), g("PR"), g("UI"));
    let eq1 = if av == 'N' && pr == 'N' && ui == 'N' {
        0
    } else if (av == 'N' || pr == 'N' || ui == 'N') && av != 'P' {
        1
    } else {
        2
    };
    let eq2 = if g("AC") == 'L' && g("AT") == 'N' {
        0
    } else {
        1
    };
    let eq3 = if g("VC") == 'H' && g("VI") == 'H' {
        0
    } else if g("VC") == 'H' || g("VI") == 'H' || g("VA") == 'H' {
        1
    } else {
        2
    };
    let eq4 = if g("MSI") == 'S' || g("MSA") == 'S' {
        0
    } else if g("SC") == 'H' || g("SI") == 'H' || g("SA") == 'H' {
        1
    } else {
        2
    };
    let eq5 = match g("E") {
        'A' => 0,
        'P' => 1,
        _ => 2,
    };
    let eq6 = if (g("CR") == 'H' && g("VC") == 'H')
        || (g("IR") == 'H' && g("VI") == 'H')
        || (g("AR") == 'H' && g("VA") == 'H')
    {
        0
    } else {
        1
    };
    let e = [eq1, eq2, eq3, eq4, eq5, eq6];
    let key = |d: [u8; 6]| d.iter().map(|x| char::from(b'0' + x)).collect::<String>();
    let value =
        macro_score(&key(e)).ok_or_else(|| bad(vector, "no MacroVector for this combination"))?;

    let with = |idx: usize, add: u8| -> [u8; 6] {
        let mut d = e;
        d[idx] += add;
        d
    };
    let lower1 = macro_score(&key(with(0, 1)));
    let lower2 = macro_score(&key(with(1, 1)));
    let lower36 = match (eq3, eq6) {
        (1, 1) | (0, 1) => macro_score(&key(with(2, 1))),
        (1, 0) => macro_score(&key(with(5, 1))),
        (0, 0) => {
            let left = macro_score(&key(with(5, 1)));
            let right = macro_score(&key(with(2, 1)));
            match (left, right) {
                (Some(l), Some(r)) => Some(if l > r { l } else { r }),
                (l, r) => l.or(r),
            }
        }
        _ => None,
    };
    let lower4 = macro_score(&key(with(3, 1)));
    let lower5 = macro_score(&key(with(4, 1)));

    // The highest-severity vector in the MacroVector that is at least as severe as this one.
    let mut maxes: Vec<String> = Vec::new();
    for a in max_composed(1, e) {
        for b in max_composed(2, e) {
            for c in max_composed(3, e) {
                for d in max_composed(4, e) {
                    for f in max_composed(5, e) {
                        maxes.push(format!("{a}{b}{c}{d}{f}"));
                    }
                }
            }
        }
    }
    let metrics_list = [
        "AV", "PR", "UI", "AC", "AT", "VC", "VI", "VA", "SC", "SI", "SA", "CR", "IR", "AR",
    ];
    let distances = |mv: &str| -> BTreeMap<&'static str, f64> {
        metrics_list
            .iter()
            .map(|k| (*k, level(k, g(k)) - level(k, extract(mv, k))))
            .collect()
    };
    let mut chosen = distances(&maxes[0]);
    for mv in &maxes {
        let d = distances(mv);
        chosen = d;
        if chosen.values().all(|x| *x >= -1e-9) {
            break;
        }
    }
    let d = |k: &str| chosen.get(k).copied().unwrap_or(0.0);
    let cur1 = d("AV") + d("PR") + d("UI");
    let cur2 = d("AC") + d("AT");
    let cur36 = d("VC") + d("VI") + d("VA") + d("CR") + d("IR") + d("AR");
    let cur4 = d("SC") + d("SI") + d("SA");

    let mut n = 0u32;
    let mut sum = 0.0;
    let mut part = |lower: Option<f64>, current: f64, depth: f64| {
        if let Some(l) = lower {
            n += 1;
            sum += (value - l) * (current / depth);
        }
    };
    part(lower1, cur1, max_severity_depth("eq1", e));
    part(lower2, cur2, max_severity_depth("eq2", e));
    part(lower36, cur36, max_severity_depth("eq3eq6", e));
    part(lower4, cur4, max_severity_depth("eq4", e));
    // EQ5 contributes a zero-distance term whenever a lower MacroVector exists.
    part(lower5, 0.0, 1.0);
    let mean = if n == 0 { 0.0 } else { sum / f64::from(n) };
    let score = (value - mean).clamp(0.0, 10.0);
    Ok((score * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(v: &str) -> f64 {
        parse(v).unwrap().score
    }

    #[test]
    fn cvss31_reference_scores() {
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), 9.8);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H"), 10.0);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"), 7.5);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"), 6.1);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"), 8.8);
        assert_eq!(score("CVSS:3.1/AV:L/AC:L/PR:N/UI:R/S:U/C:H/I:H/A:H"), 7.8);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"), 5.3);
        assert_eq!(score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N"), 0.0);
        assert_eq!(score("CVSS:3.1/AV:P/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N"), 1.6);
    }

    #[test]
    fn cvss30_uses_the_older_rounding() {
        assert_eq!(score("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), 9.8);
        assert_eq!(score("CVSS:3.0/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"), 6.1);
    }

    #[test]
    fn cvss4_reference_scores() {
        assert_eq!(
            score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N"),
            9.3
        );
        assert_eq!(
            score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:L/VI:L/VA:L/SC:N/SI:N/SA:N"),
            6.9
        );
        assert_eq!(
            score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:N/SC:N/SI:N/SA:N"),
            0.0
        );
        // A lower-than-table vector is pulled down from its MacroVector score.
        let low = score("CVSS:4.0/AV:L/AC:H/AT:P/PR:H/UI:A/VC:L/VI:N/VA:N/SC:N/SI:N/SA:N");
        assert!(low > 0.0 && low < 3.0, "{low}");
        // Environmental and threat metrics lower the score.
        let base = score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N");
        let threat = score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N/E:U");
        assert!(threat < base);
    }

    #[test]
    fn severity_bands() {
        assert_eq!(Severity::from_score(0.0), Severity::None);
        assert_eq!(Severity::from_score(3.9), Severity::Low);
        assert_eq!(Severity::from_score(4.0), Severity::Medium);
        assert_eq!(Severity::from_score(7.0), Severity::High);
        assert_eq!(Severity::from_score(9.0), Severity::Critical);
        assert_eq!(Severity::from_label("moderate"), Some(Severity::Medium));
        assert!(Severity::Critical > Severity::High);
        assert_eq!(
            parse("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H")
                .unwrap()
                .severity,
            Severity::Critical
        );
    }

    #[test]
    fn rejects_malformed_vectors() {
        for v in [
            "",
            "AV:N/AC:L",
            "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H",
            "CVSS:3.1/AV:X/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H",
            "CVSS:3.1/AV:N/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H",
            "CVSS:2.0/AV:N/AC:L",
            "CVSS:4.0/AV:N/AC:L",
        ] {
            assert!(parse(v).is_err(), "{v}");
        }
    }

    #[test]
    fn every_v4_table_key_is_a_valid_macrovector_and_sorted() {
        assert!(MACRO_SCORES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(MACRO_SCORES.len(), 270);
    }
}
