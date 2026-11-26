//! The scanner: prefilter, regex, entropy, allow-lists.

use std::collections::BTreeMap;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use bee_core::{BeeError, Subcode};
use serde::Deserialize;

use crate::builtin::builtin_rules;
use crate::glob::glob_match;
use crate::rule::{Confidence, RawMatch, Rule, RuleSpec};

/// The inline marker that allows every finding that starts on the same line.
pub const INLINE_ALLOW: &str = "bee:allow-secret";

/// Default size cap: files above 1 MiB are not scanned.
pub const DEFAULT_MAX_FILE_SIZE: usize = 1 << 20;

/// How many leading bytes are inspected for a NUL when deciding a file is binary.
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// One detected secret. It locates the secret and never contains it.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// The path given to [`Scanner::scan`].
    pub path: String,
    /// Id of the rule that matched.
    pub rule_id: String,
    /// Provider of the rule.
    pub provider: String,
    /// Byte offset of the first byte of the secret.
    pub start: usize,
    /// Byte offset one past the last byte of the secret.
    pub end: usize,
    /// One-based line of `start`.
    pub line: usize,
    /// A redacted preview: at most a quarter of the secret's bytes (up to four), then `****`.
    pub redacted_preview: String,
    /// The rule's confidence class.
    pub confidence: Confidence,
}

/// An allow-list entry. A finding is allowed when its path matches any of `paths` (or `paths` is
/// empty) and its rule is in `rules` (or `rules` is empty). Path globs follow the rules in the
/// crate docs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowEntry {
    /// Path globs.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Rule ids.
    #[serde(default)]
    pub rules: Vec<String>,
}

/// Scanner limits and allow-list.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Files larger than this many bytes are skipped.
    pub max_file_size: usize,
    /// Allow-list entries.
    pub allow: Vec<AllowEntry>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            allow: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    max_file_size: Option<usize>,
    #[serde(default = "yes")]
    builtin: bool,
    #[serde(default)]
    rule: Vec<RuleSpec>,
    #[serde(default)]
    allow: Vec<AllowEntry>,
}

fn yes() -> bool {
    true
}

/// A compiled rule set with its keyword prefilter.
#[derive(Debug)]
pub struct Scanner {
    rules: Vec<Rule>,
    prefilter: AhoCorasick,
    keyword_rules: Vec<Vec<usize>>,
    options: ScanOptions,
}

impl Scanner {
    /// A scanner over `rules` with default options.
    ///
    /// # Panics
    ///
    /// Never for rules built through [`Rule::new`], which validates keywords.
    pub fn new(rules: Vec<Rule>) -> Scanner {
        Scanner::with_options(rules, ScanOptions::default())
    }

    /// A scanner over `rules` with explicit options.
    pub fn with_options(rules: Vec<Rule>, options: ScanOptions) -> Scanner {
        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        let mut keywords: Vec<&str> = Vec::new();
        let mut keyword_rules: Vec<Vec<usize>> = Vec::new();
        for (ri, rule) in rules.iter().enumerate() {
            for k in rule.keywords() {
                let slot = *index.entry(k.as_str()).or_insert_with(|| {
                    keywords.push(k.as_str());
                    keyword_rules.push(Vec::new());
                    keywords.len() - 1
                });
                if !keyword_rules[slot].contains(&ri) {
                    keyword_rules[slot].push(ri);
                }
            }
        }
        let prefilter = AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::Standard)
            .build(&keywords)
            .expect("keyword set is small and non-empty strings");
        Scanner {
            rules,
            prefilter,
            keyword_rules,
            options,
        }
    }

    /// A scanner with the built-in rules and default options.
    pub fn builtin() -> Scanner {
        Scanner::new(builtin_rules())
    }

    /// Builds a scanner from TOML config.
    ///
    /// ```toml
    /// max_file_size = 2097152     # optional, bytes
    /// builtin = true              # optional: include the built-in rules (default true)
    ///
    /// [[rule]]                    # custom rules, same shape as built-ins (see `RuleSpec`)
    /// id = "acme-token"
    /// provider = "acme"
    /// regex = '\b(acme_[A-Za-z0-9]{32})\b'
    /// keywords = ["acme_"]
    ///
    /// [[allow]]                   # either key may be omitted to mean "any"
    /// paths = ["tests/fixtures/**", "*.md"]
    /// rules = ["aws-access-key-id"]
    /// ```
    ///
    /// Unknown keys, duplicate rule ids, bad regexes, allow entries naming unknown rules and
    /// empty allow entries are `INVALID_ARGUMENT` errors.
    pub fn from_toml(text: &str) -> Result<Scanner, BeeError> {
        let bad = |msg: String| BeeError::invalid(Subcode::InvalidField, msg);
        let cfg: ConfigFile = toml::from_str(text)
            .map_err(|e| bad(format!("invalid secret-scanning config: {e}")))?;
        let mut rules = if cfg.builtin {
            builtin_rules()
        } else {
            Vec::new()
        };
        for spec in cfg.rule {
            let rule = Rule::new(spec)?;
            if rules.iter().any(|r| r.id() == rule.id()) {
                return Err(bad(format!("duplicate rule id {}", rule.id())));
            }
            rules.push(rule);
        }
        for entry in &cfg.allow {
            if entry.paths.is_empty() && entry.rules.is_empty() {
                return Err(bad("an allow entry needs paths or rules".into()));
            }
            if let Some(unknown) = entry
                .rules
                .iter()
                .find(|id| !rules.iter().any(|r| r.id() == id.as_str()))
            {
                return Err(bad(format!("allow entry names unknown rule {unknown}")));
            }
        }
        if rules.is_empty() {
            return Err(bad("no rules configured".into()));
        }
        let options = ScanOptions {
            max_file_size: cfg.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
            allow: cfg.allow,
        };
        Ok(Scanner::with_options(rules, options))
    }

    /// The compiled rules.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// True when `bytes` would be scanned: not over the size cap and no NUL in the first
    /// [`BINARY_SNIFF_BYTES`] bytes.
    pub fn is_scannable(&self, bytes: &[u8]) -> bool {
        bytes.len() <= self.options.max_file_size
            && !bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
    }

    /// Scans one file. Findings are sorted by `(start, end, rule_id)`; a finding wholly inside a
    /// wider one is dropped. Unscannable files (see [`Scanner::is_scannable`]) yield nothing.
    pub fn scan(&self, path: &str, bytes: &[u8]) -> Vec<Finding> {
        if !self.is_scannable(bytes) {
            return Vec::new();
        }
        let mut hit = vec![false; self.rules.len()];
        let mut remaining = self.rules.len();
        for m in self.prefilter.find_overlapping_iter(bytes) {
            for &ri in &self.keyword_rules[m.pattern().as_usize()] {
                if !hit[ri] {
                    hit[ri] = true;
                    remaining -= 1;
                }
            }
            if remaining == 0 {
                break;
            }
        }
        let mut raw: Vec<(usize, RawMatch)> = Vec::new();
        for (ri, rule) in self.rules.iter().enumerate() {
            if !hit[ri] || self.path_allows(path, rule.id()) {
                continue;
            }
            let mut found = Vec::new();
            rule.find_all(bytes, &mut found);
            raw.extend(found.into_iter().map(|m| (ri, m)));
        }
        if raw.is_empty() {
            return Vec::new();
        }
        let key = |item: &(usize, RawMatch)| {
            let rule = &self.rules[item.0];
            (
                item.1.start,
                std::cmp::Reverse(item.1.end),
                std::cmp::Reverse(rule.confidence()),
                rule.id(),
            )
        };
        raw.sort_by(|a, b| key(a).cmp(&key(b)));
        let mut kept: Vec<(usize, RawMatch)> = Vec::new();
        for item in raw {
            if kept
                .iter()
                .any(|k| k.1.start <= item.1.start && item.1.end <= k.1.end)
            {
                continue;
            }
            kept.push(item);
        }
        let newlines: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| i)
            .collect();
        let line_bounds = |offset: usize| {
            let n = newlines.partition_point(|&p| p < offset);
            let begin = if n == 0 { 0 } else { newlines[n - 1] + 1 };
            let end = newlines.get(n).copied().unwrap_or(bytes.len());
            (n + 1, begin, end)
        };
        let marker = INLINE_ALLOW.as_bytes();
        let mut out = Vec::new();
        for (ri, m) in kept {
            let (line, b0, e0) = line_bounds(m.start);
            let (_, b1, e1) = line_bounds(m.end.saturating_sub(1).max(m.start));
            if contains(&bytes[b0..e0], marker) || contains(&bytes[b1..e1], marker) {
                continue;
            }
            let rule = &self.rules[ri];
            out.push(Finding {
                path: path.to_owned(),
                rule_id: rule.id().to_owned(),
                provider: rule.provider().to_owned(),
                start: m.start,
                end: m.end,
                line,
                redacted_preview: redact(&bytes[m.start..m.end]),
                confidence: rule.confidence(),
            });
        }
        out
    }

    fn path_allows(&self, path: &str, rule_id: &str) -> bool {
        self.options.allow.iter().any(|a| {
            (a.rules.is_empty() || a.rules.iter().any(|r| r == rule_id))
                && (a.paths.is_empty() || a.paths.iter().any(|g| glob_match(g, path)))
        })
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Shows at most a quarter of the secret (and at most four printable ASCII bytes).
fn redact(secret: &[u8]) -> String {
    let shown = (secret.len() / 4).min(4);
    let mut s: String = secret[..shown]
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect();
    s.push_str("****");
    s
}
