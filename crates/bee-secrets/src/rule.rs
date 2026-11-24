//! Rule definitions: the shape shared by built-in and custom rules.

use bee_core::{BeeError, Subcode};
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;

/// How sure a rule is that a match is a live credential.
///
/// `High` and `Medium` findings block under push protection; `Low` findings (generic
/// assignments) are reported but never block on their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Heuristic class (generic `password=` assignments); advisory only.
    Low,
    /// Provider-shaped but weakly distinctive (JWTs, test keys).
    Medium,
    /// Provider-specific prefix and structure.
    High,
}

impl Confidence {
    /// True when findings of this confidence block under push protection.
    pub fn blocks(self) -> bool {
        self >= Confidence::Medium
    }

    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

fn default_confidence() -> Confidence {
    Confidence::High
}

/// The serialisable shape of a rule, used for built-ins and for `[[rule]]` tables in config.
///
/// ```toml
/// [[rule]]
/// id = "acme-token"             # unique, lowercase kebab-case
/// provider = "acme"
/// description = "Acme API token"
/// regex = '\b(acme_[A-Za-z0-9]{32})\b'   # regex crate syntax, byte mode, ASCII semantics
/// keywords = ["acme_"]          # at least one; a file is only regex-scanned if one occurs
/// entropy = 3.0                 # optional minimum Shannon entropy (bits per byte) of the secret
/// confidence = "high"           # low | medium | high (default high)
/// example = "token = acme_..."  # optional: must match (checked by `Rule::check_examples`)
/// near_miss = "acme_short"      # optional: must not match
/// ```
///
/// The secret is the first capture group that participates in the match (group 0 when the regex
/// has no groups). Findings carry the byte range of the secret, never its text.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// Unique rule id.
    pub id: String,
    /// Provider name for display and audit.
    pub provider: String,
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// Regex source.
    pub regex: String,
    /// Case-insensitive literal prefilter keywords.
    pub keywords: Vec<String>,
    /// Minimum Shannon entropy of the secret, bits per byte.
    #[serde(default)]
    pub entropy: Option<f64>,
    /// Confidence class.
    #[serde(default = "default_confidence")]
    pub confidence: Confidence,
    /// A snippet that must match.
    #[serde(default)]
    pub example: Option<String>,
    /// A snippet that must not match.
    #[serde(default)]
    pub near_miss: Option<String>,
}

/// A compiled rule.
#[derive(Clone, Debug)]
pub struct Rule {
    spec: RuleSpec,
    keywords: Vec<String>,
    regex: Regex,
}

/// A raw rule match, before allow-listing and line resolution.
pub(crate) struct RawMatch {
    pub start: usize,
    pub end: usize,
}

impl Rule {
    /// Compiles a rule, validating its shape. Errors are `INVALID_ARGUMENT` / `INVALID_FIELD`.
    pub fn new(spec: RuleSpec) -> Result<Rule, BeeError> {
        let bad = |msg: String| BeeError::invalid(Subcode::InvalidField, msg);
        if spec.id.is_empty()
            || !spec
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(bad(format!(
                "rule id {:?} must be lowercase kebab-case",
                spec.id
            )));
        }
        if spec.keywords.is_empty() || spec.keywords.iter().any(|k| k.is_empty()) {
            return Err(bad(format!(
                "rule {} needs at least one non-empty keyword",
                spec.id
            )));
        }
        if let Some(e) = spec.entropy
            && !(0.0..=8.0).contains(&e)
        {
            return Err(bad(format!("rule {} entropy must be in 0..=8", spec.id)));
        }
        let regex = RegexBuilder::new(&spec.regex)
            .unicode(false)
            .size_limit(32 << 20)
            .build()
            .map_err(|e| bad(format!("rule {} regex does not compile: {e}", spec.id)))?;
        let keywords = spec
            .keywords
            .iter()
            .map(|k| k.to_ascii_lowercase())
            .collect();
        Ok(Rule {
            spec,
            keywords,
            regex,
        })
    }

    /// The rule id.
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// The provider name.
    pub fn provider(&self) -> &str {
        &self.spec.provider
    }

    /// The description.
    pub fn description(&self) -> &str {
        &self.spec.description
    }

    /// The confidence class.
    pub fn confidence(&self) -> Confidence {
        self.spec.confidence
    }

    /// The lowercase prefilter keywords.
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }

    /// The example snippet, if any.
    pub fn example(&self) -> Option<&str> {
        self.spec.example.as_deref()
    }

    /// The near-miss snippet, if any.
    pub fn near_miss(&self) -> Option<&str> {
        self.spec.near_miss.as_deref()
    }

    /// Runs the rule over `bytes` (no prefilter) and appends accepted matches.
    pub(crate) fn find_all(&self, bytes: &[u8], out: &mut Vec<RawMatch>) {
        for caps in self.regex.captures_iter(bytes) {
            let m = (1..caps.len())
                .find_map(|i| caps.get(i))
                .or_else(|| caps.get(0));
            let Some(m) = m else { continue };
            if let Some(min) = self.spec.entropy
                && shannon_entropy(m.as_bytes()) < min
            {
                continue;
            }
            out.push(RawMatch {
                start: m.start(),
                end: m.end(),
            });
        }
    }

    /// Checks that `example` matches and `near_miss` does not, with keyword prefilter semantics.
    pub fn check_examples(&self) -> Result<(), String> {
        let hits = |text: &str| {
            let lower = text.to_ascii_lowercase();
            if !self.keywords.iter().any(|k| lower.contains(k.as_str())) {
                return false;
            }
            let mut v = Vec::new();
            self.find_all(text.as_bytes(), &mut v);
            !v.is_empty()
        };
        if let Some(e) = self.example()
            && !hits(e)
        {
            return Err(format!("{}: example does not match", self.id()));
        }
        if let Some(n) = self.near_miss()
            && hits(n)
        {
            return Err(format!("{}: near miss matches", self.id()));
        }
        Ok(())
    }
}

/// Shannon entropy of `bytes` in bits per byte (0.0 for empty input).
///
/// Pure and deterministic: counts are integers and the sum runs in byte order.
pub fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}
