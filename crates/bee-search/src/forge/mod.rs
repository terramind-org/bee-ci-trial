//! Forge query languages: GitHub-compatible qualifiers for issues and change requests,
//! repos, changesets (commits), users, topics, discussions and packages.
//!
//! This module only parses and validates; it never touches forge tables. The forge
//! translates [`Filter`] (from [`ForgeQuery::to_filter`]) into its own storage queries, so
//! list views keep working from forge tables with no dependency on a search index
//! (PRODUCT §15.1). See docs/spec/search.md for the full qualifier reference.

pub mod date;
pub mod filter;

use std::fmt;

pub use date::{CivilDate, Cmp, DateValue};
pub use filter::{CompiledFilter, Filter, FilterContext, Predicate, SortOrder, SortSpec};

use crate::query::{Atom, Expr, LexOptions, QueryError, Span, ValueKind, quote_value};

/// Which forge grammar a query uses (mirrors `bee.v1.SearchQueryKind`, minus code).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ForgeKind {
    /// Issues and change requests.
    Issues,
    /// Repositories.
    Repos,
    /// Changesets (commits).
    Changesets,
    /// Users, agents and orgs.
    Users,
    /// Topics.
    Topics,
    /// Discussions.
    Discussions,
    /// Packages.
    Packages,
}

impl ForgeKind {
    /// Every kind.
    pub const ALL: [ForgeKind; 7] = [
        ForgeKind::Issues,
        ForgeKind::Repos,
        ForgeKind::Changesets,
        ForgeKind::Users,
        ForgeKind::Topics,
        ForgeKind::Discussions,
        ForgeKind::Packages,
    ];

    /// Lower-case name.
    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::Issues => "issues",
            ForgeKind::Repos => "repos",
            ForgeKind::Changesets => "changesets",
            ForgeKind::Users => "users",
            ForgeKind::Topics => "topics",
            ForgeKind::Discussions => "discussions",
            ForgeKind::Packages => "packages",
        }
    }
}

macro_rules! keys {
    ($( $(#[$doc:meta])* $variant:ident => $text:literal, )*) => {
        /// A forge qualifier key.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Key {
            $( $(#[$doc])* $variant, )*
        }

        impl Key {
            /// Every key.
            pub const ALL: &'static [Key] = &[ $( Key::$variant, )* ];

            /// The canonical qualifier name.
            pub fn as_str(self) -> &'static str {
                match self { $( Key::$variant => $text, )* }
            }
        }
    };
}

keys! {
    /// `is:` state and type shorthands.
    Is => "is",
    /// `state:` open/closed/merged.
    State => "state",
    /// `type:` item or account type, or package ecosystem.
    Type => "type",
    /// `author:`.
    Author => "author",
    /// `assignee:`.
    Assignee => "assignee",
    /// `commenter:`.
    Commenter => "commenter",
    /// `involves:` author, assignee, commenter or mentioned.
    Involves => "involves",
    /// `mentions:`.
    Mentions => "mentions",
    /// `reviewed-by:`.
    ReviewedBy => "reviewed-by",
    /// `review-requested:`.
    ReviewRequested => "review-requested",
    /// `team-review-requested:` (`org/team`).
    TeamReviewRequested => "team-review-requested",
    /// `team:` mentions a team.
    Team => "team",
    /// `label:`.
    Label => "label",
    /// `milestone:`.
    Milestone => "milestone",
    /// `project:`.
    Project => "project",
    /// `review:` decision.
    Review => "review",
    /// `status:` combined checks state.
    Status => "status",
    /// `in:` text fields (modifier).
    In => "in",
    /// `created:`.
    Created => "created",
    /// `updated:`.
    Updated => "updated",
    /// `closed:`.
    Closed => "closed",
    /// `merged:` (landed) date.
    Merged => "merged",
    /// `pushed:`.
    Pushed => "pushed",
    /// `comments:` count.
    Comments => "comments",
    /// `reactions:` count.
    Reactions => "reactions",
    /// `interactions:` count.
    Interactions => "interactions",
    /// `no:` missing metadata.
    No => "no",
    /// `head:` branch.
    Head => "head",
    /// `base:` branch.
    Base => "base",
    /// `path:` CR touches paths under this prefix (Beehive).
    Path => "path",
    /// `risk:` semantic diff class (Beehive, SPEC §11.3).
    Risk => "risk",
    /// `agent:` authored by an agent principal (Beehive).
    Agent => "agent",
    /// `repo:` `org/name`.
    Repo => "repo",
    /// `org:`.
    Org => "org",
    /// `user:` owner.
    User => "user",
    /// `language:`.
    Language => "language",
    /// `linked:` linked item type.
    Linked => "linked",
    /// `reason:` close reason.
    Reason => "reason",
    /// `archived:`.
    Archived => "archived",
    /// `draft:`.
    Draft => "draft",
    /// `topic:`.
    Topic => "topic",
    /// `topics:` count.
    Topics => "topics",
    /// `stars:`.
    Stars => "stars",
    /// `forks:`.
    Forks => "forks",
    /// `fork:` include forks.
    Fork => "fork",
    /// `size:` in KB.
    Size => "size",
    /// `license:`.
    License => "license",
    /// `followers:`.
    Followers => "followers",
    /// `good-first-issues:`.
    GoodFirstIssues => "good-first-issues",
    /// `help-wanted-issues:`.
    HelpWantedIssues => "help-wanted-issues",
    /// `mirror:`.
    Mirror => "mirror",
    /// `template:`.
    Template => "template",
    /// `committer:`.
    Committer => "committer",
    /// `author-name:`.
    AuthorName => "author-name",
    /// `committer-name:`.
    CommitterName => "committer-name",
    /// `author-email:`.
    AuthorEmail => "author-email",
    /// `committer-email:`.
    CommitterEmail => "committer-email",
    /// `author-date:`.
    AuthorDate => "author-date",
    /// `committer-date:`.
    CommitterDate => "committer-date",
    /// `merge:` merge changesets only (or none).
    Merge => "merge",
    /// `hash:` changeset id or prefix.
    Hash => "hash",
    /// `parent:` parent id or prefix.
    Parent => "parent",
    /// `tree:` root tree id or prefix.
    Tree => "tree",
    /// `change-id:` bee ChangeId.
    ChangeId => "change-id",
    /// `repos:` count.
    Repos => "repos",
    /// `repositories:` count (topics).
    Repositories => "repositories",
    /// `location:`.
    Location => "location",
    /// `category:` (discussions).
    Category => "category",
    /// `sort:` (modifier).
    Sort => "sort",
}

/// How a qualifier's value is parsed.
#[derive(Clone, Copy, Debug)]
enum ValueType {
    Text,
    Principal,
    Enum(&'static [&'static str]),
    Bool,
    Num,
    Date,
    List(&'static [&'static str]),
    Agent,
    Risk,
    Sort(&'static [&'static str]),
}

struct Spec {
    key: Key,
    aliases: &'static [&'static str],
    kinds: &'static [ForgeKind],
    value: ValueType,
}

use ForgeKind::{
    Changesets as C, Discussions as D, Issues as I, Packages as P, Repos as R, Topics as T,
    Users as U,
};

const fn s(key: Key, kinds: &'static [ForgeKind], value: ValueType) -> Spec {
    Spec {
        key,
        aliases: &[],
        kinds,
        value,
    }
}

/// The semantic diff classes in risk order (SPEC §11.3).
pub const RISK_CLASSES: &[&str] = &[
    "generated",
    "docs",
    "comment_only",
    "test_only",
    "rename_only",
    "dependency",
    "logic",
    "sensitive",
];

static SPECS: &[Spec] = &[
    s(
        Key::Is,
        &[I],
        ValueType::Enum(&[
            "open", "closed", "merged", "unmerged", "draft", "issue", "pr", "cr", "locked",
            "unlocked", "public", "private", "archived", "queued",
        ]),
    ),
    s(
        Key::Is,
        &[R],
        ValueType::Enum(&[
            "public",
            "private",
            "internal",
            "fork",
            "template",
            "archived",
            "mirror",
            "sponsorable",
        ]),
    ),
    s(Key::Is, &[C], ValueType::Enum(&["public", "private"])),
    s(Key::Is, &[U], ValueType::Enum(&["sponsorable"])),
    s(Key::Is, &[T], ValueType::Enum(&["featured", "curated"])),
    s(
        Key::Is,
        &[D],
        ValueType::Enum(&[
            "answered",
            "unanswered",
            "open",
            "closed",
            "locked",
            "unlocked",
        ]),
    ),
    s(
        Key::State,
        &[I],
        ValueType::Enum(&["open", "closed", "merged"]),
    ),
    s(Key::Type, &[I], ValueType::Enum(&["issue", "pr", "cr"])),
    s(Key::Type, &[U], ValueType::Enum(&["user", "org", "agent"])),
    s(
        Key::Type,
        &[P],
        ValueType::Enum(&[
            "npm",
            "container",
            "maven",
            "rubygems",
            "nuget",
            "cargo",
            "pypi",
            "go",
        ]),
    ),
    s(Key::Author, &[I, C, D], ValueType::Principal),
    s(Key::Assignee, &[I], ValueType::Principal),
    s(Key::Commenter, &[I, D], ValueType::Principal),
    s(Key::Involves, &[I, D], ValueType::Principal),
    s(Key::Mentions, &[I], ValueType::Principal),
    s(Key::ReviewedBy, &[I], ValueType::Principal),
    s(Key::ReviewRequested, &[I], ValueType::Principal),
    s(Key::TeamReviewRequested, &[I], ValueType::Text),
    s(Key::Team, &[I], ValueType::Text),
    s(Key::Label, &[I, D], ValueType::Text),
    s(Key::Milestone, &[I], ValueType::Text),
    s(Key::Project, &[I], ValueType::Text),
    s(
        Key::Review,
        &[I],
        ValueType::Enum(&["none", "required", "approved", "changes_requested"]),
    ),
    s(
        Key::Status,
        &[I],
        ValueType::Enum(&["success", "failure", "pending"]),
    ),
    s(
        Key::In,
        &[I, D],
        ValueType::List(&["title", "body", "comments"]),
    ),
    s(
        Key::In,
        &[R],
        ValueType::List(&["name", "description", "readme", "topics"]),
    ),
    s(Key::In, &[U], ValueType::List(&["login", "name", "email"])),
    s(Key::Created, &[I, R, U, T, D], ValueType::Date),
    s(Key::Updated, &[I, D], ValueType::Date),
    s(Key::Closed, &[I], ValueType::Date),
    s(Key::Merged, &[I], ValueType::Date),
    s(Key::Pushed, &[R], ValueType::Date),
    s(Key::Comments, &[I, D], ValueType::Num),
    s(Key::Reactions, &[I], ValueType::Num),
    s(Key::Interactions, &[I], ValueType::Num),
    s(
        Key::No,
        &[I],
        ValueType::Enum(&["label", "milestone", "assignee", "project", "reviewer"]),
    ),
    s(Key::Head, &[I], ValueType::Text),
    s(Key::Base, &[I], ValueType::Text),
    s(Key::Path, &[I], ValueType::Text),
    s(Key::Risk, &[I, C], ValueType::Risk),
    s(Key::Agent, &[I, C], ValueType::Agent),
    s(Key::Repo, &[I, R, C, D, P], ValueType::Text),
    s(Key::Org, &[I, R, C, D, P], ValueType::Text),
    s(Key::User, &[I, R, C, D, P], ValueType::Text),
    s(Key::Language, &[I, R, U], ValueType::Text),
    s(Key::Linked, &[I], ValueType::Enum(&["pr", "cr", "issue"])),
    s(
        Key::Reason,
        &[I],
        ValueType::Enum(&["completed", "not-planned", "duplicate"]),
    ),
    s(Key::Archived, &[I, R], ValueType::Bool),
    s(Key::Draft, &[I], ValueType::Bool),
    s(Key::Topic, &[R], ValueType::Text),
    s(Key::Topics, &[R], ValueType::Num),
    s(Key::Stars, &[R], ValueType::Num),
    s(Key::Forks, &[R], ValueType::Num),
    s(Key::Fork, &[R], ValueType::Enum(&["true", "false", "only"])),
    s(Key::Size, &[R], ValueType::Num),
    s(Key::License, &[R], ValueType::Text),
    s(Key::Followers, &[R, U], ValueType::Num),
    s(Key::GoodFirstIssues, &[R], ValueType::Num),
    s(Key::HelpWantedIssues, &[R], ValueType::Num),
    s(Key::Mirror, &[R], ValueType::Bool),
    s(Key::Template, &[R], ValueType::Bool),
    s(Key::Committer, &[C], ValueType::Principal),
    s(Key::AuthorName, &[C], ValueType::Text),
    s(Key::CommitterName, &[C], ValueType::Text),
    s(Key::AuthorEmail, &[C], ValueType::Text),
    s(Key::CommitterEmail, &[C], ValueType::Text),
    s(Key::AuthorDate, &[C], ValueType::Date),
    s(Key::CommitterDate, &[C], ValueType::Date),
    s(Key::Merge, &[C], ValueType::Bool),
    s(Key::Hash, &[C], ValueType::Text),
    s(Key::Parent, &[C], ValueType::Text),
    s(Key::Tree, &[C], ValueType::Text),
    s(Key::ChangeId, &[C], ValueType::Text),
    s(Key::Repos, &[U], ValueType::Num),
    s(Key::Repositories, &[T], ValueType::Num),
    s(Key::Location, &[U], ValueType::Text),
    s(Key::Category, &[D], ValueType::Text),
    s(
        Key::Sort,
        &[I],
        ValueType::Sort(&[
            "created",
            "updated",
            "comments",
            "reactions",
            "interactions",
            "best-match",
        ]),
    ),
    s(
        Key::Sort,
        &[R],
        ValueType::Sort(&[
            "stars",
            "forks",
            "updated",
            "help-wanted-issues",
            "best-match",
        ]),
    ),
    s(
        Key::Sort,
        &[C],
        ValueType::Sort(&["author-date", "committer-date", "best-match"]),
    ),
    s(
        Key::Sort,
        &[U],
        ValueType::Sort(&["followers", "repositories", "joined", "best-match"]),
    ),
    s(
        Key::Sort,
        &[D],
        ValueType::Sort(&["created", "updated", "comments", "best-match"]),
    ),
    s(
        Key::Sort,
        &[P],
        ValueType::Sort(&["downloads", "updated", "best-match"]),
    ),
    Spec {
        key: Key::Language,
        aliases: &["lang"],
        kinds: &[],
        value: ValueType::Text,
    },
];

/// Alias spellings accepted for keys (`lang:` → `language:`).
fn canonical_key(name: &str) -> Option<Key> {
    Key::ALL
        .iter()
        .copied()
        .find(|k| k.as_str() == name)
        .or_else(|| {
            SPECS
                .iter()
                .find(|s| s.aliases.contains(&name))
                .map(|s| s.key)
        })
}

fn spec_for(key: Key, kind: ForgeKind) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|s| s.key == key && s.kinds.contains(&kind))
}

/// A user, agent or org reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrincipalRef {
    /// `@me`: the caller.
    Me,
    /// A login.
    Login(String),
}

impl fmt::Display for PrincipalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrincipalRef::Me => f.write_str("@me"),
            PrincipalRef::Login(l) => f.write_str(l),
        }
    }
}

/// The `agent:` qualifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentValue {
    /// `agent:true` / `agent:false`: authored (or not) by any agent principal.
    Any(bool),
    /// `agent:<login>`: authored by this agent.
    Named(String),
}

/// A typed qualifier value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    /// Free text or a name.
    Text(String),
    /// A principal.
    Principal(PrincipalRef),
    /// One of the qualifier's allowed words.
    Enum(&'static str),
    /// `true` / `false`.
    Bool(bool),
    /// A numeric comparison.
    Num(Cmp<u64>),
    /// A date comparison.
    Date(Cmp<DateValue>),
    /// A list of allowed words (`in:title,body`).
    List(Vec<&'static str>),
    /// `agent:`.
    Agent(AgentValue),
    /// `risk:` a comparison over the risk order, as class indexes into [`RISK_CLASSES`].
    Risk(Cmp<usize>),
    /// `sort:` field and order.
    Sort(SortSpec),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Text(t) => f.write_str(&quote_value(t, ValueKind::Bare)),
            Value::Principal(p) => write!(f, "{p}"),
            Value::Enum(e) => f.write_str(e),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(c) => write!(f, "{c}"),
            Value::Date(c) => write!(f, "{c}"),
            Value::List(l) => f.write_str(&l.join(",")),
            Value::Agent(AgentValue::Any(b)) => write!(f, "{b}"),
            Value::Agent(AgentValue::Named(n)) => f.write_str(n),
            Value::Risk(c) => {
                let name = |i: &usize| RISK_CLASSES[*i];
                let s = match c {
                    Cmp::Eq(i) => name(i).to_string(),
                    Cmp::Gt(i) => format!(">{}", name(i)),
                    Cmp::Ge(i) => format!(">={}", name(i)),
                    Cmp::Lt(i) => format!("<{}", name(i)),
                    Cmp::Le(i) => format!("<={}", name(i)),
                    Cmp::Between(a, b) => format!(
                        "{}..{}",
                        a.as_ref().map_or("*", name),
                        b.as_ref().map_or("*", name)
                    ),
                };
                f.write_str(&s)
            }
            Value::Sort(s) => write!(f, "{s}"),
        }
    }
}

/// One condition of a forge query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeTerm {
    /// The qualifier; `None` for free text.
    pub key: Option<Key>,
    /// The typed value.
    pub value: Value,
    /// Position in the query.
    pub span: Span,
}

impl ForgeTerm {
    /// Canonical text.
    pub fn render(&self) -> String {
        match self.key {
            None => match &self.value {
                Value::Text(t) => quote_value(t, ValueKind::Quoted),
                other => other.to_string(),
            },
            Some(k) => format!("{}:{}", k.as_str(), self.value),
        }
    }
}

/// A parsed term with its query-string details (mirrors `bee.v1.SearchQualifier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeQualifierInfo {
    /// Canonical key; empty for free text.
    pub key: String,
    /// Value as written.
    pub value: String,
    /// Under an odd number of negations.
    pub negated: bool,
    /// Position in the query.
    pub span: Span,
    /// Canonical value.
    pub resolved: String,
}

/// A parsed, validated forge query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeQuery {
    /// The grammar.
    pub kind: ForgeKind,
    /// The condition; `None` matches everything (e.g. a query of only `sort:`).
    pub expr: Option<Expr<ForgeTerm>>,
    /// `sort:`, if given.
    pub sort: Option<SortSpec>,
    /// `in:` fields free text applies to (empty = the kind's default fields).
    pub in_fields: Vec<&'static str>,
    /// Every term, for `ParseSearchQuery`.
    pub qualifiers: Vec<ForgeQualifierInfo>,
}

impl ForgeQuery {
    /// Canonical text: keys canonical and lower-case, values normalized, modifiers last.
    pub fn normalized(&self) -> String {
        let mut parts = Vec::new();
        if let Some(e) = &self.expr {
            parts.push(e.render(&ForgeTerm::render));
        }
        if !self.in_fields.is_empty() {
            parts.push(format!("in:{}", self.in_fields.join(",")));
        }
        if let Some(s) = &self.sort {
            parts.push(format!("sort:{s}"));
        }
        parts.join(" ")
    }
}

fn parse_value(spec: &Spec, atom: &Atom) -> std::result::Result<Value, QueryError> {
    let v = atom.value.as_str();
    let bad = |what: String| QueryError::new("bad_value", atom.value_span, what);
    let key = spec.key.as_str();
    if v.is_empty() {
        return Err(bad(format!("{key}: needs a value")));
    }
    Ok(match spec.value {
        ValueType::Text => Value::Text(v.to_string()),
        ValueType::Principal => {
            if v.eq_ignore_ascii_case("@me") {
                Value::Principal(PrincipalRef::Me)
            } else if v.chars().any(char::is_whitespace) || v.starts_with('@') {
                return Err(bad(format!("{key}: {v:?} is not a login")));
            } else {
                Value::Principal(PrincipalRef::Login(v.to_string()))
            }
        }
        ValueType::Enum(allowed) => {
            let lower = v.to_ascii_lowercase();
            match allowed.iter().find(|a| **a == lower) {
                Some(a) => Value::Enum(a),
                None => {
                    return Err(bad(format!(
                        "{key}: expected one of {}, got {v:?}",
                        allowed.join(", ")
                    )));
                }
            }
        }
        ValueType::Bool => match v.to_ascii_lowercase().as_str() {
            "true" | "yes" => Value::Bool(true),
            "false" | "no" => Value::Bool(false),
            _ => return Err(bad(format!("{key}: expected true or false"))),
        },
        ValueType::Num => Value::Num(
            date::parse_cmp(v, &date::parse_u64)
                .ok_or_else(|| bad(format!("{key}: {v:?} is not a number or range")))?,
        ),
        ValueType::Date => Value::Date(date::parse_cmp(v, &date::parse_date).ok_or_else(|| {
            QueryError::new(
                "bad_date",
                atom.value_span,
                format!("{key}: {v:?} is not a date (YYYY-MM-DD, ISO 8601 or @today) or range"),
            )
        })?),
        ValueType::List(allowed) => {
            let mut out: Vec<&'static str> = Vec::new();
            for part in v.split(',') {
                let lower = part.trim().to_ascii_lowercase();
                match allowed.iter().find(|a| **a == lower) {
                    Some(a) => {
                        if !out.contains(a) {
                            out.push(a);
                        }
                    }
                    None => {
                        return Err(bad(format!(
                            "{key}: expected a list of {}, got {part:?}",
                            allowed.join(", ")
                        )));
                    }
                }
            }
            Value::List(out)
        }
        ValueType::Agent => match v.to_ascii_lowercase().as_str() {
            "true" | "yes" => Value::Agent(AgentValue::Any(true)),
            "false" | "no" => Value::Agent(AgentValue::Any(false)),
            _ if v.chars().any(char::is_whitespace) => {
                return Err(bad(format!("agent: {v:?} is not a login")));
            }
            _ => Value::Agent(AgentValue::Named(v.to_string())),
        },
        ValueType::Risk => {
            let class = |x: &str| {
                let l = x.to_ascii_lowercase();
                RISK_CLASSES.iter().position(|c| *c == l)
            };
            Value::Risk(date::parse_cmp(v, &class).ok_or_else(|| {
                bad(format!(
                    "risk: expected a class ({}) or a comparison",
                    RISK_CLASSES.join(", ")
                ))
            })?)
        }
        ValueType::Sort(allowed) => {
            let lower = v.to_ascii_lowercase();
            let (field, order) = if let Some(f) = lower.strip_suffix("-asc") {
                (f.to_string(), SortOrder::Asc)
            } else if let Some(f) = lower.strip_suffix("-desc") {
                (f.to_string(), SortOrder::Desc)
            } else {
                (lower.clone(), SortOrder::Desc)
            };
            match allowed.iter().find(|a| **a == field) {
                Some(a) => Value::Sort(SortSpec { field: a, order }),
                None => {
                    return Err(bad(format!(
                        "sort: expected one of {} (optionally -asc or -desc)",
                        allowed.join(", ")
                    )));
                }
            }
        }
    })
}

fn is_forge_key(k: &str) -> bool {
    canonical_key(k).is_some()
}

/// Parses and validates a forge query of `kind`. All errors are reported, with positions.
pub fn parse_forge_query(
    kind: ForgeKind,
    query: &str,
) -> std::result::Result<ForgeQuery, Vec<QueryError>> {
    let opts = LexOptions {
        regex: false,
        is_qualifier: &is_forge_key,
        strict_keys: true,
    };
    let (expr, mut errors) = crate::query::parse(query, &opts);
    let modifier = |a: &Atom| {
        matches!(
            a.key.as_deref().and_then(canonical_key),
            Some(Key::Sort) | Some(Key::In)
        )
    };
    let (expr, modifiers) = crate::query::extract_modifiers(expr, &modifier, &mut |_| {});
    let mut sort = None;
    let mut in_fields: Vec<&'static str> = Vec::new();
    let mut qualifiers = Vec::new();
    let mut seen_in = false;
    for m in &modifiers {
        let Some(key) = m.key.as_deref().and_then(canonical_key) else {
            continue;
        };
        let Some(spec) = spec_for(key, kind) else {
            errors.push(QueryError::new(
                "wrong_kind",
                m.span,
                format!("{}: does not apply to {} search", key.as_str(), kind.name()),
            ));
            continue;
        };
        match parse_value(spec, m) {
            Ok(Value::Sort(s)) => {
                if sort.is_some() {
                    errors.push(QueryError::new(
                        "duplicate_qualifier",
                        m.span,
                        "sort: given more than once",
                    ));
                }
                qualifiers.push(ForgeQualifierInfo {
                    key: "sort".into(),
                    value: m.value.clone(),
                    negated: false,
                    span: m.span,
                    resolved: s.to_string(),
                });
                sort = Some(s);
            }
            Ok(Value::List(l)) => {
                if seen_in {
                    errors.push(QueryError::new(
                        "duplicate_qualifier",
                        m.span,
                        "in: given more than once",
                    ));
                }
                seen_in = true;
                qualifiers.push(ForgeQualifierInfo {
                    key: "in".into(),
                    value: m.value.clone(),
                    negated: false,
                    span: m.span,
                    resolved: l.join(","),
                });
                in_fields = l;
            }
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    let expr = expr.and_then(|e| {
        e.try_map(
            &mut |a: Atom, negated: bool| -> std::result::Result<ForgeTerm, QueryError> {
                let term = match a.key.as_deref() {
                    None => ForgeTerm {
                        key: None,
                        value: Value::Text(a.value.clone()),
                        span: a.span,
                    },
                    Some(name) => {
                        let Some(key) = canonical_key(name) else {
                            // Already reported as unknown by the lexer; keep it as text so
                            // the remaining terms still validate.
                            return Ok(ForgeTerm {
                                key: None,
                                value: Value::Text(a.value.clone()),
                                span: a.span,
                            });
                        };
                        if matches!(key, Key::Sort | Key::In) {
                            return Err(QueryError::new(
                                "misplaced_qualifier",
                                a.span,
                                format!(
                                    "{}: applies to the whole query; use it at the top level",
                                    key.as_str()
                                ),
                            ));
                        }
                        let spec = spec_for(key, kind).ok_or_else(|| {
                            QueryError::new(
                                "wrong_kind",
                                a.span,
                                format!(
                                    "{}: does not apply to {} search",
                                    key.as_str(),
                                    kind.name()
                                ),
                            )
                        })?;
                        ForgeTerm {
                            key: Some(key),
                            value: parse_value(spec, &a)?,
                            span: a.span,
                        }
                    }
                };
                qualifiers.push(ForgeQualifierInfo {
                    key: term.key.map_or(String::new(), |k| k.as_str().to_string()),
                    value: a.value.clone(),
                    negated,
                    span: a.span,
                    resolved: term.value.to_string(),
                });
                Ok(term)
            },
            &mut errors,
        )
    });
    if !errors.is_empty() {
        errors.sort_by_key(|e| e.span);
        errors.dedup();
        return Err(errors);
    }
    qualifiers.sort_by_key(|q| q.span);
    Ok(ForgeQuery {
        kind,
        expr,
        sort,
        in_fields,
        qualifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_a_spec() {
        for k in Key::ALL {
            assert!(
                SPECS.iter().any(|s| s.key == *k && !s.kinds.is_empty()),
                "{k:?} has no spec"
            );
        }
        assert_eq!(canonical_key("lang"), Some(Key::Language));
    }

    #[test]
    fn basic() {
        let q = parse_forge_query(
            ForgeKind::Issues,
            "is:open label:\"good first issue\" sort:updated-asc",
        )
        .unwrap();
        assert_eq!(
            q.normalized(),
            "is:open label:\"good first issue\" sort:updated-asc"
        );
        assert_eq!(q.qualifiers.len(), 3);
        let e = parse_forge_query(ForgeKind::Repos, "review:approved").unwrap_err();
        assert_eq!(e[0].subcode, "wrong_kind");
    }
}
