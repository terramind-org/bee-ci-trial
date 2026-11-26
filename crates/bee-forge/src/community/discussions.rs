//! Discussions (PRODUCT §4.16): categories with Q&A, announcement and poll formats, threaded
//! comments, accepted answers, polls, upvotes, pins, locks, labels, discussion forms and
//! conversion to and from issues.
//!
//! # Model
//!
//! A discussion lives in a *home*: a repo or an org. Numbers are per home (a counter row
//! allocates them in the creating transaction). A discussion's public id is
//! `<home id>.<number>` and a comment's is `<discussion id>.<ULID>`, so both resolve to their
//! rows without an index.
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | categories | home | ULID | `k1` slug (unique), `n1` position |
//! | discussions | home | number, zero padded | `k2` category, `n1` last activity |
//! | comments | discussion id | ULID | `k1` parent comment ("" = top level), `n1` created |
//! | poll votes | discussion id | principal | `k1` option |
//! | upvotes | discussion id | `<target>:<principal>` | `k1` target (`d` or comment ULID) |
//!
//! Counters (comments, upvotes, poll votes) are denormalized into the records and changed in
//! the same transaction as the row they count, under a row lock.
//!
//! # Rules
//!
//! - Announcement categories: only moderators start discussions.
//! - Poll categories: every discussion carries a poll of 2 to 8 options; one vote per
//!   principal, changeable; closed or locked discussions take no votes.
//! - Q&A categories: one comment (or reply) is the answer; the discussion author or a
//!   moderator marks it.
//! - Locked discussions take comments from moderators only (`LOCKED`).
//! - At most [`MAX_PINNED`] pinned discussions per home.
//! - Forms: a `.github/DISCUSSION_TEMPLATE/<slug>.yml` form on the default ref turns
//!   responses into the body ([`compose_form_body`]); required fields are enforced.

use bee_proto::v1::{
    Comment, CommentEdit, Discussion, DiscussionCategory, DiscussionCategoryFormat,
    DiscussionComment, DiscussionLockReason, DiscussionPoll, DiscussionPollOption, DiscussionSort,
    DiscussionStateReason, KeyValue, Label, Principal, Repo, SubjectKind, SubjectRef,
};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Row, Scan, TableDef, Tx};

use super::common::{
    MAX_BODY, MAX_TITLE, draft, emit, max_len, page_vec, paged, required, slugify, subject,
};
use super::ports::IssueBridge;
use super::reactions::{self, comment_key, discussion_key};
use super::{SURFACE, yaml};

/// Most pinned discussions in one home.
pub const MAX_PINNED: usize = 4;
/// Most categories in one home.
pub const MAX_CATEGORIES: usize = 25;
/// Most labels on one discussion.
pub const MAX_LABELS: usize = 20;

/// Categories, scoped by home.
pub static CATEGORIES: TableDef = TableDef {
    name: "forge_dsc_categories",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Discussions, scoped by home.
pub static DISCUSSIONS: TableDef = TableDef {
    name: "forge_dsc_discussions",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK2, Index::ScopeN1],
};
/// Comments, scoped by discussion id.
pub static COMMENTS: TableDef = TableDef {
    name: "forge_dsc_comments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Poll votes, scoped by discussion id.
pub static VOTES: TableDef = TableDef {
    name: "forge_dsc_votes",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Upvotes, scoped by discussion id.
pub static UPVOTES: TableDef = TableDef {
    name: "forge_dsc_upvotes",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Per-home number counters.
pub static COUNTERS: TableDef = TableDef {
    name: "forge_dsc_counters",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Discussion event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::gh(
        "discussion.created",
        "A discussion was started",
        "discussion",
        "created",
    ),
    EventKind::gh(
        "discussion.edited",
        "A discussion was edited",
        "discussion",
        "edited",
    ),
    EventKind::gh(
        "discussion.deleted",
        "A discussion was deleted",
        "discussion",
        "deleted",
    ),
    EventKind::gh(
        "discussion.closed",
        "A discussion was closed",
        "discussion",
        "closed",
    ),
    EventKind::gh(
        "discussion.reopened",
        "A discussion was reopened",
        "discussion",
        "reopened",
    ),
    EventKind::gh(
        "discussion.answered",
        "A discussion answer was marked",
        "discussion",
        "answered",
    ),
    EventKind::gh(
        "discussion.unanswered",
        "A discussion answer was unmarked",
        "discussion",
        "unanswered",
    ),
    EventKind::gh(
        "discussion.pinned",
        "A discussion was pinned",
        "discussion",
        "pinned",
    ),
    EventKind::gh(
        "discussion.unpinned",
        "A discussion was unpinned",
        "discussion",
        "unpinned",
    ),
    EventKind::gh(
        "discussion.locked",
        "A discussion was locked",
        "discussion",
        "locked",
    ),
    EventKind::gh(
        "discussion.unlocked",
        "A discussion was unlocked",
        "discussion",
        "unlocked",
    ),
    EventKind::gh(
        "discussion.labeled",
        "A label was added to a discussion",
        "discussion",
        "labeled",
    ),
    EventKind::gh(
        "discussion.unlabeled",
        "A label was removed from a discussion",
        "discussion",
        "unlabeled",
    ),
    EventKind::gh(
        "discussion.category_changed",
        "A discussion moved category",
        "discussion",
        "category_changed",
    ),
    EventKind::gh(
        "discussion_comment.created",
        "A discussion comment was added",
        "discussion_comment",
        "created",
    ),
    EventKind::gh(
        "discussion_comment.edited",
        "A discussion comment was edited",
        "discussion_comment",
        "edited",
    ),
    EventKind::gh(
        "discussion_comment.deleted",
        "A discussion comment was deleted",
        "discussion_comment",
        "deleted",
    ),
    EventKind::new("discussion.poll_voted", "A poll vote was cast or changed"),
    EventKind::new("discussion.upvoted", "A discussion or comment was upvoted"),
    EventKind::new("discussion.upvote_removed", "An upvote was removed"),
    EventKind::new(
        "discussion.category_created",
        "A discussion category was created",
    ),
    EventKind::new(
        "discussion.category_updated",
        "A discussion category was updated",
    ),
    EventKind::new(
        "discussion.category_deleted",
        "A discussion category was deleted",
    ),
    EventKind::new(
        "discussion.converted_from_issue",
        "An issue was converted to a discussion",
    ),
    EventKind::new(
        "discussion.converted_to_issue",
        "A discussion was converted to an issue",
    ),
];

/// Where discussions live: a repo or an org.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Home {
    /// Repo id, empty for org discussions.
    pub repo_id: String,
    /// Org id (the repo's org for repo homes).
    pub org_id: String,
    /// Repo full name or org name.
    pub full_name: String,
}

impl Home {
    /// The home of a repo's discussions.
    pub fn of_repo(r: &Repo) -> Home {
        Home {
            repo_id: r.id.clone(),
            org_id: r.org_id.clone(),
            full_name: r.full_name.clone(),
        }
    }

    /// The home of an org's discussions.
    pub fn of_org(org_id: &str, name: &str) -> Home {
        Home {
            repo_id: String::new(),
            org_id: org_id.to_string(),
            full_name: name.to_string(),
        }
    }

    /// The partition key: repo id, or org id for org homes.
    pub fn key(&self) -> &str {
        if self.repo_id.is_empty() {
            &self.org_id
        } else {
            &self.repo_id
        }
    }

    /// Whether this is an org home.
    pub fn is_org(&self) -> bool {
        self.repo_id.is_empty()
    }

    fn url(&self, base: &str, number: u64) -> String {
        if self.is_org() {
            format!("{base}/orgs/{}/discussions/{number}", self.full_name)
        } else {
            format!("{base}/{}/discussions/{number}", self.full_name)
        }
    }
}

/// A discussion's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct DiscRec {
    /// The discussion (counts denormalized; category holds only the id).
    #[prost(message, optional, tag = "1")]
    pub d: Option<Discussion>,
    /// Spotlight pattern.
    #[prost(string, tag = "2")]
    pub pin_pattern: String,
    /// Spotlight gradient.
    #[prost(string, tag = "3")]
    pub pin_gradient: String,
    /// Form responses the body was composed from.
    #[prost(message, repeated, tag = "4")]
    pub form: Vec<KeyValue>,
    /// When it was pinned (ms).
    #[prost(uint64, tag = "5")]
    pub pinned_at: u64,
}

/// A comment's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct CommentRec {
    /// The comment.
    #[prost(message, optional, tag = "1")]
    pub c: Option<Comment>,
    /// Upvotes.
    #[prost(uint32, tag = "2")]
    pub upvotes: u32,
    /// Replies (top-level comments).
    #[prost(uint32, tag = "3")]
    pub replies: u32,
    /// Earlier bodies.
    #[prost(message, repeated, tag = "4")]
    pub edits: Vec<CommentEdit>,
}

/// The public id of a discussion.
pub fn discussion_id(home_key: &str, number: u64) -> String {
    format!("{home_key}.{number}")
}

/// Splits a discussion id into home key and number.
pub fn parse_discussion_id(id: &str) -> Result<(String, u64)> {
    id.split_once('.')
        .and_then(|(h, n)| Some((h.to_string(), n.parse::<u64>().ok()?)))
        .filter(|(h, _)| !h.is_empty())
        .ok_or_else(|| ForgeError::not_found(format!("discussion {id}")))
}

/// Splits a comment id into discussion id and comment ULID.
pub fn parse_comment_id(id: &str) -> Result<(String, String)> {
    id.rsplit_once('.')
        .map(|(d, c)| (d.to_string(), c.to_string()))
        .filter(|(d, c)| !d.is_empty() && !c.is_empty() && d.contains('.'))
        .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))
}

fn num_id(number: u64) -> String {
    format!("{number:012}")
}

fn clean_labels(labels: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    for l in &out {
        max_len("label", l, 50)?;
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_LABELS {
        return Err(ForgeError::too_large(
            "labels",
            MAX_LABELS as u64,
            out.len() as u64,
        ));
    }
    Ok(out)
}

fn label_msgs(names: &[String]) -> Vec<Label> {
    names
        .iter()
        .map(|n| Label {
            id: n.clone(),
            name: n.clone(),
            ..Default::default()
        })
        .collect()
}

fn format_of(c: &DiscussionCategory) -> DiscussionCategoryFormat {
    DiscussionCategoryFormat::try_from(c.format).unwrap_or(DiscussionCategoryFormat::OpenEnded)
}

/// What a new discussion needs.
#[derive(Clone, Debug, Default)]
pub struct NewDiscussion {
    /// Category id or slug.
    pub category: String,
    /// Title.
    pub title: String,
    /// Markdown body (already composed from a form, if any).
    pub body: String,
    /// Label names.
    pub labels: Vec<String>,
    /// Poll question (poll categories).
    pub poll_question: String,
    /// Poll options (2 to 8).
    pub poll_options: Vec<String>,
    /// Form responses to keep with the discussion.
    pub form: Vec<KeyValue>,
    /// `OWNER`, `MEMBER`, `CONTRIBUTOR`, `NONE`.
    pub author_association: String,
    /// The author may moderate (announcement categories).
    pub moderator: bool,
}

/// Filters of [`Discussions::list`].
#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    /// Category id or slug.
    pub category: String,
    /// Required labels.
    pub labels: Vec<String>,
    /// Author id or display name.
    pub author: String,
    /// Include closed discussions.
    pub include_closed: bool,
    /// Only closed discussions.
    pub closed_only: bool,
    /// Q&A answered filter.
    pub answered: Option<bool>,
    /// Query with qualifiers (`is:`, `label:`, `category:`, `author:`) and free text.
    pub query: String,
    /// Sort.
    pub sort: i32,
    /// Ascending instead of descending.
    pub ascending: bool,
}

/// A partial discussion update.
#[derive(Clone, Debug, Default)]
pub struct UpdateDiscussion {
    /// New title.
    pub title: Option<String>,
    /// New body.
    pub body: Option<String>,
    /// Move to this category (id or slug).
    pub category: Option<String>,
    /// Expected row version, 0 = any.
    pub expected_version: u64,
    /// The caller may moderate.
    pub moderator: bool,
}

#[derive(Default)]
struct Query {
    open: Option<bool>,
    answered: Option<bool>,
    locked: Option<bool>,
    pinned: Option<bool>,
    labels: Vec<String>,
    category: String,
    author: String,
    text: Vec<String>,
}

fn parse_query(q: &str) -> Query {
    let mut out = Query::default();
    let mut rest = String::new();
    let mut chars = q.chars().peekable();
    // Tokenize on spaces, keeping quoted qualifier values together.
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in chars.by_ref() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    for t in tokens {
        match t.split_once(':') {
            Some(("is", "open")) => out.open = Some(true),
            Some(("is", "closed")) => out.open = Some(false),
            Some(("is", "answered")) => out.answered = Some(true),
            Some(("is", "unanswered")) => out.answered = Some(false),
            Some(("is", "locked")) => out.locked = Some(true),
            Some(("is", "unlocked")) => out.locked = Some(false),
            Some(("is", "pinned")) => out.pinned = Some(true),
            Some(("label", v)) => out.labels.push(v.to_string()),
            Some(("category", v)) => out.category = v.to_string(),
            Some(("author", v)) => out.author = v.to_string(),
            Some(("in", _)) | Some(("sort", _)) => {}
            _ => {
                rest.push_str(&t);
                out.text.push(t.to_lowercase());
            }
        }
    }
    out
}

/// A form field of a discussion form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormField {
    /// Field id (key of the response).
    pub id: String,
    /// Label shown above the answer.
    pub label: String,
    /// Whether an answer is required.
    pub required: bool,
}

/// Parses the input fields of a GitHub-style discussion form (`markdown` blocks are not
/// inputs).
pub fn parse_form(src: &str) -> Result<Vec<FormField>> {
    let y = yaml::parse(src).map_err(|e| ForgeError::invalid("invalid_form", e))?;
    let mut out = Vec::new();
    for item in y.get("body").map(|b| b.items()).unwrap_or_default() {
        if item.get("type").and_then(|t| t.as_str()).as_deref() == Some("markdown") {
            continue;
        }
        let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        out.push(FormField {
            label: item
                .get("attributes")
                .and_then(|a| a.get("label"))
                .and_then(|l| l.as_str())
                .unwrap_or_else(|| id.clone()),
            required: item
                .get("validations")
                .and_then(|v| v.get("required"))
                .and_then(|r| r.as_bool())
                .unwrap_or(false),
            id,
        });
    }
    Ok(out)
}

/// Builds the Markdown body of a form discussion: `### <label>` then the answer, per field in
/// form order. Missing required answers are `INVALID_ARGUMENT` / `form_field_required`.
pub fn compose_form_body(fields: &[FormField], responses: &[KeyValue]) -> Result<String> {
    let mut out = String::new();
    for f in fields {
        let answer = responses
            .iter()
            .find(|r| r.key == f.id)
            .map(|r| r.value.trim())
            .unwrap_or("");
        if answer.is_empty() {
            if f.required {
                return Err(ForgeError::invalid(
                    "form_field_required",
                    format!("form field `{}` is required", f.id),
                ));
            }
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("### {}\n\n{answer}", f.label));
    }
    Ok(out)
}

/// Discussions over the store.
pub struct Discussions {
    db: Db,
    base_url: String,
}

impl Discussions {
    /// Discussions over `db`; `base_url` prefixes permalinks.
    pub fn new(db: Db, base_url: &str) -> Discussions {
        Discussions {
            db,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    // ---- categories -------------------------------------------------------------------

    async fn category_by(
        tx: &mut Tx,
        home: &str,
        id_or_slug: &str,
    ) -> Result<(DiscussionCategory, Row)> {
        if let Some((c, row)) = tx
            .get_msg::<DiscussionCategory>(&CATEGORIES, home, id_or_slug)
            .await?
        {
            return Ok((c, row));
        }
        let rows = tx
            .scan(
                &CATEGORIES,
                &Scan::scope(home, 1).key(Key::K1(id_or_slug.to_lowercase())),
            )
            .await?;
        match rows.into_iter().next() {
            Some(row) => Ok((row.decode()?, row)),
            None => Err(ForgeError::not_found(format!("category {id_or_slug}"))),
        }
    }

    async fn hydrate_category(
        tx: &mut Tx,
        home: &str,
        c: DiscussionCategory,
        row: &Row,
    ) -> Result<DiscussionCategory> {
        let mut c = c;
        c.discussion_count = tx
            .count(&DISCUSSIONS, home, Some(&Key::K2(c.id.clone())))
            .await? as u32;
        c.position = row.n1.unwrap_or(0) as u32;
        c.version = row.version;
        Ok(c)
    }

    /// Creates a category.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_category(
        &self,
        cx: &Cx,
        home: &Home,
        name: &str,
        emoji: &str,
        description: &str,
        format: DiscussionCategoryFormat,
        section: &str,
    ) -> Result<DiscussionCategory> {
        required("name", name)?;
        max_len("name", name, 64)?;
        max_len("description", description, 500)?;
        let slug = slugify(name);
        if slug.is_empty() {
            return Err(ForgeError::invalid(
                "invalid_name",
                "the name has no letters or digits",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = tx
            .scan(&CATEGORIES, &Scan::scope(home.key(), MAX_CATEGORIES + 1))
            .await?;
        if existing.len() >= MAX_CATEGORIES {
            return Err(ForgeError::too_large(
                "discussion_categories",
                MAX_CATEGORIES as u64,
                existing.len() as u64 + 1,
            ));
        }
        if existing
            .iter()
            .any(|r| r.k1.as_deref() == Some(slug.as_str()))
        {
            return Err(ForgeError::already_exists(format!("category {slug}")));
        }
        let position = existing.iter().filter_map(|r| r.n1).max().unwrap_or(0) + 1;
        let format = if format == DiscussionCategoryFormat::Unspecified {
            DiscussionCategoryFormat::OpenEnded
        } else {
            format
        };
        let c = DiscussionCategory {
            id: tx.new_id()?,
            repo_id: home.repo_id.clone(),
            org_id: if home.is_org() {
                home.org_id.clone()
            } else {
                String::new()
            },
            name: name.trim().to_string(),
            slug: slug.clone(),
            emoji: emoji.to_string(),
            description: description.to_string(),
            format: format as i32,
            section: section.to_string(),
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        let row = Row::new(home.key(), c.id.clone())
            .k1(slug)
            .n1(position)
            .msg(&c);
        tx.insert(&CATEGORIES, &row).await?;
        let payload = serde_json::json!({"category": &c});
        emit(
            &mut tx,
            "discussion.category_created",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Repo, &home.repo_id, &c.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.category_create",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &c.id,
            &payload,
        ))
        .await?;
        let out = Self::hydrate_category(&mut tx, home.key(), c, &row.version(1)).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Updates a category.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_category(
        &self,
        cx: &Cx,
        home: &Home,
        id_or_slug: &str,
        name: Option<&str>,
        emoji: Option<&str>,
        description: Option<&str>,
        format: DiscussionCategoryFormat,
        section: Option<&str>,
        position: Option<u32>,
        expected_version: u64,
    ) -> Result<DiscussionCategory> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut c, row) = Self::category_by(&mut tx, home.key(), id_or_slug).await?;
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let mut row = row;
        if let Some(n) = name {
            required("name", n)?;
            max_len("name", n, 64)?;
            let slug = slugify(n);
            if slug.is_empty() {
                return Err(ForgeError::invalid(
                    "invalid_name",
                    "the name has no letters or digits",
                ));
            }
            if slug != c.slug {
                let taken = tx
                    .scan(
                        &CATEGORIES,
                        &Scan::scope(home.key(), 1).key(Key::K1(slug.clone())),
                    )
                    .await?;
                if !taken.is_empty() {
                    return Err(ForgeError::already_exists(format!("category {slug}")));
                }
            }
            c.name = n.trim().to_string();
            c.slug = slug.clone();
            row.k1 = Some(slug);
        }
        if let Some(e) = emoji {
            c.emoji = e.to_string();
        }
        if let Some(d) = description {
            max_len("description", d, 500)?;
            c.description = d.to_string();
        }
        if let Some(s) = section {
            c.section = s.to_string();
        }
        if format != DiscussionCategoryFormat::Unspecified {
            c.format = format as i32;
        }
        if let Some(p) = position {
            row.n1 = Some(i64::from(p));
        }
        row.body = c.encode_to_vec();
        let v = tx.update(&CATEGORIES, &row).await?;
        let payload = serde_json::json!({"category": &c});
        emit(
            &mut tx,
            "discussion.category_updated",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Repo, &home.repo_id, &c.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.category_update",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &c.id,
            &payload,
        ))
        .await?;
        row.version = v;
        let out = Self::hydrate_category(&mut tx, home.key(), c, &row).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a category; its discussions move to `move_to` (required when it has any).
    pub async fn delete_category(
        &self,
        cx: &Cx,
        home: &Home,
        id_or_slug: &str,
        move_to: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (c, row) = Self::category_by(&mut tx, home.key(), id_or_slug).await?;
        let held = tx
            .scan(
                &DISCUSSIONS,
                &Scan::scope(home.key(), crate::store::MAX_SCAN).key(Key::K2(c.id.clone())),
            )
            .await?;
        if !held.is_empty() {
            if move_to.is_empty() {
                return Err(ForgeError::invalid(
                    "move_to_required",
                    "the category holds discussions; name a category to move them to",
                ));
            }
            let (target, _) = Self::category_by(&mut tx, home.key(), move_to).await?;
            if target.id == c.id {
                return Err(ForgeError::invalid(
                    "move_to_same",
                    "cannot move discussions into the category being deleted",
                ));
            }
            for r in held {
                let mut rec: DiscRec = r.decode()?;
                if let Some(d) = rec.d.as_mut() {
                    d.category = Some(DiscussionCategory {
                        id: target.id.clone(),
                        ..Default::default()
                    });
                    if format_of(&target) != DiscussionCategoryFormat::QuestionAnswer {
                        d.answer_comment_id.clear();
                        d.answer_chosen_by = None;
                        d.answer_chosen_at = None;
                    }
                }
                let mut nr = r.clone().k2(target.id.clone());
                nr.body = rec.encode_to_vec();
                tx.update(&DISCUSSIONS, &nr).await?;
            }
        }
        tx.delete(&CATEGORIES, home.key(), &row.id).await?;
        let payload = serde_json::json!({"category": &c, "moved_to": move_to});
        emit(
            &mut tx,
            "discussion.category_deleted",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Repo, &home.repo_id, &c.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.category_delete",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &c.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Categories in position order.
    pub async fn list_categories(
        &self,
        cx: &Cx,
        home: &Home,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<DiscussionCategory>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = paged(
            &mut tx,
            &CATEGORIES,
            Scan::scope(home.key(), limit).by_n1(),
            token,
            limit,
        )
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let c: DiscussionCategory = r.decode()?;
            out.push(Self::hydrate_category(&mut tx, home.key(), c, &r).await?);
        }
        Ok((out, next))
    }

    /// One category by id or slug.
    pub async fn category(
        &self,
        cx: &Cx,
        home: &Home,
        id_or_slug: &str,
    ) -> Result<DiscussionCategory> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (c, row) = Self::category_by(&mut tx, home.key(), id_or_slug).await?;
        Self::hydrate_category(&mut tx, home.key(), c, &row).await
    }

    // ---- discussions ------------------------------------------------------------------

    async fn load(tx: &mut Tx, home: &str, number: u64) -> Result<(DiscRec, Row)> {
        match tx
            .get_msg::<DiscRec>(&DISCUSSIONS, home, &num_id(number))
            .await?
        {
            Some(x) => Ok(x),
            None => Err(ForgeError::not_found(format!("discussion #{number}"))),
        }
    }

    async fn load_locked(tx: &mut Tx, home: &str, number: u64) -> Result<(DiscRec, Row)> {
        match tx
            .get_for_update(&DISCUSSIONS, home, &num_id(number))
            .await?
        {
            Some(row) => Ok((row.decode()?, row)),
            None => Err(ForgeError::not_found(format!("discussion #{number}"))),
        }
    }

    async fn view(
        &self,
        tx: &mut Tx,
        home: &Home,
        rec: &DiscRec,
        row: &Row,
        viewer: &str,
    ) -> Result<Discussion> {
        let mut d = rec.d.clone().unwrap_or_default();
        let cat_id = d
            .category
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        if let Ok((c, crow)) = Self::category_by(tx, home.key(), &cat_id).await {
            d.category = Some(Self::hydrate_category(tx, home.key(), c, &crow).await?);
        }
        d.repo_full_name = if home.is_org() {
            String::new()
        } else {
            home.full_name.clone()
        };
        let did = d.id.clone();
        d.viewer_upvoted = tx
            .get(&UPVOTES, &did, &format!("d:{viewer}"))
            .await?
            .is_some();
        d.reactions = reactions::groups(tx, &discussion_key(&did), viewer).await?;
        if let Some(p) = d.poll.as_mut() {
            let mine = tx.get(&VOTES, &did, viewer).await?.and_then(|r| r.k1);
            p.viewer_has_voted = mine.is_some();
            for o in &mut p.options {
                o.viewer_voted = mine.as_deref() == Some(o.id.as_str());
            }
            p.closed = d.closed || d.locked;
        }
        d.url = home.url(&self.base_url, d.number);
        d.version = row.version;
        Ok(d)
    }

    /// Starts a discussion.
    pub async fn create(&self, cx: &Cx, home: &Home, n: NewDiscussion) -> Result<Discussion> {
        self.create_as(cx, home, n, cx.principal(), None).await
    }

    async fn create_as(
        &self,
        cx: &Cx,
        home: &Home,
        n: NewDiscussion,
        author: Principal,
        converted_from: Option<SubjectRef>,
    ) -> Result<Discussion> {
        required("title", &n.title)?;
        max_len("title", &n.title, MAX_TITLE)?;
        required("body", &n.body)?;
        max_len("body", &n.body, MAX_BODY)?;
        let labels = clean_labels(&n.labels)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (cat, _) = Self::category_by(&mut tx, home.key(), &n.category).await?;
        let format = format_of(&cat);
        if format == DiscussionCategoryFormat::Announcement && !n.moderator {
            return Err(ForgeError::permission_denied(
                "only maintainers start discussions in an announcement category",
            ));
        }
        let poll = if format == DiscussionCategoryFormat::Poll && converted_from.is_none() {
            required("poll_question", &n.poll_question)?;
            max_len("poll_question", &n.poll_question, 200)?;
            let mut opts: Vec<String> = n
                .poll_options
                .iter()
                .map(|o| o.trim().to_string())
                .collect();
            if opts.len() < 2
                || opts.len() > 8
                || opts.iter().any(|o| o.is_empty() || o.len() > 100)
            {
                return Err(ForgeError::invalid(
                    "invalid_poll",
                    "a poll has 2 to 8 non-empty options",
                ));
            }
            let mut seen = opts.clone();
            seen.sort();
            seen.dedup();
            if seen.len() != opts.len() {
                return Err(ForgeError::invalid(
                    "invalid_poll",
                    "poll options must be distinct",
                ));
            }
            Some(DiscussionPoll {
                question: n.poll_question.clone(),
                options: opts
                    .drain(..)
                    .enumerate()
                    .map(|(i, text)| DiscussionPollOption {
                        id: (i + 1).to_string(),
                        text,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
        } else if format == DiscussionCategoryFormat::Poll {
            return Err(ForgeError::invalid(
                "poll_category",
                "issues cannot be converted into a poll category",
            ));
        } else {
            if !n.poll_question.is_empty() || !n.poll_options.is_empty() {
                return Err(ForgeError::invalid(
                    "not_a_poll_category",
                    "only poll categories take a poll",
                ));
            }
            None
        };
        let number = tx.add_n1(&COUNTERS, home.key(), "number", 1).await? as u64;
        let now = tx.now_ms();
        let id = discussion_id(home.key(), number);
        let d = Discussion {
            id: id.clone(),
            repo_id: home.repo_id.clone(),
            repo_full_name: home.full_name.clone(),
            org_id: if home.is_org() {
                home.org_id.clone()
            } else {
                String::new()
            },
            number,
            title: n.title.trim().to_string(),
            body: n.body.clone(),
            category: Some(DiscussionCategory {
                id: cat.id.clone(),
                ..Default::default()
            }),
            author: Some(author),
            author_association: n.author_association.clone(),
            labels: label_msgs(&labels),
            poll,
            converted_from,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        let rec = DiscRec {
            d: Some(d),
            form: n.form,
            ..Default::default()
        };
        let row = Row::new(home.key(), num_id(number))
            .k2(cat.id.clone())
            .n1(now as i64)
            .msg(&rec);
        tx.insert(&DISCUSSIONS, &row).await?;
        let out = self
            .view(&mut tx, home, &rec, &row.clone().version(1), &cx.actor_id)
            .await?;
        let payload = serde_json::json!({"action": "created", "discussion": &out});
        emit(
            &mut tx,
            "discussion.created",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.create",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &id,
            &serde_json::json!({"number": number, "category": cat.slug}),
        ))
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// A discussion by number.
    pub async fn get(&self, cx: &Cx, home: &Home, number: u64) -> Result<Discussion> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load(&mut tx, home.key(), number).await?;
        self.view(&mut tx, home, &rec, &row, &cx.actor_id).await
    }

    /// Discussions matching `f`, and the total before paging.
    pub async fn list(
        &self,
        cx: &Cx,
        home: &Home,
        f: &ListFilter,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Discussion>, u64, String)> {
        let q = parse_query(&f.query);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let cat_id = {
            let want = if f.category.is_empty() {
                q.category.clone()
            } else {
                f.category.clone()
            };
            if want.is_empty() {
                String::new()
            } else {
                match Self::category_by(&mut tx, home.key(), &want).await {
                    Ok((c, _)) => c.id,
                    Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {
                        return Ok((Vec::new(), 0, String::new()));
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        let open_filter = match (f.closed_only, f.include_closed, q.open) {
            (true, _, _) => Some(false),
            (_, _, Some(o)) => Some(o),
            (_, true, None) => None,
            _ => Some(true),
        };
        let mut scan = Scan::scope(home.key(), crate::store::MAX_SCAN);
        if !cat_id.is_empty() {
            scan = scan.key(Key::K2(cat_id));
        }
        let rows = tx.scan(&DISCUSSIONS, &scan).await?;
        let mut want_labels = f.labels.clone();
        want_labels.extend(q.labels.iter().cloned());
        let author = if f.author.is_empty() {
            q.author.clone()
        } else {
            f.author.clone()
        };
        let answered = f.answered.or(q.answered);
        let mut hits: Vec<(DiscRec, Row)> = Vec::new();
        for row in rows {
            let rec: DiscRec = row.decode()?;
            let d = rec.d.clone().unwrap_or_default();
            if open_filter.is_some_and(|o| o == d.closed) {
                continue;
            }
            if answered.is_some_and(|a| a == d.answer_comment_id.is_empty()) {
                continue;
            }
            if q.locked.is_some_and(|l| l != d.locked) || q.pinned.is_some_and(|p| p != d.pinned) {
                continue;
            }
            if !want_labels
                .iter()
                .all(|w| d.labels.iter().any(|l| l.name.eq_ignore_ascii_case(w)))
            {
                continue;
            }
            if !author.is_empty() {
                let a = d.author.clone().unwrap_or_default();
                if !(a.id == author || a.display.eq_ignore_ascii_case(&author)) {
                    continue;
                }
            }
            if !q.text.is_empty() {
                let hay = format!("{} {}", d.title, d.body).to_lowercase();
                if !q.text.iter().all(|t| hay.contains(t.as_str())) {
                    continue;
                }
            }
            hits.push((rec, row));
        }
        let sort = DiscussionSort::try_from(f.sort).unwrap_or(DiscussionSort::LatestActivity);
        hits.sort_by(|(a, ar), (b, br)| {
            let (a, b) = (
                a.d.clone().unwrap_or_default(),
                b.d.clone().unwrap_or_default(),
            );
            let ord = match sort {
                DiscussionSort::Created => a.number.cmp(&b.number),
                DiscussionSort::Top => a
                    .upvote_count
                    .cmp(&b.upvote_count)
                    .then(a.number.cmp(&b.number)),
                _ => ar.n1.cmp(&br.n1).then(a.number.cmp(&b.number)),
            };
            if f.ascending { ord } else { ord.reverse() }
        });
        let total = hits.len() as u64;
        let (page, next) = page_vec(hits, token, limit)?;
        let mut out = Vec::new();
        for (rec, row) in page {
            out.push(self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?);
        }
        Ok((out, total, next))
    }

    fn author_or_moderator(rec: &DiscRec, cx: &Cx, moderator: bool) -> Result<()> {
        let author = rec
            .d
            .as_ref()
            .and_then(|d| d.author.as_ref())
            .map(|a| a.id.as_str())
            .unwrap_or("");
        if moderator || author == cx.actor_id {
            Ok(())
        } else {
            Err(ForgeError::permission_denied(
                "only the author or a moderator may change this discussion",
            ))
        }
    }

    /// Edits a discussion (author or moderator).
    pub async fn update(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        u: UpdateDiscussion,
    ) -> Result<Discussion> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        if u.expected_version != 0 && u.expected_version != row.version {
            return Err(ForgeError::version_mismatch(
                u.expected_version,
                row.version,
            ));
        }
        Self::author_or_moderator(&rec, cx, u.moderator)?;
        let mut d = rec.d.clone().unwrap_or_default();
        let mut moved = false;
        if let Some(t) = &u.title {
            required("title", t)?;
            max_len("title", t, MAX_TITLE)?;
            d.title = t.trim().to_string();
        }
        if let Some(b) = &u.body {
            required("body", b)?;
            max_len("body", b, MAX_BODY)?;
            d.body = b.clone();
        }
        if let Some(c) = &u.category {
            let (cat, _) = Self::category_by(&mut tx, home.key(), c).await?;
            let cur = d
                .category
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_default();
            if cat.id != cur {
                match format_of(&cat) {
                    DiscussionCategoryFormat::Poll if d.poll.is_none() => {
                        return Err(ForgeError::invalid(
                            "poll_category",
                            "a discussion without a poll cannot move into a poll category",
                        ));
                    }
                    DiscussionCategoryFormat::Announcement if !u.moderator => {
                        return Err(ForgeError::permission_denied(
                            "only maintainers move discussions into an announcement category",
                        ));
                    }
                    DiscussionCategoryFormat::QuestionAnswer => {}
                    _ => {
                        d.answer_comment_id.clear();
                        d.answer_chosen_by = None;
                        d.answer_chosen_at = None;
                    }
                }
                d.category = Some(DiscussionCategory {
                    id: cat.id.clone(),
                    ..Default::default()
                });
                row.k2 = Some(cat.id);
                moved = true;
            }
        }
        let now = tx.now_ms();
        d.updated_at = Some(ts(now));
        rec.d = Some(d);
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let kind = if moved && u.title.is_none() && u.body.is_none() {
            "discussion.category_changed"
        } else {
            "discussion.edited"
        };
        self.record(&mut tx, home, kind, "discussion.update", &out, &out.id)
            .await?;
        tx.commit().await?;
        Ok(out)
    }

    async fn record(
        &self,
        tx: &mut Tx,
        home: &Home,
        kind: &str,
        audit: &str,
        d: &Discussion,
        id: &str,
    ) -> Result<()> {
        let action = kind.split_once('.').map(|(_, a)| a).unwrap_or(kind);
        let payload = serde_json::json!({"action": action, "discussion": d});
        emit(
            tx,
            kind,
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, id),
            payload,
        )
        .await?;
        tx.audit(draft(audit, &home.org_id, &home.repo_id, SubjectKind::Discussion, id,
            &serde_json::json!({"number": d.number, "closed": d.closed, "locked": d.locked, "pinned": d.pinned}))).await
    }

    async fn purge(tx: &mut Tx, did: &str) -> Result<()> {
        for t in [&UPVOTES, &VOTES] {
            for r in tx
                .scan(t, &Scan::scope(did, crate::store::MAX_SCAN))
                .await?
            {
                tx.delete(t, did, &r.id).await?;
            }
        }
        for r in tx
            .scan(&COMMENTS, &Scan::scope(did, crate::store::MAX_SCAN))
            .await?
        {
            let ckey = comment_key(&format!("{did}.{}", r.id));
            for x in tx
                .scan(
                    &reactions::REACTIONS,
                    &Scan::scope(ckey.clone(), crate::store::MAX_SCAN),
                )
                .await?
            {
                tx.delete(&reactions::REACTIONS, &ckey, &x.id).await?;
            }
            tx.delete(&COMMENTS, did, &r.id).await?;
        }
        let dkey = discussion_key(did);
        for x in tx
            .scan(
                &reactions::REACTIONS,
                &Scan::scope(dkey.clone(), crate::store::MAX_SCAN),
            )
            .await?
        {
            tx.delete(&reactions::REACTIONS, &dkey, &x.id).await?;
        }
        Ok(())
    }

    /// Deletes a discussion with its comments, votes and reactions (author or moderator).
    pub async fn delete(&self, cx: &Cx, home: &Home, number: u64, moderator: bool) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load_locked(&mut tx, home.key(), number).await?;
        Self::author_or_moderator(&rec, cx, moderator)?;
        let d = rec.d.clone().unwrap_or_default();
        Self::purge(&mut tx, &d.id).await?;
        tx.delete(&DISCUSSIONS, home.key(), &row.id).await?;
        self.record(
            &mut tx,
            home,
            "discussion.deleted",
            "discussion.delete",
            &d,
            &d.id,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Closes a discussion (author or moderator).
    pub async fn close(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        reason: DiscussionStateReason,
        moderator: bool,
    ) -> Result<Discussion> {
        self.set_state(cx, home, number, true, reason, moderator)
            .await
    }

    /// Reopens a discussion (author or moderator).
    pub async fn reopen(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        moderator: bool,
    ) -> Result<Discussion> {
        self.set_state(
            cx,
            home,
            number,
            false,
            DiscussionStateReason::Reopened,
            moderator,
        )
        .await
    }

    async fn set_state(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        closed: bool,
        reason: DiscussionStateReason,
        moderator: bool,
    ) -> Result<Discussion> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        Self::author_or_moderator(&rec, cx, moderator)?;
        let mut d = rec.d.clone().unwrap_or_default();
        if d.closed == closed {
            return Err(ForgeError::invalid_state(if closed {
                "the discussion is already closed"
            } else {
                "the discussion is not closed"
            }));
        }
        let now = tx.now_ms();
        d.closed = closed;
        d.state_reason = if reason == DiscussionStateReason::Unspecified && closed {
            DiscussionStateReason::Resolved as i32
        } else {
            reason as i32
        };
        d.closed_at = closed.then(|| ts(now));
        d.updated_at = Some(ts(now));
        rec.d = Some(d);
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let (kind, audit) = if closed {
            ("discussion.closed", "discussion.close")
        } else {
            ("discussion.reopened", "discussion.reopen")
        };
        self.record(&mut tx, home, kind, audit, &out, &out.id)
            .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Locks (with a reason) or unlocks a discussion; moderators only (the service checks).
    pub async fn set_locked(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        locked: bool,
        reason: DiscussionLockReason,
    ) -> Result<Discussion> {
        self.mutate(
            cx,
            home,
            number,
            if locked {
                "discussion.locked"
            } else {
                "discussion.unlocked"
            },
            if locked {
                "discussion.lock"
            } else {
                "discussion.unlock"
            },
            |d, _| {
                if d.locked == locked {
                    return Err(ForgeError::invalid_state(if locked {
                        "the discussion is already locked"
                    } else {
                        "the discussion is not locked"
                    }));
                }
                d.locked = locked;
                d.lock_reason = if locked { reason as i32 } else { 0 };
                Ok(())
            },
        )
        .await
    }

    /// Pins or unpins a discussion; at most [`MAX_PINNED`] per home.
    pub async fn set_pinned(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        pinned: bool,
        pattern: &str,
        gradient: &str,
    ) -> Result<Discussion> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut d = rec.d.clone().unwrap_or_default();
        if d.pinned == pinned {
            return Err(ForgeError::invalid_state(if pinned {
                "the discussion is already pinned"
            } else {
                "the discussion is not pinned"
            }));
        }
        if pinned {
            let rows = tx
                .scan(
                    &DISCUSSIONS,
                    &Scan::scope(home.key(), crate::store::MAX_SCAN),
                )
                .await?;
            let n = rows
                .iter()
                .filter(|r| {
                    r.decode::<DiscRec>()
                        .map(|x| x.d.is_some_and(|d| d.pinned))
                        .unwrap_or(false)
                })
                .count();
            if n >= MAX_PINNED {
                return Err(ForgeError::too_large(
                    "pinned_discussions",
                    MAX_PINNED as u64,
                    n as u64 + 1,
                ));
            }
        }
        d.pinned = pinned;
        rec.pin_pattern = if pinned {
            pattern.to_string()
        } else {
            String::new()
        };
        rec.pin_gradient = if pinned {
            gradient.to_string()
        } else {
            String::new()
        };
        rec.pinned_at = if pinned { tx.now_ms() } else { 0 };
        rec.d = Some(d);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let (kind, audit) = if pinned {
            ("discussion.pinned", "discussion.pin")
        } else {
            ("discussion.unpinned", "discussion.unpin")
        };
        self.record(&mut tx, home, kind, audit, &out, &out.id)
            .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Pinned discussions, most recently pinned first.
    pub async fn list_pinned(&self, cx: &Cx, home: &Home) -> Result<Vec<Discussion>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &DISCUSSIONS,
                &Scan::scope(home.key(), crate::store::MAX_SCAN),
            )
            .await?;
        let mut pinned: Vec<(DiscRec, Row)> = Vec::new();
        for r in rows {
            let rec: DiscRec = r.decode()?;
            if rec.d.as_ref().is_some_and(|d| d.pinned) {
                pinned.push((rec, r));
            }
        }
        pinned.sort_by_key(|(r, _)| {
            std::cmp::Reverse((r.pinned_at, r.d.as_ref().map(|d| d.number).unwrap_or(0)))
        });
        let mut out = Vec::new();
        for (rec, row) in pinned {
            out.push(self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?);
        }
        Ok(out)
    }

    async fn mutate(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        kind: &str,
        audit: &str,
        f: impl FnOnce(&mut Discussion, &mut DiscRec) -> Result<()>,
    ) -> Result<Discussion> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut d = rec.d.clone().unwrap_or_default();
        f(&mut d, &mut rec)?;
        let now = tx.now_ms();
        d.updated_at = Some(ts(now));
        rec.d = Some(d);
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        self.record(&mut tx, home, kind, audit, &out, &out.id)
            .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Adds labels (names are created on first use).
    pub async fn add_labels(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        labels: &[String],
    ) -> Result<Discussion> {
        let add = clean_labels(labels)?;
        self.mutate(
            cx,
            home,
            number,
            "discussion.labeled",
            "discussion.label",
            |d, _| {
                let mut names: Vec<String> = d.labels.iter().map(|l| l.name.clone()).collect();
                names.extend(add);
                d.labels = label_msgs(&clean_labels(&names)?);
                Ok(())
            },
        )
        .await
    }

    /// Removes labels.
    pub async fn remove_labels(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        labels: &[String],
    ) -> Result<Discussion> {
        self.mutate(
            cx,
            home,
            number,
            "discussion.unlabeled",
            "discussion.unlabel",
            |d, _| {
                d.labels
                    .retain(|l| !labels.iter().any(|r| r.eq_ignore_ascii_case(&l.name)));
                Ok(())
            },
        )
        .await
    }

    /// Replaces all labels.
    pub async fn set_labels(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        labels: &[String],
    ) -> Result<Discussion> {
        let set = clean_labels(labels)?;
        self.mutate(
            cx,
            home,
            number,
            "discussion.labeled",
            "discussion.label",
            |d, _| {
                d.labels = label_msgs(&set);
                Ok(())
            },
        )
        .await
    }

    // ---- comments ---------------------------------------------------------------------

    async fn comment_view(
        tx: &mut Tx,
        did: &str,
        rec: &CommentRec,
        answer: &str,
        viewer: &str,
        base: &str,
        url: &str,
    ) -> Result<DiscussionComment> {
        let mut c = rec.c.clone().unwrap_or_default();
        let cid = c.id.clone();
        c.reactions = reactions::groups(tx, &comment_key(&cid), viewer).await?;
        c.url = format!(
            "{base}{url}#discussioncomment-{}",
            cid.rsplit('.').next().unwrap_or("")
        );
        let _ = did;
        Ok(DiscussionComment {
            is_answer: !answer.is_empty() && answer == cid,
            upvote_count: rec.upvotes,
            viewer_upvoted: tx
                .get(
                    &UPVOTES,
                    did,
                    &format!("{}:{viewer}", cid.rsplit('.').next().unwrap_or("")),
                )
                .await?
                .is_some(),
            reply_count: rec.replies,
            comment: Some(c),
        })
    }

    /// Adds a comment or reply. Replies attach to the top-level comment of the thread.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_comment(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        body: &str,
        reply_to: &str,
        moderator: bool,
        association: &str,
    ) -> Result<DiscussionComment> {
        required("body", body)?;
        max_len("body", body, MAX_BODY)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut d = rec.d.clone().unwrap_or_default();
        if d.locked && !moderator {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "the discussion is locked",
            ));
        }
        let did = d.id.clone();
        let mut parent = String::new();
        if !reply_to.is_empty() {
            let (pd, pc) = parse_comment_id(reply_to)?;
            if pd != did {
                return Err(ForgeError::not_found(format!("comment {reply_to}")));
            }
            let (prec, mut prow) = match tx.get_for_update(&COMMENTS, &did, &pc).await? {
                Some(r) => (r.decode::<CommentRec>()?, r),
                None => return Err(ForgeError::not_found(format!("comment {reply_to}"))),
            };
            let root_ulid = match prow.k1.as_deref() {
                Some("") | None => pc.clone(),
                Some(root) => root.to_string(),
            };
            if root_ulid != pc {
                // A reply to a reply joins its root.
                match tx.get_for_update(&COMMENTS, &did, &root_ulid).await? {
                    Some(r) => {
                        prow = r;
                    }
                    None => return Err(ForgeError::not_found("parent comment")),
                }
            }
            let mut root: CommentRec = if root_ulid == pc {
                prec
            } else {
                prow.decode()?
            };
            root.replies += 1;
            prow.body = root.encode_to_vec();
            tx.update(&COMMENTS, &prow).await?;
            parent = root_ulid;
        }
        let now = tx.now_ms();
        let ulid = tx.new_id()?;
        let cid = format!("{did}.{ulid}");
        let c = Comment {
            id: cid.clone(),
            subject: Some(subject(SubjectKind::Discussion, &home.repo_id, &did)),
            author: Some(cx.principal()),
            body: body.to_string(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            in_reply_to: if parent.is_empty() {
                String::new()
            } else {
                format!("{did}.{parent}")
            },
            author_association: association.to_string(),
            ..Default::default()
        };
        let crec = CommentRec {
            c: Some(c),
            ..Default::default()
        };
        tx.insert(
            &COMMENTS,
            &Row::new(did.clone(), ulid.clone())
                .k1(parent)
                .n1(now as i64)
                .msg(&crec),
        )
        .await?;
        d.comment_count += 1;
        d.updated_at = Some(ts(now));
        rec.d = Some(d.clone());
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let url = home.url("", d.number);
        let out = Self::comment_view(
            &mut tx,
            &did,
            &crec,
            &d.answer_comment_id,
            &cx.actor_id,
            &self.base_url,
            &url,
        )
        .await?;
        let payload = serde_json::json!({"action": "created", "discussion": {"id": did, "number": d.number, "title": d.title,
            "author": d.author, "repo_full_name": home.full_name}, "comment": out.comment});
        emit(
            &mut tx,
            "discussion_comment.created",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload,
        )
        .await?;
        tx.audit(draft(
            "discussion.comment_create",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Comment,
            &cid,
            &serde_json::json!({"discussion": did}),
        ))
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Top-level comments of a discussion, oldest first.
    pub async fn list_comments(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<DiscussionComment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, home.key(), number).await?;
        let d = rec.d.unwrap_or_default();
        let (rows, next) = paged(
            &mut tx,
            &COMMENTS,
            Scan::scope(d.id.clone(), limit)
                .by_n1()
                .key(Key::K1(String::new())),
            token,
            limit,
        )
        .await?;
        let url = home.url("", d.number);
        let mut out = Vec::new();
        for r in rows {
            out.push(
                Self::comment_view(
                    &mut tx,
                    &d.id,
                    &r.decode()?,
                    &d.answer_comment_id,
                    &cx.actor_id,
                    &self.base_url,
                    &url,
                )
                .await?,
            );
        }
        Ok((out, next))
    }

    /// Replies of a top-level comment, oldest first.
    pub async fn list_replies(
        &self,
        cx: &Cx,
        comment_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<DiscussionComment>, String)> {
        let (did, ulid) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &home_key, number).await?;
        let d = rec.d.unwrap_or_default();
        if tx.get(&COMMENTS, &did, &ulid).await?.is_none() {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        }
        let (rows, next) = paged(
            &mut tx,
            &COMMENTS,
            Scan::scope(did.clone(), limit).by_n1().key(Key::K1(ulid)),
            token,
            limit,
        )
        .await?;
        let mut out = Vec::new();
        for r in rows {
            out.push(
                Self::comment_view(
                    &mut tx,
                    &did,
                    &r.decode()?,
                    &d.answer_comment_id,
                    &cx.actor_id,
                    &self.base_url,
                    "",
                )
                .await?,
            );
        }
        Ok((out, next))
    }

    /// One comment by id.
    pub async fn get_comment(&self, cx: &Cx, comment_id: &str) -> Result<DiscussionComment> {
        let (did, ulid) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &home_key, number).await?;
        let d = rec.d.unwrap_or_default();
        match tx.get_msg::<CommentRec>(&COMMENTS, &did, &ulid).await? {
            Some((c, _)) => {
                Self::comment_view(
                    &mut tx,
                    &did,
                    &c,
                    &d.answer_comment_id,
                    &cx.actor_id,
                    &self.base_url,
                    "",
                )
                .await
            }
            None => Err(ForgeError::not_found(format!("comment {comment_id}"))),
        }
    }

    /// Edits a comment, keeping the previous body in its edit history (author or moderator).
    pub async fn edit_comment(
        &self,
        cx: &Cx,
        home: &Home,
        comment_id: &str,
        body: &str,
        expected_version: u64,
        moderator: bool,
    ) -> Result<DiscussionComment> {
        required("body", body)?;
        max_len("body", body, MAX_BODY)?;
        let (did, ulid) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        if home_key != home.key() {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (drec, _) = Self::load(&mut tx, home.key(), number).await?;
        let d = drec.d.unwrap_or_default();
        let Some(mut row) = tx.get_for_update(&COMMENTS, &did, &ulid).await? else {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        };
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let mut rec: CommentRec = row.decode()?;
        let mut c = rec.c.clone().unwrap_or_default();
        if c.deleted {
            return Err(ForgeError::invalid_state("the comment was deleted"));
        }
        if !moderator && c.author.as_ref().map(|a| a.id.as_str()) != Some(cx.actor_id.as_str()) {
            return Err(ForgeError::permission_denied(
                "only the author or a moderator may edit this comment",
            ));
        }
        let now = tx.now_ms();
        rec.edits.push(CommentEdit {
            comment_id: comment_id.to_string(),
            editor: Some(cx.principal()),
            previous_body: c.body.clone(),
            edited_at: Some(ts(now)),
        });
        c.body = body.to_string();
        c.edited = true;
        c.updated_at = Some(ts(now));
        rec.c = Some(c);
        row.body = rec.encode_to_vec();
        tx.update(&COMMENTS, &row).await?;
        let out = Self::comment_view(
            &mut tx,
            &did,
            &rec,
            &d.answer_comment_id,
            &cx.actor_id,
            &self.base_url,
            "",
        )
        .await?;
        let payload = serde_json::json!({"action": "edited", "discussion": {"id": did, "number": d.number, "title": d.title}, "comment": out.comment});
        emit(
            &mut tx,
            "discussion_comment.edited",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload,
        )
        .await?;
        tx.audit(draft(
            "discussion.comment_edit",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Comment,
            comment_id,
            &serde_json::json!({"discussion": did}),
        ))
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a comment: the tombstone keeps its place in the thread (author or moderator).
    pub async fn delete_comment(
        &self,
        cx: &Cx,
        home: &Home,
        comment_id: &str,
        moderator: bool,
    ) -> Result<()> {
        let (did, ulid) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        if home_key != home.key() {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut drec, mut drow) = Self::load_locked(&mut tx, home.key(), number).await?;
        let Some(mut row) = tx.get_for_update(&COMMENTS, &did, &ulid).await? else {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        };
        let mut rec: CommentRec = row.decode()?;
        let mut c = rec.c.clone().unwrap_or_default();
        if c.deleted {
            return Ok(());
        }
        if !moderator && c.author.as_ref().map(|a| a.id.as_str()) != Some(cx.actor_id.as_str()) {
            return Err(ForgeError::permission_denied(
                "only the author or a moderator may delete this comment",
            ));
        }
        c.deleted = true;
        c.body.clear();
        rec.c = Some(c);
        row.body = rec.encode_to_vec();
        tx.update(&COMMENTS, &row).await?;
        let mut d = drec.d.clone().unwrap_or_default();
        d.comment_count = d.comment_count.saturating_sub(1);
        if d.answer_comment_id == comment_id {
            d.answer_comment_id.clear();
            d.answer_chosen_by = None;
            d.answer_chosen_at = None;
        }
        let title = d.title.clone();
        drec.d = Some(d);
        drow.body = drec.encode_to_vec();
        tx.update(&DISCUSSIONS, &drow).await?;
        let payload = serde_json::json!({"action": "deleted", "discussion": {"id": did, "number": number, "title": title}, "comment_id": comment_id});
        emit(
            &mut tx,
            "discussion_comment.deleted",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload,
        )
        .await?;
        tx.audit(draft(
            "discussion.comment_delete",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Comment,
            comment_id,
            &serde_json::json!({"discussion": did}),
        ))
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The edit history of a comment, oldest first.
    pub async fn comment_edits(&self, cx: &Cx, comment_id: &str) -> Result<Vec<CommentEdit>> {
        let (did, ulid) = parse_comment_id(comment_id)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match tx.get_msg::<CommentRec>(&COMMENTS, &did, &ulid).await? {
            Some((rec, _)) => Ok(rec.edits),
            None => Err(ForgeError::not_found(format!("comment {comment_id}"))),
        }
    }

    /// The home key and number of the discussion a comment belongs to.
    pub fn comment_home(comment_id: &str) -> Result<(String, u64)> {
        let (did, _) = parse_comment_id(comment_id)?;
        parse_discussion_id(&did)
    }

    // ---- answers ----------------------------------------------------------------------

    /// Marks a comment as the answer of a Q&A discussion (discussion author or moderator).
    pub async fn mark_answer(
        &self,
        cx: &Cx,
        home: &Home,
        comment_id: &str,
        moderator: bool,
    ) -> Result<Discussion> {
        let (did, ulid) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        if home_key != home.key() {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        Self::author_or_moderator(&rec, cx, moderator)?;
        let mut d = rec.d.clone().unwrap_or_default();
        let cat_id = d
            .category
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        let (cat, _) = Self::category_by(&mut tx, home.key(), &cat_id).await?;
        if format_of(&cat) != DiscussionCategoryFormat::QuestionAnswer {
            return Err(
                ForgeError::invalid_state("answers can only be marked in a Q&A category")
                    .with_subcode("not_question_answer"),
            );
        }
        let crec = match tx.get_msg::<CommentRec>(&COMMENTS, &did, &ulid).await? {
            Some((c, _)) => c,
            None => return Err(ForgeError::not_found(format!("comment {comment_id}"))),
        };
        if crec.c.as_ref().is_some_and(|c| c.deleted) {
            return Err(ForgeError::invalid_state(
                "a deleted comment cannot be the answer",
            ));
        }
        let now = tx.now_ms();
        d.answer_comment_id = comment_id.to_string();
        d.answer_chosen_by = Some(cx.principal());
        d.answer_chosen_at = Some(ts(now));
        d.updated_at = Some(ts(now));
        rec.d = Some(d);
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let payload =
            serde_json::json!({"action": "answered", "discussion": &out, "answer": crec.c});
        emit(
            &mut tx,
            "discussion.answered",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload,
        )
        .await?;
        tx.audit(draft(
            "discussion.answer",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &did,
            &serde_json::json!({"comment": comment_id}),
        ))
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Unmarks the answer.
    pub async fn unmark_answer(
        &self,
        cx: &Cx,
        home: &Home,
        comment_id: &str,
        moderator: bool,
    ) -> Result<Discussion> {
        let (did, _) = parse_comment_id(comment_id)?;
        let (home_key, number) = parse_discussion_id(&did)?;
        if home_key != home.key() {
            return Err(ForgeError::not_found(format!("comment {comment_id}")));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        Self::author_or_moderator(&rec, cx, moderator)?;
        let mut d = rec.d.clone().unwrap_or_default();
        if d.answer_comment_id != comment_id {
            return Err(ForgeError::invalid_state("that comment is not the answer"));
        }
        d.answer_comment_id.clear();
        d.answer_chosen_by = None;
        d.answer_chosen_at = None;
        let now = tx.now_ms();
        d.updated_at = Some(ts(now));
        rec.d = Some(d);
        row.n1 = Some(now as i64);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        self.record(
            &mut tx,
            home,
            "discussion.unanswered",
            "discussion.unanswer",
            &out,
            &did,
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    // ---- polls and upvotes ------------------------------------------------------------

    /// The poll of a discussion with the viewer's vote.
    pub async fn poll(&self, cx: &Cx, home: &Home, number: u64) -> Result<DiscussionPoll> {
        self.get(cx, home, number)
            .await?
            .poll
            .ok_or_else(|| ForgeError::not_found("poll"))
    }

    /// Casts or changes the caller's vote.
    pub async fn vote(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        option_id: &str,
    ) -> Result<DiscussionPoll> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut d = rec.d.clone().unwrap_or_default();
        let did = d.id.clone();
        let (closed, locked) = (d.closed, d.locked);
        let poll = d.poll.as_mut().ok_or_else(|| {
            ForgeError::invalid_state("the discussion has no poll").with_subcode("no_poll")
        })?;
        if closed || locked {
            return Err(
                ForgeError::invalid_state("closed or locked discussions take no votes")
                    .with_subcode("poll_closed"),
            );
        }
        if !poll.options.iter().any(|o| o.id == option_id) {
            return Err(ForgeError::invalid(
                "unknown_option",
                format!("no poll option {option_id:?}"),
            ));
        }
        let prev = tx.get(&VOTES, &did, &cx.actor_id).await?.and_then(|r| r.k1);
        if prev.as_deref() == Some(option_id) {
            let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
            return out
                .poll
                .ok_or_else(|| ForgeError::internal("poll vanished"));
        }
        match &prev {
            Some(p) => {
                if let Some(o) = poll.options.iter_mut().find(|o| o.id == *p) {
                    o.vote_count = o.vote_count.saturating_sub(1);
                }
            }
            None => poll.total_votes += 1,
        }
        if let Some(o) = poll.options.iter_mut().find(|o| o.id == option_id) {
            o.vote_count += 1;
        }
        tx.put(
            &VOTES,
            &Row::new(did.clone(), cx.actor_id.clone())
                .k1(option_id.to_string())
                .n1(tx.now_ms() as i64),
        )
        .await?;
        rec.d = Some(d);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let payload = serde_json::json!({"action": "poll_voted", "discussion": {"id": did, "number": number}, "option": option_id});
        emit(
            &mut tx,
            "discussion.poll_voted",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.poll_vote",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &did,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        out.poll
            .ok_or_else(|| ForgeError::internal("poll vanished"))
    }

    /// Upvotes the discussion (or a comment); returns the new count.
    pub async fn upvote(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        comment_id: &str,
        add: bool,
    ) -> Result<u32> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut d = rec.d.clone().unwrap_or_default();
        let did = d.id.clone();
        let target = if comment_id.is_empty() {
            "d".to_string()
        } else {
            let (cd, ulid) = parse_comment_id(comment_id)?;
            if cd != did {
                return Err(ForgeError::not_found(format!("comment {comment_id}")));
            }
            ulid
        };
        let vote_id = format!("{target}:{}", cx.actor_id);
        let exists = tx.get(&UPVOTES, &did, &vote_id).await?.is_some();
        if exists == add {
            // Idempotent: already in the requested state.
            return Self::upvote_count(&mut tx, &d, &rec, &did, &target).await;
        }
        let count = if target == "d" {
            if add {
                d.upvote_count += 1;
            } else {
                d.upvote_count = d.upvote_count.saturating_sub(1);
            }
            let c = d.upvote_count;
            rec.d = Some(d.clone());
            row.body = rec.encode_to_vec();
            row.version = tx.update(&DISCUSSIONS, &row).await?;
            c
        } else {
            let Some(mut crow) = tx.get_for_update(&COMMENTS, &did, &target).await? else {
                return Err(ForgeError::not_found(format!("comment {comment_id}")));
            };
            let mut crec: CommentRec = crow.decode()?;
            if add {
                crec.upvotes += 1
            } else {
                crec.upvotes = crec.upvotes.saturating_sub(1)
            }
            let c = crec.upvotes;
            crow.body = crec.encode_to_vec();
            tx.update(&COMMENTS, &crow).await?;
            c
        };
        if add {
            tx.insert(
                &UPVOTES,
                &Row::new(did.clone(), vote_id)
                    .k1(target.clone())
                    .n1(tx.now_ms() as i64),
            )
            .await?;
        } else {
            tx.delete(&UPVOTES, &did, &vote_id).await?;
        }
        let kind = if add {
            "discussion.upvoted"
        } else {
            "discussion.upvote_removed"
        };
        let payload = serde_json::json!({"discussion": {"id": did, "number": number}, "comment_id": comment_id, "count": count});
        emit(
            &mut tx,
            kind,
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &did),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            if add {
                "discussion.upvote"
            } else {
                "discussion.unupvote"
            },
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &did,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(count)
    }

    async fn upvote_count(
        tx: &mut Tx,
        d: &Discussion,
        _rec: &DiscRec,
        did: &str,
        target: &str,
    ) -> Result<u32> {
        if target == "d" {
            return Ok(d.upvote_count);
        }
        Ok(tx
            .get_msg::<CommentRec>(&COMMENTS, did, target)
            .await?
            .map(|(c, _)| c.upvotes)
            .unwrap_or(0))
    }

    // ---- conversion -------------------------------------------------------------------

    /// Converts an issue into a discussion in `category`, keeping its author, body and labels.
    /// The work domain consumes the `discussion.converted_from_issue` event to close and lock
    /// the issue.
    pub async fn convert_from_issue(
        &self,
        cx: &Cx,
        repo: &Repo,
        issue_number: u64,
        category: &str,
        issues: &dyn IssueBridge,
    ) -> Result<Discussion> {
        let issue = issues.get_issue(repo, issue_number).await?;
        let home = Home::of_repo(repo);
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let rows = tx
                .scan(
                    &DISCUSSIONS,
                    &Scan::scope(home.key(), crate::store::MAX_SCAN),
                )
                .await?;
            for r in rows {
                let rec: DiscRec = r.decode()?;
                if rec
                    .d
                    .and_then(|d| d.converted_from)
                    .is_some_and(|s| s.id == issue_number.to_string())
                {
                    return Err(ForgeError::already_exists(format!(
                        "discussion converted from issue #{issue_number}"
                    ))
                    .with_subcode("already_converted"));
                }
            }
        }
        let body = if issue.body.trim().is_empty() {
            "_No description provided._".to_string()
        } else {
            issue.body.clone()
        };
        let from = subject(SubjectKind::Issue, &repo.id, &issue_number.to_string());
        let d = self
            .create_as(
                cx,
                &home,
                NewDiscussion {
                    category: category.to_string(),
                    title: issue.title.clone(),
                    body,
                    labels: issue.labels.clone(),
                    moderator: true,
                    ..Default::default()
                },
                issue.author.clone(),
                Some(from),
            )
            .await?;
        // Best effort: the discussion exists and the event below tells the work domain too.
        let _ = issues
            .converted_to_discussion(repo, issue_number, &d.url, &cx.principal())
            .await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let payload = serde_json::json!({"issue_number": issue_number, "discussion_id": d.id, "discussion_number": d.number, "discussion_url": d.url});
        emit(
            &mut tx,
            "discussion.converted_from_issue",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Issue, &repo.id, &issue_number.to_string()),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.convert_from_issue",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &d.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Converts a discussion into an issue in `target` and links it from the discussion; the
    /// work domain consumes the `discussion.converted_to_issue` event.
    #[allow(clippy::too_many_arguments)]
    pub async fn convert_to_issue(
        &self,
        cx: &Cx,
        home: &Home,
        number: u64,
        target: &Repo,
        title: &str,
        body: &str,
        issues: &dyn IssueBridge,
    ) -> Result<(SubjectRef, Discussion)> {
        let d = self.get(cx, home, number).await?;
        if d.poll.is_some() {
            return Err(
                ForgeError::invalid_state("a poll cannot be converted to an issue")
                    .with_subcode("poll_category"),
            );
        }
        let title = if title.trim().is_empty() {
            d.title.clone()
        } else {
            title.to_string()
        };
        let body = if body.trim().is_empty() {
            format!("{}\n\n---\n_Converted from discussion {}_", d.body, d.url)
        } else {
            body.to_string()
        };
        required("title", &title)?;
        max_len("title", &title, MAX_TITLE)?;
        let labels: Vec<String> = d.labels.iter().map(|l| l.name.clone()).collect();
        let author = d.author.clone().unwrap_or_default();
        let issue_number = issues
            .create_issue(target, &title, &body, &author, &labels)
            .await?;
        let issue_ref = subject(SubjectKind::Issue, &target.id, &issue_number.to_string());
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, mut row) = Self::load_locked(&mut tx, home.key(), number).await?;
        let mut dd = rec.d.clone().unwrap_or_default();
        dd.linked_issues.push(issue_ref.clone());
        rec.d = Some(dd);
        row.body = rec.encode_to_vec();
        row.version = tx.update(&DISCUSSIONS, &row).await?;
        let out = self.view(&mut tx, home, &rec, &row, &cx.actor_id).await?;
        let payload = serde_json::json!({"discussion_id": out.id, "issue_number": issue_number, "issue_repo_id": target.id});
        emit(
            &mut tx,
            "discussion.converted_to_issue",
            &home.org_id,
            &home.repo_id,
            subject(SubjectKind::Discussion, &home.repo_id, &out.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "discussion.convert_to_issue",
            &home.org_id,
            &home.repo_id,
            SubjectKind::Discussion,
            &out.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok((issue_ref, out))
    }
}
