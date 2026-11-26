//! The normalized filter a forge query compiles to.
//!
//! [`ForgeQuery::to_filter`] resolves `@me`, expands `is:` shorthands into the fields they
//! stand for, turns dates into unix-millisecond bounds, applies `in:` to free text and picks
//! the sort. The forge translates the result to its own table queries; nothing here runs a
//! query.

use std::fmt;
use std::ops::Bound;

use bee_core::{BeeError, Reason, Result};

use super::date::{CivilDate, date_bounds, num_bounds};
use super::{AgentValue, Cmp, ForgeKind, ForgeQuery, ForgeTerm, Key, PrincipalRef, Value};
use crate::query::Expr;

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SortOrder {
    /// Ascending.
    Asc,
    /// Descending (the default).
    Desc,
}

/// A sort field and order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SortSpec {
    /// Field, e.g. `created`, `stars`, `best-match`.
    pub field: &'static str,
    /// Order.
    pub order: SortOrder,
}

impl fmt::Display for SortSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = match self.order {
            SortOrder::Asc => "asc",
            SortOrder::Desc => "desc",
        };
        write!(f, "{}-{o}", self.field)
    }
}

/// Resolution inputs. Supplied by the caller so filters stay pure functions of the query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterContext {
    /// The caller's login, for `@me`.
    pub me: Option<String>,
    /// Today's date (UTC), for `@today`.
    pub today: Option<CivilDate>,
}

/// One normalized condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// Free text in the given fields (case-insensitive substring/word match is the forge's
    /// choice; the fields are the kind's defaults when `in:` was not given).
    Text {
        /// The text.
        text: String,
        /// Fields to search.
        fields: Vec<&'static str>,
    },
    /// A principal in a role: `author`, `assignee`, `commenter`, `involves`, `mentions`,
    /// `reviewed-by`, `review-requested`, `committer`.
    Person {
        /// The role.
        role: Key,
        /// The login (`@me` resolved).
        login: String,
    },
    /// The author is (or is not) an agent principal; with `login`, that agent.
    AgentAuthor {
        /// Agent (true) or not (false).
        is_agent: bool,
        /// A specific agent.
        login: Option<String>,
    },
    /// A name equals the value (case-insensitive): label, milestone, project, head, base,
    /// repo, org, user, language, topic, license, team, category, location, hashes, emails.
    Name {
        /// The field.
        field: Key,
        /// The value.
        value: String,
    },
    /// A change request touches a path at or under this prefix (no leading `/`).
    PathPrefix(String),
    /// A field equals one of its fixed words: `state` (open, closed, merged), `type`
    /// (issue, cr, user, org, agent, npm ...), `review`, `status`, `linked`, `reason`,
    /// `visibility`, `fork`, `answered`, `featured` ...
    Enum {
        /// The field.
        field: &'static str,
        /// The word.
        value: &'static str,
    },
    /// A boolean field: `draft`, `locked`, `archived`, `mirror`, `template`, `merge`,
    /// `sponsorable`, `curated` ...
    Flag {
        /// The field.
        field: &'static str,
        /// The value.
        value: bool,
    },
    /// A numeric or time field within bounds. Times are unix milliseconds; `risk` is the
    /// index into [`super::RISK_CLASSES`].
    Range {
        /// The field.
        field: Key,
        /// Lower bound.
        min: Bound<i64>,
        /// Upper bound.
        max: Bound<i64>,
    },
    /// The field has no value (`no:label`).
    Missing(&'static str),
}

/// A boolean combination of predicates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Everything.
    All,
    /// One predicate.
    Pred(Predicate),
    /// Negation.
    Not(Box<Filter>),
    /// Conjunction.
    And(Vec<Filter>),
    /// Disjunction.
    Or(Vec<Filter>),
}

/// A compiled forge query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledFilter {
    /// The grammar.
    pub kind: ForgeKind,
    /// The condition.
    pub filter: Filter,
    /// The sort (`best-match` descending by default).
    pub sort: SortSpec,
}

fn default_fields(kind: ForgeKind) -> Vec<&'static str> {
    match kind {
        ForgeKind::Issues | ForgeKind::Discussions => vec!["title", "body"],
        ForgeKind::Repos => vec!["name", "description"],
        ForgeKind::Changesets => vec!["message"],
        ForgeKind::Users => vec!["login", "name"],
        ForgeKind::Topics => vec!["name", "description"],
        ForgeKind::Packages => vec!["name", "description"],
    }
}

fn pred(p: Predicate) -> Filter {
    Filter::Pred(p)
}

fn invalid(msg: String) -> BeeError {
    BeeError::new(Reason::InvalidArgument, msg).with_payload("subcode", crate::query::BAD_QUERY)
}

impl ForgeQuery {
    /// Compiles to a normalized filter. Fails with `INVALID_ARGUMENT` (subcode `bad_query`)
    /// when `@me` or `@today` is used without the matching context.
    pub fn to_filter(&self, ctx: &FilterContext) -> Result<CompiledFilter> {
        let fields = if self.in_fields.is_empty() {
            default_fields(self.kind)
        } else {
            self.in_fields.clone()
        };
        let filter = match &self.expr {
            None => Filter::All,
            Some(e) => convert(e, self.kind, &fields, ctx)?,
        };
        Ok(CompiledFilter {
            kind: self.kind,
            filter,
            sort: self.sort.unwrap_or(SortSpec {
                field: "best-match",
                order: SortOrder::Desc,
            }),
        })
    }
}

fn convert(
    e: &Expr<ForgeTerm>,
    kind: ForgeKind,
    fields: &[&'static str],
    ctx: &FilterContext,
) -> Result<Filter> {
    Ok(match e {
        Expr::Term(t) => term(t, kind, fields, ctx)?,
        Expr::Not(inner) => Filter::Not(Box::new(convert(inner, kind, fields, ctx)?)),
        Expr::And(v) => Filter::And(
            v.iter()
                .map(|c| convert(c, kind, fields, ctx))
                .collect::<Result<_>>()?,
        ),
        Expr::Or(v) => Filter::Or(
            v.iter()
                .map(|c| convert(c, kind, fields, ctx))
                .collect::<Result<_>>()?,
        ),
    })
}

fn login(p: &PrincipalRef, ctx: &FilterContext) -> Result<String> {
    match p {
        PrincipalRef::Login(l) => Ok(l.clone()),
        PrincipalRef::Me => ctx
            .me
            .clone()
            .ok_or_else(|| invalid("@me needs an authenticated caller".into())),
    }
}

fn is_word(kind: ForgeKind, w: &'static str) -> Filter {
    let e = |field, value| pred(Predicate::Enum { field, value });
    let f = |field, value| pred(Predicate::Flag { field, value });
    match (kind, w) {
        (ForgeKind::Issues, "open" | "closed" | "merged") => e("state", w),
        (ForgeKind::Issues, "unmerged") => Filter::And(vec![
            e("type", "cr"),
            Filter::Not(Box::new(e("state", "merged"))),
        ]),
        (ForgeKind::Issues, "issue") => e("type", "issue"),
        (ForgeKind::Issues, "pr" | "cr") => e("type", "cr"),
        (ForgeKind::Issues, "draft") => f("draft", true),
        (ForgeKind::Issues, "queued") => f("queued", true),
        (_, "locked") => f("locked", true),
        (_, "unlocked") => f("locked", false),
        (_, "public" | "private" | "internal") => e("visibility", w),
        (ForgeKind::Repos, "fork") => f("fork", true),
        (_, "archived") => f("archived", true),
        (ForgeKind::Discussions, "answered") => f("answered", true),
        (ForgeKind::Discussions, "unanswered") => f("answered", false),
        (ForgeKind::Discussions, "open" | "closed") => e("state", w),
        (_, other) => f(other, true),
    }
}

fn term(
    t: &ForgeTerm,
    kind: ForgeKind,
    fields: &[&'static str],
    ctx: &FilterContext,
) -> Result<Filter> {
    let Some(key) = t.key else {
        let Value::Text(text) = &t.value else {
            return Err(BeeError::new(Reason::Internal, "free text without text"));
        };
        return Ok(pred(Predicate::Text {
            text: text.clone(),
            fields: fields.to_vec(),
        }));
    };
    Ok(match (&t.value, key) {
        (Value::Enum(w), Key::Is) => is_word(kind, w),
        (Value::Enum(w), Key::State) => pred(Predicate::Enum {
            field: "state",
            value: w,
        }),
        (Value::Enum(w), Key::Type) => pred(Predicate::Enum {
            field: "type",
            value: if *w == "pr" { "cr" } else { w },
        }),
        (Value::Enum(w), Key::No) => pred(Predicate::Missing(w)),
        (Value::Enum(w), Key::Linked) => pred(Predicate::Enum {
            field: "linked",
            value: if *w == "pr" { "cr" } else { w },
        }),
        (Value::Enum(w), k) => pred(Predicate::Enum {
            field: k.as_str(),
            value: w,
        }),
        (Value::Principal(p), role) => pred(Predicate::Person {
            role,
            login: login(p, ctx)?,
        }),
        (Value::Agent(AgentValue::Any(b)), _) => pred(Predicate::AgentAuthor {
            is_agent: *b,
            login: None,
        }),
        (Value::Agent(AgentValue::Named(n)), _) => pred(Predicate::AgentAuthor {
            is_agent: true,
            login: Some(n.clone()),
        }),
        (Value::Text(v), Key::Path) => {
            pred(Predicate::PathPrefix(v.trim_start_matches('/').to_string()))
        }
        (Value::Text(v), field) => pred(Predicate::Name {
            field,
            value: v.clone(),
        }),
        (Value::Bool(b), k) => pred(Predicate::Flag {
            field: k.as_str(),
            value: *b,
        }),
        (Value::Num(c), field) => {
            let (min, max) = num_bounds(c);
            pred(Predicate::Range { field, min, max })
        }
        (Value::Date(c), field) => {
            let (min, max) = date_bounds(c, ctx.today)
                .ok_or_else(|| invalid("@today needs today's date in the filter context".into()))?;
            pred(Predicate::Range { field, min, max })
        }
        (Value::Risk(c), field) => {
            let as_u64 = match c {
                Cmp::Eq(i) => Cmp::Eq(*i as u64),
                Cmp::Gt(i) => Cmp::Gt(*i as u64),
                Cmp::Ge(i) => Cmp::Ge(*i as u64),
                Cmp::Lt(i) => Cmp::Lt(*i as u64),
                Cmp::Le(i) => Cmp::Le(*i as u64),
                Cmp::Between(a, b) => Cmp::Between(a.map(|i| i as u64), b.map(|i| i as u64)),
            };
            let (min, max) = num_bounds(&as_u64);
            pred(Predicate::Range { field, min, max })
        }
        (Value::List(_) | Value::Sort(_), _) => {
            return Err(BeeError::new(
                Reason::Internal,
                "modifier left in the expression",
            ));
        }
    })
}
