//! The project filter language: `status:Done,"In Progress" -label:bug assignee:@me is:issue
//! no:milestone effort:>=3 sprint:@current updated:>@today-7d fix login`.
//!
//! Terms are separated by spaces and ANDed. `-term` negates, `a,b` ORs the values of one key,
//! values with spaces are quoted, and bare words match the item title. Keys are the built-in
//! names (`is`, `no`, `has`, `assignee`, `label`, `milestone`, `repo`, `type`, `title`,
//! `author`, `state`, `reviewers`, `parent-issue`, `linked-changes`, `updated`) and the
//! project's field names. Unknown keys are `INVALID_ARGUMENT` / `unknown_filter_key`.

use std::collections::BTreeMap;
use std::ops::Bound;

use bee_proto::v1::{
    ProjectField, ProjectFieldKind, ProjectFieldValue, ProjectItemKind, project_field_value::Value,
};
use bee_search::forge::date::{CivilDate, date_bounds, parse_cmp, parse_date};

use super::model::ItemRec;
use super::*;
use crate::ids::ts_ms;

const DAY_MS: i64 = 86_400_000;

/// One parsed term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    /// `-term`.
    pub negate: bool,
    /// The key, lower-cased; empty for a bare word.
    pub key: String,
    /// The values (a bare word has one).
    pub values: Vec<String>,
}

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_filter", msg)
}

/// Splits a filter into terms.
pub fn parse(q: &str) -> Result<Vec<Term>> {
    let mut terms = Vec::new();
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let negate = chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace();
        if negate {
            i += 1;
        }
        // Read a raw token, honoring quotes.
        let mut raw: Vec<(char, bool)> = Vec::new();
        let mut in_q = false;
        while i < chars.len() && (in_q || !chars[i].is_whitespace()) {
            if chars[i] == '"' {
                in_q = !in_q;
            } else {
                raw.push((chars[i], in_q));
            }
            i += 1;
        }
        if in_q {
            return Err(bad("unterminated quote in the filter"));
        }
        // The first unquoted ':' splits the key from the values.
        let colon = raw.iter().position(|(c, q)| *c == ':' && !*q);
        match colon {
            Some(p) if p > 0 => {
                let key: String = raw[..p].iter().map(|(c, _)| *c).collect();
                let mut values = Vec::new();
                let mut cur = String::new();
                for (c, quoted) in &raw[p + 1..] {
                    if *c == ',' && !*quoted {
                        values.push(std::mem::take(&mut cur));
                    } else {
                        cur.push(*c);
                    }
                }
                values.push(cur);
                if values.iter().all(String::is_empty) {
                    return Err(bad(format!("`{key}:` needs a value")));
                }
                values.retain(|v| !v.is_empty());
                terms.push(Term {
                    negate,
                    key: key.to_lowercase(),
                    values,
                });
            }
            _ => terms.push(Term {
                negate,
                key: String::new(),
                values: vec![raw.iter().map(|(c, _)| *c).collect()],
            }),
        }
    }
    Ok(terms)
}

/// Inputs the evaluation needs, resolved by the service.
#[derive(Clone, Debug, Default)]
pub struct EvalCtx {
    /// The caller's principal id, for `@me`.
    pub me: String,
    /// Lower-cased login or handle to principal id.
    pub people: BTreeMap<String, String>,
    /// Lower-cased `owner/name` to repo id.
    pub repos: BTreeMap<String, String>,
    /// Now (ms), for `@today` and iteration keywords.
    pub now_ms: u64,
}

const BUILTIN_KEYS: &[&str] = &[
    "is",
    "no",
    "has",
    "assignee",
    "assignees",
    "label",
    "labels",
    "milestone",
    "repo",
    "repository",
    "type",
    "title",
    "author",
    "state",
    "reviewers",
    "reviewer",
    "parent-issue",
    "linked-changes",
    "updated",
    "last-updated",
];

/// A filter checked against a project's fields.
#[derive(Clone, Debug)]
pub struct Compiled {
    terms: Vec<Term>,
    fields: Vec<ProjectField>,
}

impl Compiled {
    /// Parses and validates `q` for a project with `fields`.
    pub fn new(q: &str, fields: &[ProjectField]) -> Result<Compiled> {
        let terms = parse(q)?;
        for t in &terms {
            if t.key.is_empty() || BUILTIN_KEYS.contains(&t.key.as_str()) {
                continue;
            }
            if !fields.iter().any(|f| f.name.eq_ignore_ascii_case(&t.key)) {
                return Err(ForgeError::invalid(
                    "unknown_filter_key",
                    format!(
                        "`{}` is neither a filter key nor a field of this project",
                        t.key
                    ),
                ));
            }
        }
        for t in &terms {
            if matches!(t.key.as_str(), "no" | "has") {
                for v in &t.values {
                    let known = BUILTIN_KEYS.contains(&v.to_lowercase().as_str())
                        || fields.iter().any(|f| f.name.eq_ignore_ascii_case(v));
                    if !known {
                        return Err(ForgeError::invalid(
                            "unknown_filter_key",
                            format!("`{}:{v}` names no field", t.key),
                        ));
                    }
                }
            }
        }
        Ok(Compiled {
            terms,
            fields: fields.to_vec(),
        })
    }

    /// Whether `item` passes every term.
    pub fn matches(&self, item: &ItemRec, ctx: &EvalCtx) -> bool {
        self.terms
            .iter()
            .all(|t| self.term(t, item, ctx) != t.negate)
    }

    fn field(&self, name: &str) -> Option<&ProjectField> {
        self.fields
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
    }

    fn term(&self, t: &Term, item: &ItemRec, ctx: &EvalCtx) -> bool {
        let it = item.item.clone().unwrap_or_default();
        let any = |f: &dyn Fn(&str) -> bool| t.values.iter().any(|v| f(v));
        match t.key.as_str() {
            "" => any(&|v| it.title.to_lowercase().contains(&v.to_lowercase())),
            "title" => any(&|v| it.title.to_lowercase().contains(&v.to_lowercase())),
            "is" => any(&|v| match v.to_lowercase().as_str() {
                "issue" => it.kind == ProjectItemKind::Issue as i32,
                "pr" | "cr" | "change" | "pull-request" => {
                    it.kind == ProjectItemKind::ChangeRequest as i32
                }
                "draft" => it.kind == ProjectItemKind::Draft as i32,
                "open" => it.content_state == "open" || it.content_state == "draft",
                "closed" => it.content_state == "closed" || it.content_state == "landed",
                "merged" | "landed" => it.content_state == "landed",
                "archived" => it.archived,
                _ => false,
            }),
            "state" => any(&|v| {
                let v = v.to_lowercase();
                it.content_state == v || (v == "merged" && it.content_state == "landed")
            }),
            "assignee" | "assignees" => any(&|v| {
                if v.eq_ignore_ascii_case("none") || v == "@none" {
                    return it.assignees.is_empty();
                }
                let id = resolve_person(v, ctx);
                it.assignees
                    .iter()
                    .any(|a| a.id == id || a.display.eq_ignore_ascii_case(v))
            }),
            "author" => any(&|v| item.author_id == resolve_person(v, ctx)),
            "reviewer" | "reviewers" => {
                any(&|v| item.reviewers.iter().any(|r| r.eq_ignore_ascii_case(v)))
            }
            "label" | "labels" => any(&|v| item.labels.iter().any(|l| l.eq_ignore_ascii_case(v))),
            "milestone" => any(&|v| item.milestone.eq_ignore_ascii_case(v)),
            "type" => any(&|v| item.issue_type.eq_ignore_ascii_case(v)),
            "parent-issue" => any(&|v| item.parent.to_lowercase().contains(&v.to_lowercase())),
            "linked-changes" => any(&|v| match v.parse::<u32>() {
                Ok(n) => item.linked_changes == n,
                Err(_) => item.linked_changes > 0,
            }),
            "repo" | "repository" => any(&|v| {
                ctx.repos
                    .get(&v.to_lowercase())
                    .is_some_and(|id| *id == item.repo_id)
                    || it.repo_full_name.eq_ignore_ascii_case(v)
            }),
            "updated" | "last-updated" => {
                let ms = it.updated_at.as_ref().map(ts_ms).unwrap_or(0) as i64;
                any(&|v| date_matches(v, ms, ctx.now_ms))
            }
            "no" => any(&|v| !self.has(v, item)),
            "has" => any(&|v| self.has(v, item)),
            name => {
                let Some(f) = self.field(name) else {
                    return false;
                };
                let val = it.field_values.iter().find(|x| x.field_id == f.id);
                any(&|v| field_matches(f, val, v, ctx))
            }
        }
    }

    fn has(&self, what: &str, item: &ItemRec) -> bool {
        let it = item.item.clone().unwrap_or_default();
        match what.to_lowercase().as_str() {
            "assignee" | "assignees" => !it.assignees.is_empty(),
            "label" | "labels" => !item.labels.is_empty(),
            "milestone" => !item.milestone.is_empty(),
            "type" => !item.issue_type.is_empty(),
            "parent-issue" => !item.parent.is_empty(),
            "linked-changes" => item.linked_changes > 0,
            "reviewer" | "reviewers" => !item.reviewers.is_empty(),
            "title" => !it.title.is_empty(),
            "repo" | "repository" => !item.repo_id.is_empty(),
            name => match self.field(name) {
                Some(f) => it.field_values.iter().any(|x| x.field_id == f.id),
                None => false,
            },
        }
    }
}

fn resolve_person(v: &str, ctx: &EvalCtx) -> String {
    if v == "@me" {
        return ctx.me.clone();
    }
    ctx.people
        .get(&v.to_lowercase())
        .cloned()
        .unwrap_or_else(|| v.to_string())
}

fn date_matches(v: &str, ms: i64, now_ms: u64) -> bool {
    let Some(cmp) = parse_cmp(v, &parse_date) else {
        return false;
    };
    let today = CivilDate::from_days((now_ms / DAY_MS as u64) as i64);
    let Some((min, max)) = date_bounds(&cmp, Some(today)) else {
        return false;
    };
    in_bounds(ms, &min, &max)
}

fn in_bounds(v: i64, min: &Bound<i64>, max: &Bound<i64>) -> bool {
    (match min {
        Bound::Included(m) => v >= *m,
        Bound::Excluded(m) => v > *m,
        Bound::Unbounded => true,
    }) && (match max {
        Bound::Included(m) => v <= *m,
        Bound::Excluded(m) => v < *m,
        Bound::Unbounded => true,
    })
}

/// A numeric comparison: `5`, `>5`, `>=5`, `<5`, `<=5`, `1..5`, `1..*`, `*..5`.
pub fn num_matches(spec: &str, v: f64) -> bool {
    let p = |s: &str| s.trim().parse::<f64>().ok();
    if let Some(r) = spec.strip_prefix(">=") {
        return p(r).is_some_and(|x| v >= x);
    }
    if let Some(r) = spec.strip_prefix("<=") {
        return p(r).is_some_and(|x| v <= x);
    }
    if let Some(r) = spec.strip_prefix('>') {
        return p(r).is_some_and(|x| v > x);
    }
    if let Some(r) = spec.strip_prefix('<') {
        return p(r).is_some_and(|x| v < x);
    }
    if let Some((a, b)) = spec.split_once("..") {
        let lo = if a == "*" {
            Some(f64::NEG_INFINITY)
        } else {
            p(a)
        };
        let hi = if b == "*" { Some(f64::INFINITY) } else { p(b) };
        return match (lo, hi) {
            (Some(l), Some(h)) => v >= l && v <= h,
            _ => false,
        };
    }
    p(spec).is_some_and(|x| (v - x).abs() < f64::EPSILON)
}

fn field_matches(
    f: &ProjectField,
    val: Option<&ProjectFieldValue>,
    v: &str,
    ctx: &EvalCtx,
) -> bool {
    let kind = ProjectFieldKind::try_from(f.kind).unwrap_or(ProjectFieldKind::Unspecified);
    if v == "@none" || v.eq_ignore_ascii_case("none") && kind != ProjectFieldKind::Text {
        return val.is_none();
    }
    let Some(val) = val else { return false };
    match (&val.value, kind) {
        (Some(Value::Text(t)), ProjectFieldKind::Text) => {
            t.to_lowercase().contains(&v.to_lowercase())
        }
        (Some(Value::Number(n)), ProjectFieldKind::Number) => num_matches(v, *n),
        (Some(Value::DateOn(d)), ProjectFieldKind::Date) => {
            date_matches(v, ts_ms(d) as i64, ctx.now_ms)
        }
        (Some(Value::OptionId(o)), ProjectFieldKind::SingleSelect) => f
            .options
            .iter()
            .any(|x| x.id == *o && (x.name.eq_ignore_ascii_case(v) || x.id == v)),
        (Some(Value::IterationId(i)), ProjectFieldKind::Iteration) => {
            let cfg = f.iteration_config.clone().unwrap_or_default();
            let all: Vec<_> = cfg
                .iterations
                .iter()
                .chain(cfg.completed_iterations.iter())
                .collect();
            let Some(it) = all.iter().find(|x| x.id == *i) else {
                return false;
            };
            match v {
                "@current" | "@next" | "@previous" => {
                    keyword_iteration(&cfg, v, ctx.now_ms).is_some_and(|k| k == it.id)
                }
                _ => it.title.eq_ignore_ascii_case(v) || it.id == v,
            }
        }
        _ => false,
    }
}

/// The id of the `@current`, `@next` or `@previous` iteration at `now_ms`.
pub fn keyword_iteration(
    cfg: &bee_proto::v1::ProjectIterationConfig,
    kw: &str,
    now_ms: u64,
) -> Option<String> {
    let now = now_ms as i64;
    let mut real: Vec<(i64, i64, &str)> = cfg
        .iterations
        .iter()
        .chain(cfg.completed_iterations.iter())
        .filter(|i| !i.is_break)
        .map(|i| {
            let s = i.start_on.as_ref().map(ts_ms).unwrap_or(0) as i64;
            (s, s + i64::from(i.duration_days) * DAY_MS, i.id.as_str())
        })
        .collect();
    real.sort();
    match kw {
        "@current" => real
            .iter()
            .find(|(s, e, _)| *s <= now && now < *e)
            .map(|x| x.2.to_string()),
        "@next" => real
            .iter()
            .find(|(s, _, _)| *s > now)
            .map(|x| x.2.to_string()),
        "@previous" => real
            .iter()
            .rev()
            .find(|(_, e, _)| *e <= now)
            .map(|x| x.2.to_string()),
        _ => None,
    }
}

/// The logins and repo names a filter mentions, for the service to resolve into
/// [`EvalCtx::people`] and [`EvalCtx::repos`].
pub fn references(q: &str) -> Result<(Vec<String>, Vec<String>)> {
    let (mut people, mut repos) = (Vec::new(), Vec::new());
    for t in parse(q)? {
        match t.key.as_str() {
            "assignee" | "assignees" | "author" => people.extend(
                t.values
                    .iter()
                    .filter(|v| !matches!(v.as_str(), "@me" | "@none" | "none"))
                    .cloned(),
            ),
            "repo" | "repository" => repos.extend(t.values.iter().cloned()),
            _ => {}
        }
    }
    Ok((people, repos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_on_quotes_commas_and_negation() {
        let t = parse(r#"status:Done,"In Progress" -label:bug fix login no:milestone"#).unwrap();
        assert_eq!(t.len(), 5);
        assert_eq!(t[0].values, vec!["Done", "In Progress"]);
        assert!(t[1].negate && t[1].key == "label");
        assert_eq!(
            t[2],
            Term {
                negate: false,
                key: String::new(),
                values: vec!["fix".into()]
            }
        );
        assert!(parse("status:\"oops").is_err());
        assert!(parse("status:").is_err());
    }

    #[test]
    fn numbers_compare() {
        assert!(num_matches(">=3", 3.0));
        assert!(!num_matches(">3", 3.0));
        assert!(num_matches("1..5", 5.0));
        assert!(num_matches("*..2", -1.0));
        assert!(num_matches("2.5", 2.5));
        assert!(!num_matches("x", 1.0));
    }
}
