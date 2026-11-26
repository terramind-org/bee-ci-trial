//! Teams: nested groups of org members with maintainers, repo access, review assignment and
//! IdP group sync (PRODUCT §4.20, `TeamService`).
//!
//! # Model
//!
//! - A team belongs to one org, has a unique slug there, an optional parent (no cycles, depth
//!   at most [`MAX_DEPTH`], secret teams cannot nest) and is also a policy group
//!   `@<org>/<slug>`. Deleting a team moves its children to its parent.
//! - Members hold the role `member` or `maintainer`. Members of a child team are inherited
//!   members of its ancestors. A principal who is not yet an org member gets a pending
//!   invitation (which becomes a membership when they join the org, see
//!   [`Teams::resolve_pending`]).
//! - A team holds one role per repo: a built-in role or a custom role id
//!   ([`crate::enterprise::roles`]). Children inherit their ancestors' grants, so a member
//!   of a child team has the parent team's access. [`TeamAccess`] reports it to
//!   [`crate::orgs::Orgs::effective_role`] as an [`crate::orgs::AccessSource`].
//! - A team linked to IdP groups (SAML groups, SCIM groups) is *synced*: membership follows the
//!   groups ([`Teams::sync`]) and manual changes are `INVALID_STATE`.
//! - Review assignment: [`Teams::pick_reviewers`] turns a review request to a team into
//!   individual reviewers, round robin or by load, deterministically from stored state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    Org, Principal, PrincipalKind, Repo, RepoRole, SubjectKind, Team, TeamExternalGroup,
    TeamInvitation, TeamMember, TeamMemberRole, TeamPrivacy, TeamRepoAccess, TeamReviewAlgorithm,
    TeamReviewAssignment,
};

use super::SURFACE;
use super::common::{self, TeamDirectory, get_or_404, principal_of, scan_all, scan_page};
use super::roles::{ResolvedRole, Roles};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::orgs::{AccessSource, role_rank};
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, TableDef, Tx};

/// Deepest team nesting (a top-level team has depth 1).
pub const MAX_DEPTH: usize = 8;
/// Most teams in one org.
pub const MAX_TEAMS: u64 = 5000;
/// Days a team invitation stays pending.
pub const INVITATION_DAYS: u64 = 7;
const DAY_MS: u64 = 86_400_000;

macro_rules! table {
    ($name:ident, $table:literal, $uk1:expr, [$($idx:ident),*], $doc:literal) => {
        #[doc = $doc]
        pub static $name: TableDef = TableDef {
            name: $table,
            surface: SURFACE,
            unique_k1: $uk1,
            unique_k2: false,
            indexes: &[$(Index::$idx),*],
        };
    };
}

table!(
    TEAMS,
    "forge_teams",
    true,
    [ScopeK2],
    "Teams, scoped by org; `k1` slug (unique), `k2` parent team id."
);
table!(
    TEAM_ORG,
    "forge_team_orgs",
    false,
    [],
    "Team id to org id (`k1`)."
);
table!(
    TEAM_MEMBERS,
    "forge_team_members",
    false,
    [K1, ScopeK2],
    "Members, scoped by team; id and `k1` the principal id, `k2` the role."
);
table!(
    TEAM_INVITES,
    "forge_team_invitations",
    false,
    [ScopeK2],
    "Pending invitations, scoped by team; `k2` invitee key, `n1` expiry."
);
table!(
    TEAM_REPOS,
    "forge_team_repos",
    false,
    [K1, K2],
    "Repo grants, scoped by team; id and `k1` the repo id, `k2` the role string."
);
table!(
    TEAM_REVIEW,
    "forge_team_review_settings",
    false,
    [],
    "Review assignment settings, scoped by org, id = team id."
);
table!(
    TEAM_ASSIGNED,
    "forge_team_review_assigned",
    false,
    [],
    "Last assignment time (`n1`) per member, scoped by team."
);
table!(
    TEAM_GROUPS,
    "forge_team_groups",
    false,
    [K1],
    "Linked IdP groups, scoped by team; id and `k1` `provider:group_id`."
);

/// Migration creating the team tables.
pub static M_TEAMS: Migration = Migration {
    module: "enterprise",
    version: 1,
    name: "create_teams",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&TEAMS),
        MigrationOp::CreateTable(&TEAM_ORG),
        MigrationOp::CreateTable(&TEAM_MEMBERS),
        MigrationOp::CreateTable(&TEAM_INVITES),
        MigrationOp::CreateTable(&TEAM_REPOS),
        MigrationOp::CreateTable(&TEAM_REVIEW),
        MigrationOp::CreateTable(&TEAM_ASSIGNED),
        MigrationOp::CreateTable(&TEAM_GROUPS),
    ],
};

/// Who is looking: decides which secret teams are visible.
#[derive(Clone, Debug, Default)]
pub struct Viewer {
    /// The principals of the call (actor first).
    pub ids: Vec<String>,
    /// Whether the viewer owns the org (sees every team).
    pub org_owner: bool,
}

/// A new team.
#[derive(Clone, Debug, Default)]
pub struct NewTeam {
    /// Slug; derived from the name when empty.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Privacy (unspecified = visible).
    pub privacy: TeamPrivacy,
    /// Parent team, slug or id; empty for top level.
    pub parent: String,
    /// @-mentions notify members.
    pub notifications_enabled: bool,
    /// Principals added as maintainers (they must be org members). Empty means the caller.
    pub maintainers: Vec<String>,
    /// Create the team without maintainers (IdP-provisioned teams).
    pub no_maintainers: bool,
}

/// A partial team update.
#[derive(Clone, Debug, Default)]
pub struct TeamPatch {
    /// New slug.
    pub slug: Option<String>,
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New privacy.
    pub privacy: Option<TeamPrivacy>,
    /// New parent (slug or id); `Some("")` moves to top level.
    pub parent: Option<String>,
    /// New notification setting.
    pub notifications_enabled: Option<bool>,
}

/// Who is added to a team.
#[derive(Clone, Debug)]
pub enum Invitee {
    /// A principal id.
    Principal(String),
    /// An email address.
    Email(String),
}

/// The outcome of adding a member.
#[derive(Clone, Debug)]
pub enum AddOutcome {
    /// The principal is now a member.
    Member(TeamMember),
    /// The invitee is not an org member yet: a pending invitation.
    Invitation(TeamInvitation),
}

/// A partial review assignment update.
#[derive(Clone, Debug, Default)]
pub struct ReviewPatch {
    /// Enable or disable.
    pub enabled: Option<bool>,
    /// Algorithm.
    pub algorithm: Option<TeamReviewAlgorithm>,
    /// Reviewers per request.
    pub member_count: Option<u32>,
    /// Replacement excluded list.
    pub excluded: Option<Vec<String>>,
    /// Notify the whole team.
    pub notify_team: Option<bool>,
    /// Skip agents.
    pub skip_agents: Option<bool>,
    /// Child team members are eligible.
    pub count_child_team_members: Option<bool>,
}

/// What a sync changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Principals added.
    pub added: Vec<String>,
    /// Principals removed.
    pub removed: Vec<String>,
    /// Desired principals that are not org members and were skipped.
    pub skipped: Vec<String>,
}

/// Teams over the enterprise surface.
pub struct Teams {
    db: Db,
    forge: Arc<Forge>,
}

fn role_str(r: TeamMemberRole) -> &'static str {
    if r == TeamMemberRole::Maintainer {
        "maintainer"
    } else {
        "member"
    }
}

fn group_key(provider: &str, group_id: &str) -> String {
    format!("{provider}:{group_id}")
}

impl Teams {
    /// Teams over the forge's store.
    pub fn new(forge: Arc<Forge>) -> Teams {
        Teams {
            db: forge.db.clone(),
            forge,
        }
    }

    async fn load(tx: &mut Tx, org_id: &str, r: &str) -> Result<(Team, Row)> {
        if r.is_empty() {
            return Err(ForgeError::missing("team"));
        }
        if let Some(found) = tx.get_msg::<Team>(&TEAMS, org_id, r).await? {
            return Ok(found);
        }
        tx.scan_msgs::<Team>(
            &TEAMS,
            &Scan::scope(org_id, 1).key(Key::K1(r.to_lowercase())),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ForgeError::not_found(format!("team {r}")))
    }

    async fn fill(tx: &mut Tx, org: &Org, mut t: Team) -> Result<Team> {
        t.org = org.name.clone();
        t.policy_group = format!("@{}/{}", org.name, t.slug);
        t.member_count = tx.count(&TEAM_MEMBERS, &t.id, None).await? as u32;
        t.child_team_count = tx
            .count(&TEAMS, &org.id, Some(&Key::K2(t.id.clone())))
            .await? as u32;
        t.repo_count = tx.count(&TEAM_REPOS, &t.id, None).await? as u32;
        t.idp_synced = tx.count(&TEAM_GROUPS, &t.id, None).await? > 0;
        Ok(t)
    }

    /// The ancestors of a team, nearest first (bounded by [`MAX_DEPTH`]).
    async fn ancestors(tx: &mut Tx, org_id: &str, team: &Team) -> Result<Vec<Team>> {
        let mut out = Vec::new();
        let mut parent = team.parent_team_id.clone();
        while !parent.is_empty() && out.len() < MAX_DEPTH {
            match tx.get_msg::<Team>(&TEAMS, org_id, &parent).await? {
                Some((p, _)) => {
                    parent = p.parent_team_id.clone();
                    out.push(p);
                }
                None => break,
            }
        }
        Ok(out)
    }

    async fn descendants(tx: &mut Tx, org_id: &str, team_id: &str) -> Result<Vec<Team>> {
        let mut out: Vec<Team> = Vec::new();
        let mut frontier = vec![team_id.to_string()];
        while let Some(id) = frontier.pop() {
            let kids = scan_all::<Team>(tx, &TEAMS, org_id, Some(Key::K2(id))).await?;
            for (k, _) in kids {
                if out.len() >= MAX_TEAMS as usize {
                    return Ok(out);
                }
                frontier.push(k.id.clone());
                out.push(k);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn is_member_row(tx: &mut Tx, team_id: &str, principal_id: &str) -> Result<bool> {
        Ok(tx
            .get(&TEAM_MEMBERS, team_id, principal_id)
            .await?
            .is_some())
    }

    async fn visible(tx: &mut Tx, team: &Team, v: &Viewer) -> Result<bool> {
        if team.privacy != TeamPrivacy::Secret as i32 || v.org_owner {
            return Ok(true);
        }
        for id in &v.ids {
            if Self::is_member_row(tx, &team.id, id).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn maintains(tx: &mut Tx, org_id: &str, team: &Team, principal_id: &str) -> Result<bool> {
        let mut chain = vec![team.clone()];
        chain.extend(Self::ancestors(tx, org_id, team).await?);
        for t in chain {
            if let Some((m, _)) = tx
                .get_msg::<TeamMember>(&TEAM_MEMBERS, &t.id, principal_id)
                .await?
                && m.role == TeamMemberRole::Maintainer as i32
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `principal_id` maintains the team or one of its ancestors.
    pub async fn is_maintainer(
        &self,
        cx: &Cx,
        org_id: &str,
        team: &str,
        principal_id: &str,
    ) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, org_id, team).await?;
        Self::maintains(&mut tx, org_id, &t, principal_id).await
    }

    fn event_json(t: &Team) -> String {
        common::json(t)
    }

    /// Creates a team.
    pub async fn create(&self, cx: &Cx, org: &Org, n: NewTeam) -> Result<Team> {
        if n.name.is_empty() && n.slug.is_empty() {
            return Err(ForgeError::missing("name"));
        }
        let slug = if n.slug.is_empty() {
            common::slugify(&n.name)
        } else {
            n.slug.clone()
        };
        if !common::valid_slug(&slug) {
            return Err(ForgeError::invalid(
                "invalid_name",
                format!("`{slug}` is not a valid team slug"),
            ));
        }
        let privacy = if n.privacy == TeamPrivacy::Unspecified {
            TeamPrivacy::Visible
        } else {
            n.privacy
        };
        let mut maintainers: BTreeSet<String> = n.maintainers.iter().cloned().collect();
        if maintainers.is_empty() && !n.no_maintainers {
            maintainers.insert(cx.actor_id.clone());
        }
        for m in &maintainers {
            if self.forge.orgs.member(cx, &org.id, m).await?.is_none() {
                return Err(ForgeError::invalid_state(format!(
                    "{m} is not a member of {}, so cannot maintain a team there",
                    org.name
                )));
            }
        }
        let mut principals = Vec::new();
        for m in &maintainers {
            principals.push(principal_of(&self.forge, m).await);
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx
            .scan(
                &TEAMS,
                &Scan::scope(org.id.clone(), 1).key(Key::K1(slug.clone())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("team {slug}")));
        }
        if tx.count(&TEAMS, &org.id, None).await? >= MAX_TEAMS {
            return Err(ForgeError::too_large(
                "teams_per_org",
                MAX_TEAMS,
                MAX_TEAMS + 1,
            ));
        }
        let mut parent_id = String::new();
        if !n.parent.is_empty() {
            let (p, _) = Self::load(&mut tx, &org.id, &n.parent).await?;
            if privacy == TeamPrivacy::Secret || p.privacy == TeamPrivacy::Secret as i32 {
                return Err(ForgeError::invalid(
                    "secret_team_nesting",
                    "secret teams cannot be nested",
                ));
            }
            if Self::ancestors(&mut tx, &org.id, &p).await?.len() + 2 > MAX_DEPTH {
                return Err(ForgeError::too_large(
                    "team_depth",
                    MAX_DEPTH as u64,
                    MAX_DEPTH as u64 + 1,
                ));
            }
            parent_id = p.id;
        }
        let now = tx.now_ms();
        let team = Team {
            id: tx.new_id()?,
            org: org.name.clone(),
            slug: slug.clone(),
            name: if n.name.is_empty() {
                slug.clone()
            } else {
                n.name.clone()
            },
            description: n.description.clone(),
            privacy: privacy as i32,
            parent_team_id: parent_id.clone(),
            notifications_enabled: n.notifications_enabled,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &TEAMS,
            &Row::new(org.id.clone(), team.id.clone())
                .k1(slug)
                .k2(parent_id)
                .msg(&team),
        )
        .await?;
        tx.insert(&TEAM_ORG, &Row::new("", team.id.clone()).k1(org.id.clone()))
            .await?;
        for p in principals {
            let m = TeamMember {
                team_id: team.id.clone(),
                principal: Some(p.clone()),
                role: TeamMemberRole::Maintainer as i32,
                added_at: Some(ts(now)),
                ..Default::default()
            };
            tx.insert(
                &TEAM_MEMBERS,
                &Row::new(team.id.clone(), p.id.clone())
                    .k1(p.id.clone())
                    .k2("maintainer")
                    .msg(&m),
            )
            .await?;
        }
        let team = Self::fill(&mut tx, org, team).await?;
        common::emit(
            &mut tx,
            "team.created",
            &org.id,
            Some((SubjectKind::Team, &team.id)),
            Self::event_json(&team),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.create", &org.id)
                .target(SubjectKind::Team, &team.id)
                .after(&Self::event_json(&team)),
        )
        .await?;
        tx.commit().await?;
        Ok(team)
    }

    /// Reads a team by slug or id; a secret team is `NOT_FOUND` to those who cannot see it.
    pub async fn get(&self, cx: &Cx, org: &Org, team: &str, viewer: &Viewer) -> Result<Team> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !Self::visible(&mut tx, &t, viewer).await? {
            return Err(ForgeError::not_found(format!("team {team}")));
        }
        Self::fill(&mut tx, org, t).await
    }

    /// The raw id of a team (slug or id), without visibility checks (for authorization).
    pub async fn resolve_id(&self, cx: &Cx, org_id: &str, team: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::load(&mut tx, org_id, team).await?.0.id)
    }

    /// The parent of a team (`None` at top level), for authorizing child-team management.
    pub async fn parent_id(&self, cx: &Cx, org_id: &str, team: &str) -> Result<Option<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, org_id, team).await?;
        Ok((!t.parent_team_id.is_empty()).then_some(t.parent_team_id))
    }

    /// Changes a team.
    pub async fn update(&self, cx: &Cx, org: &Org, team: &str, p: TeamPatch) -> Result<Team> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut t, row) = Self::load(&mut tx, &org.id, team).await?;
        let before = Self::event_json(&t);
        let mut next = row.clone();
        if let Some(slug) = &p.slug {
            if !common::valid_slug(slug) {
                return Err(ForgeError::invalid(
                    "invalid_name",
                    format!("`{slug}` is not a valid team slug"),
                ));
            }
            if *slug != t.slug {
                let dup = tx
                    .scan(
                        &TEAMS,
                        &Scan::scope(org.id.clone(), 1).key(Key::K1(slug.clone())),
                    )
                    .await?;
                if !dup.is_empty() {
                    return Err(ForgeError::already_exists(format!("team {slug}")));
                }
                t.slug = slug.clone();
                next = next.k1(slug.clone());
            }
        }
        if let Some(n) = p.name {
            t.name = n;
        }
        if let Some(d) = p.description {
            t.description = d;
        }
        if let Some(b) = p.notifications_enabled {
            t.notifications_enabled = b;
        }
        let new_privacy = p
            .privacy
            .filter(|x| *x != TeamPrivacy::Unspecified)
            .map(|x| x as i32)
            .unwrap_or(t.privacy);
        let mut new_parent = t.parent_team_id.clone();
        if let Some(parent) = &p.parent {
            if parent.is_empty() {
                new_parent.clear();
            } else {
                let (par, _) = Self::load(&mut tx, &org.id, parent).await?;
                if par.id == t.id {
                    return Err(ForgeError::invalid(
                        "team_cycle",
                        "a team cannot be its own parent",
                    ));
                }
                let desc = Self::descendants(&mut tx, &org.id, &t.id).await?;
                if desc.iter().any(|d| d.id == par.id) {
                    return Err(ForgeError::invalid(
                        "team_cycle",
                        "the new parent is a descendant of the team",
                    ));
                }
                let depth_above = Self::ancestors(&mut tx, &org.id, &par).await?.len() + 1;
                let mut depth_below = 0usize;
                for d in &desc {
                    let a = Self::ancestors(&mut tx, &org.id, d).await?.len();
                    depth_below = depth_below.max(a);
                }
                if depth_above + 1 + depth_below > MAX_DEPTH + 1 {
                    return Err(ForgeError::too_large(
                        "team_depth",
                        MAX_DEPTH as u64,
                        MAX_DEPTH as u64 + 1,
                    ));
                }
                if par.privacy == TeamPrivacy::Secret as i32 {
                    return Err(ForgeError::invalid(
                        "secret_team_nesting",
                        "secret teams cannot be nested",
                    ));
                }
                new_parent = par.id;
            }
        }
        if new_privacy == TeamPrivacy::Secret as i32 {
            let has_kids = tx
                .count(&TEAMS, &org.id, Some(&Key::K2(t.id.clone())))
                .await?
                > 0;
            if has_kids || !new_parent.is_empty() {
                return Err(ForgeError::invalid(
                    "secret_team_nesting",
                    "secret teams cannot be nested",
                ));
            }
        }
        t.privacy = new_privacy;
        t.parent_team_id = new_parent.clone();
        next = next.k2(new_parent);
        t.updated_at = Some(ts(tx.now_ms()));
        tx.update(&TEAMS, &next.msg(&t)).await?;
        let t = Self::fill(&mut tx, org, t).await?;
        common::emit(
            &mut tx,
            "team.updated",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            Self::event_json(&t),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.update", &org.id)
                .target(SubjectKind::Team, &t.id)
                .before(&before)
                .after(&Self::event_json(&t)),
        )
        .await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Deletes a team; its child teams move to its parent.
    pub async fn delete(&self, cx: &Cx, org: &Org, team: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        let kids = scan_all::<Team>(&mut tx, &TEAMS, &org.id, Some(Key::K2(t.id.clone()))).await?;
        for (mut k, row) in kids {
            k.parent_team_id = t.parent_team_id.clone();
            tx.update(&TEAMS, &row.clone().k2(t.parent_team_id.clone()).msg(&k))
                .await?;
        }
        for table in [
            &TEAM_MEMBERS,
            &TEAM_INVITES,
            &TEAM_REPOS,
            &TEAM_ASSIGNED,
            &TEAM_GROUPS,
        ] {
            let rows = tx
                .scan(table, &Scan::scope(t.id.clone(), crate::store::MAX_SCAN))
                .await?;
            for r in rows {
                tx.delete(table, &t.id, &r.id).await?;
            }
        }
        tx.delete(&TEAM_REVIEW, &org.id, &t.id).await?;
        tx.delete(&TEAM_ORG, "", &t.id).await?;
        tx.delete(&TEAMS, &org.id, &t.id).await?;
        common::emit(
            &mut tx,
            "team.deleted",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            Self::event_json(&t),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.delete", &org.id)
                .target(SubjectKind::Team, &t.id)
                .before(&Self::event_json(&t)),
        )
        .await?;
        tx.commit().await
    }

    /// Lists the teams the viewer can see, by id, optionally filtered.
    pub async fn list(
        &self,
        cx: &Cx,
        org: &Org,
        query: &str,
        top_level_only: bool,
        viewer: &Viewer,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Team>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let q = query.to_lowercase();
        let mut cursor = after.to_string();
        let mut out = Vec::new();
        loop {
            let (rows, next) =
                scan_page::<Team>(&mut tx, &TEAMS, &org.id, None, &cursor, limit).await?;
            for (t, _) in rows {
                if top_level_only && !t.parent_team_id.is_empty() {
                    continue;
                }
                if !q.is_empty()
                    && !t.slug.to_lowercase().contains(&q)
                    && !t.name.to_lowercase().contains(&q)
                {
                    continue;
                }
                if !Self::visible(&mut tx, &t, viewer).await? {
                    continue;
                }
                if out.len() == limit {
                    let next = cursor_of(&out);
                    return Ok((out, next));
                }
                out.push(Self::fill(&mut tx, org, t).await?);
            }
            if next.is_empty() {
                return Ok((out, String::new()));
            }
            cursor = next;
        }
    }

    /// Lists child teams (direct, or all descendants).
    pub async fn children(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        recursive: bool,
        viewer: &Viewer,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Team>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !Self::visible(&mut tx, &t, viewer).await? {
            return Err(ForgeError::not_found(format!("team {team}")));
        }
        let kids = if recursive {
            Self::descendants(&mut tx, &org.id, &t.id).await?
        } else {
            scan_all::<Team>(&mut tx, &TEAMS, &org.id, Some(Key::K2(t.id.clone())))
                .await?
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let mut out = Vec::new();
        let mut more = false;
        for k in kids.into_iter().filter(|k| k.id.as_str() > after) {
            if !Self::visible(&mut tx, &k, viewer).await? {
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(Self::fill(&mut tx, org, k).await?);
        }
        let next = if more { cursor_of(&out) } else { String::new() };
        Ok((out, next))
    }

    /// Ids of the teams `principal_id` belongs to in `org_id`, plus (inherited) their
    /// ancestors.
    pub async fn team_ids_of(
        tx: &mut Tx,
        org_id: &str,
        principal_id: &str,
        include_inherited: bool,
    ) -> Result<Vec<String>> {
        let rows = tx
            .scan(
                &TEAM_MEMBERS,
                &Scan {
                    key: Some(Key::K1(principal_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut ids = BTreeSet::new();
        for r in rows {
            let Some((team, _)) = tx.get_msg::<Team>(&TEAMS, org_id, &r.scope).await? else {
                continue;
            };
            ids.insert(team.id.clone());
            if include_inherited {
                for a in Self::ancestors(tx, org_id, &team).await? {
                    ids.insert(a.id);
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// The policy groups (`<org>/<slug>`, SPEC §9.4 external groups) of every team
    /// `principal_id` belongs to in any org, directly or through a child team, sorted. Org
    /// names are the current ones (a renamed org's teams follow it).
    pub async fn policy_groups_of(&self, cx: &Cx, principal_id: &str) -> Result<Vec<String>> {
        let mut found: Vec<(String, String, String)> = Vec::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let rows = tx
                .scan(
                    &TEAM_MEMBERS,
                    &Scan {
                        key: Some(Key::K1(principal_id.to_string())),
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                let Some(org_id) = tx.get(&TEAM_ORG, "", &r.scope).await?.and_then(|t| t.k1) else {
                    continue;
                };
                let Some((team, _)) = tx.get_msg::<Team>(&TEAMS, &org_id, &r.scope).await? else {
                    continue;
                };
                for a in Self::ancestors(&mut tx, &org_id, &team).await? {
                    found.push((org_id.clone(), a.org.clone(), a.slug));
                }
                found.push((org_id, team.org, team.slug));
            }
        }
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        let mut out = BTreeSet::new();
        for (org_id, stored, slug) in found {
            let org = match names.get(&org_id) {
                Some(n) => n.clone(),
                None => {
                    let n = match self.forge.orgs.get_org(cx, &org_id).await {
                        Ok(o) => o.name,
                        Err(_) => stored,
                    };
                    names.insert(org_id, n.clone());
                    n
                }
            };
            if !org.is_empty() {
                out.insert(format!("{org}/{slug}"));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Teams a principal belongs to in an org, directly or (inherited) through nesting.
    pub async fn principal_teams(
        &self,
        cx: &Cx,
        org: &Org,
        principal_id: &str,
        include_inherited: bool,
        viewer: &Viewer,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Team>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let ids = Self::team_ids_of(&mut tx, &org.id, principal_id, include_inherited).await?;
        let mut out = Vec::new();
        let mut more = false;
        for id in ids.into_iter().filter(|i| i.as_str() > after) {
            let Some((t, _)) = tx.get_msg::<Team>(&TEAMS, &org.id, &id).await? else {
                continue;
            };
            if !Self::visible(&mut tx, &t, viewer).await? {
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(Self::fill(&mut tx, org, t).await?);
        }
        let next = if more { cursor_of(&out) } else { String::new() };
        Ok((out, next))
    }

    /// Adds a member (or re-roles an existing one), or records an invitation when the invitee
    /// is not an org member. `from_sync` is true when the IdP drives the change (allowed on
    /// synced teams).
    pub async fn add_member(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        who: Invitee,
        role: TeamMemberRole,
        from_sync: Option<&str>,
    ) -> Result<AddOutcome> {
        let role = if role == TeamMemberRole::Unspecified {
            TeamMemberRole::Member
        } else {
            role
        };
        let member_principal = match &who {
            Invitee::Principal(id) => {
                let is_member = self.forge.orgs.member(cx, &org.id, id).await?.is_some();
                is_member.then_some(principal_of(&self.forge, id).await)
            }
            Invitee::Email(_) => None,
        };
        let inviter = cx.principal();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if from_sync.is_none() && tx.count(&TEAM_GROUPS, &t.id, None).await? > 0 {
            return Err(ForgeError::invalid_state(
                "the team is synced from an IdP group; membership is managed by the IdP",
            ));
        }
        let now = tx.now_ms();
        let out = if let Some(p) = member_principal {
            let existing = tx
                .get_msg::<TeamMember>(&TEAM_MEMBERS, &t.id, &p.id)
                .await?;
            let m = TeamMember {
                team_id: t.id.clone(),
                principal: Some(p.clone()),
                role: role as i32,
                inherited: false,
                via_team_id: String::new(),
                external_group_id: from_sync.unwrap_or("").to_string(),
                added_at: existing
                    .as_ref()
                    .and_then(|(e, _)| e.added_at)
                    .or(Some(ts(now))),
            };
            let row = Row::new(t.id.clone(), p.id.clone())
                .k1(p.id.clone())
                .k2(role_str(role))
                .msg(&m);
            let (kind, action) = match existing {
                Some((_, r)) => {
                    tx.update(
                        &TEAM_MEMBERS,
                        &Row {
                            version: r.version,
                            ..row
                        },
                    )
                    .await?;
                    ("team.member_role_changed", "team.member_role_change")
                }
                None => {
                    tx.insert(&TEAM_MEMBERS, &row).await?;
                    ("team.member_added", "team.member_add")
                }
            };
            common::emit(
                &mut tx,
                kind,
                &org.id,
                Some((SubjectKind::Team, &t.id)),
                serde_json::json!({"team_id": t.id, "principal_id": p.id, "role": role_str(role)})
                    .to_string(),
            )
            .await?;
            tx.audit(
                AuditDraft::new(action, &org.id)
                    .target(SubjectKind::Team, &t.id)
                    .after(
                        &serde_json::json!({"principal_id": p.id, "role": role_str(role)})
                            .to_string(),
                    ),
            )
            .await?;
            AddOutcome::Member(m)
        } else {
            let (key, pid, email) = match &who {
                Invitee::Principal(id) => (id.clone(), id.clone(), String::new()),
                Invitee::Email(e) => (
                    format!("email:{}", e.to_lowercase()),
                    String::new(),
                    e.clone(),
                ),
            };
            let dup = tx
                .scan(
                    &TEAM_INVITES,
                    &Scan::scope(t.id.clone(), 1).key(Key::K2(key.clone())),
                )
                .await?;
            if dup.iter().any(|r| r.n1.unwrap_or(0) as u64 > now) {
                return Err(ForgeError::already_exists("team invitation"));
            }
            let expires = now + INVITATION_DAYS * DAY_MS;
            let inv = TeamInvitation {
                id: tx.new_id()?,
                team_id: t.id.clone(),
                invitee_principal_id: pid,
                invitee_email: email,
                role: role as i32,
                inviter: Some(inviter),
                created_at: Some(ts(now)),
                expires_at: Some(ts(expires)),
            };
            tx.insert(
                &TEAM_INVITES,
                &Row::new(t.id.clone(), inv.id.clone())
                    .k2(key)
                    .n1(expires as i64)
                    .msg(&inv),
            )
            .await?;
            common::emit(
                &mut tx,
                "team.invitation_created",
                &org.id,
                Some((SubjectKind::Team, &t.id)),
                common::json(&inv),
            )
            .await?;
            tx.audit(
                AuditDraft::new("team.invitation_create", &org.id)
                    .target(SubjectKind::Team, &t.id)
                    .after(&common::json(&inv)),
            )
            .await?;
            AddOutcome::Invitation(inv)
        };
        tx.commit().await?;
        Ok(out)
    }

    /// Removes a member.
    pub async fn remove_member(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        principal_id: &str,
        from_sync: bool,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !from_sync && tx.count(&TEAM_GROUPS, &t.id, None).await? > 0 {
            return Err(ForgeError::invalid_state(
                "the team is synced from an IdP group; membership is managed by the IdP",
            ));
        }
        if !tx.delete(&TEAM_MEMBERS, &t.id, principal_id).await? {
            return Err(ForgeError::not_found(format!("member {principal_id}")));
        }
        common::emit(
            &mut tx,
            "team.member_removed",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            serde_json::json!({"team_id": t.id, "principal_id": principal_id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.member_remove", &org.id)
                .target(SubjectKind::Team, &t.id)
                .after(&serde_json::json!({"principal_id": principal_id}).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// Lists members, optionally of one role and optionally including members of child teams
    /// (marked inherited).
    pub async fn members(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        role: TeamMemberRole,
        include_child_teams: bool,
        viewer: &Viewer,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<TeamMember>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !Self::visible(&mut tx, &t, viewer).await? {
            return Err(ForgeError::not_found(format!("team {team}")));
        }
        let mut all: BTreeMap<String, TeamMember> = BTreeMap::new();
        if include_child_teams {
            for d in Self::descendants(&mut tx, &org.id, &t.id).await? {
                for (mut m, _) in
                    scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &d.id, None).await?
                {
                    m.inherited = true;
                    m.via_team_id = d.id.clone();
                    m.team_id = t.id.clone();
                    if let Some(p) = &m.principal {
                        all.entry(p.id.clone()).or_insert(m);
                    }
                }
            }
        }
        for (m, row) in scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &t.id, None).await? {
            all.insert(row.id, m);
        }
        let mut out: Vec<TeamMember> = all
            .into_iter()
            .filter(|(id, m)| {
                id.as_str() > after
                    && (role == TeamMemberRole::Unspecified || m.role == role as i32)
            })
            .map(|(_, m)| m)
            .collect();
        let more = out.len() > limit;
        out.truncate(limit);
        let next = if more {
            out.last()
                .and_then(|m| m.principal.as_ref().map(|p| p.id.clone()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// One membership (direct, or inherited through a child team).
    pub async fn membership(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        principal_id: &str,
    ) -> Result<TeamMember> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if let Some((m, _)) = tx
            .get_msg::<TeamMember>(&TEAM_MEMBERS, &t.id, principal_id)
            .await?
        {
            return Ok(m);
        }
        for d in Self::descendants(&mut tx, &org.id, &t.id).await? {
            if let Some((mut m, _)) = tx
                .get_msg::<TeamMember>(&TEAM_MEMBERS, &d.id, principal_id)
                .await?
            {
                m.inherited = true;
                m.via_team_id = d.id.clone();
                m.team_id = t.id.clone();
                return Ok(m);
            }
        }
        Err(ForgeError::not_found(format!(
            "membership of {principal_id}"
        )))
    }

    /// Pending (unexpired) invitations of a team.
    pub async fn invitations(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<TeamInvitation>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        let now = tx.now_ms() as i64;
        let (rows, next) =
            scan_page::<TeamInvitation>(&mut tx, &TEAM_INVITES, &t.id, None, after, limit).await?;
        Ok((
            rows.into_iter()
                .filter(|(_, r)| r.n1.unwrap_or(0) > now)
                .map(|(i, _)| i)
                .collect(),
            next,
        ))
    }

    /// Cancels a pending invitation.
    pub async fn cancel_invitation(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        invitation_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !tx.delete(&TEAM_INVITES, &t.id, invitation_id).await? {
            return Err(ForgeError::not_found(format!("invitation {invitation_id}")));
        }
        common::emit(
            &mut tx,
            "team.invitation_cancelled",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            serde_json::json!({"team_id": t.id, "invitation_id": invitation_id}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.invitation_cancel", &org.id)
                .target(SubjectKind::Team, &t.id)
                .after(&serde_json::json!({"invitation_id": invitation_id}).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// Turns pending invitations addressed to `principal_id` (or one of its `emails`) into
    /// memberships, after the principal joined the org. Returns the teams joined.
    pub async fn resolve_pending(
        &self,
        cx: &Cx,
        org: &Org,
        principal_id: &str,
        emails: &[String],
    ) -> Result<Vec<String>> {
        let mut keys = vec![principal_id.to_string()];
        keys.extend(emails.iter().map(|e| format!("email:{}", e.to_lowercase())));
        let mut pending: Vec<(String, String)> = Vec::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let now = tx.now_ms() as i64;
            let teams = scan_all::<Team>(&mut tx, &TEAMS, &org.id, None).await?;
            for (t, _) in teams {
                for k in &keys {
                    let rows = tx
                        .scan_msgs::<TeamInvitation>(
                            &TEAM_INVITES,
                            &Scan::scope(t.id.clone(), 10).key(Key::K2(k.clone())),
                        )
                        .await?;
                    for (inv, r) in rows {
                        if r.n1.unwrap_or(0) > now {
                            pending.push((t.id.clone(), inv.id));
                        }
                    }
                }
            }
        }
        if self
            .forge
            .orgs
            .member(cx, &org.id, principal_id)
            .await?
            .is_none()
        {
            // Keep invitations pending because the principal is not a member of the org.
            return Ok(Vec::new());
        }
        let mut joined = Vec::new();
        let principal = principal_of(&self.forge, principal_id).await;
        for (team_id, inv_id) in pending {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some(invitation_row) = tx.get_for_update(&TEAM_INVITES, &team_id, &inv_id).await?
            else {
                continue;
            };
            let invitation = invitation_row.decode::<TeamInvitation>()?;
            if invitation_row.n1.unwrap_or(0) <= tx.now_ms() as i64 {
                continue;
            }
            let role = TeamMemberRole::try_from(invitation.role).unwrap_or(TeamMemberRole::Member);
            let existing = tx
                .get_msg::<TeamMember>(&TEAM_MEMBERS, &team_id, principal_id)
                .await?;
            let member = TeamMember {
                team_id: team_id.clone(),
                principal: Some(principal.clone()),
                role: role as i32,
                inherited: false,
                via_team_id: String::new(),
                external_group_id: "invitation".into(),
                added_at: existing
                    .as_ref()
                    .and_then(|(member, _)| member.added_at)
                    .or(Some(ts(tx.now_ms()))),
            };
            let member_row = Row::new(team_id.clone(), principal_id.to_string())
                .k1(principal_id.to_string())
                .k2(role_str(role))
                .msg(&member);
            let (event_kind, audit_action) = match existing {
                Some((_, row)) => {
                    tx.update(
                        &TEAM_MEMBERS,
                        &Row {
                            version: row.version,
                            ..member_row
                        },
                    )
                    .await?;
                    ("team.member_role_changed", "team.member_role_change")
                }
                None => {
                    tx.insert(&TEAM_MEMBERS, &member_row).await?;
                    ("team.member_added", "team.member_add")
                }
            };
            common::emit(
                &mut tx,
                event_kind,
                &org.id,
                Some((SubjectKind::Team, &team_id)),
                serde_json::json!({
                    "team_id": team_id,
                    "principal_id": principal_id,
                    "role": role_str(role)
                })
                .to_string(),
            )
            .await?;
            tx.audit(
                AuditDraft::new(audit_action, &org.id)
                    .target(SubjectKind::Team, &team_id)
                    .after(
                        &serde_json::json!({
                            "principal_id": principal_id,
                            "role": role_str(role)
                        })
                        .to_string(),
                    ),
            )
            .await?;
            if !tx.delete(&TEAM_INVITES, &team_id, &inv_id).await? {
                return Err(ForgeError::not_found(format!("invitation {inv_id}")));
            }
            tx.commit().await?;
            joined.push(team_id);
        }
        Ok(joined)
    }

    /// Removes a principal from every team of the given orgs (org removal, deprovisioning).
    /// Returns the ids of the teams left.
    pub async fn remove_principal_everywhere(
        &self,
        cx: &Cx,
        org_ids: &[String],
        principal_id: &str,
    ) -> Result<Vec<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &TEAM_MEMBERS,
                &Scan {
                    key: Some(Key::K1(principal_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut left = Vec::new();
        for r in rows {
            let mut owner = None;
            for org in org_ids {
                if tx.get(&TEAMS, org, &r.scope).await?.is_some() {
                    owner = Some(org.clone());
                    break;
                }
            }
            let Some(org) = owner else { continue };
            tx.delete(&TEAM_MEMBERS, &r.scope, &r.id).await?;
            common::emit(
                &mut tx,
                "team.member_removed",
                &org,
                Some((SubjectKind::Team, &r.scope)),
                serde_json::json!({"team_id": r.scope, "principal_id": principal_id}).to_string(),
            )
            .await?;
            tx.audit(
                AuditDraft::new("team.member_remove", &org)
                    .target(SubjectKind::Team, &r.scope)
                    .after(&serde_json::json!({"principal_id": principal_id, "reason": "removed_everywhere"}).to_string()),
            )
            .await?;
            left.push(r.scope.clone());
        }
        // Pending invitations addressed to the principal go too.
        for org in org_ids {
            let teams = scan_all::<Team>(&mut tx, &TEAMS, org, None).await?;
            for (t, _) in teams {
                let invs = tx
                    .scan(
                        &TEAM_INVITES,
                        &Scan::scope(t.id.clone(), 100).key(Key::K2(principal_id.to_string())),
                    )
                    .await?;
                for i in invs {
                    tx.delete(&TEAM_INVITES, &t.id, &i.id).await?;
                }
            }
        }
        tx.commit().await?;
        Ok(left)
    }

    /// The org of a team (by id).
    pub async fn org_of(&self, cx: &Cx, team_id: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get(&TEAM_ORG, "", team_id)
            .await?
            .and_then(|r| r.k1)
            .ok_or_else(|| ForgeError::not_found(format!("team {team_id}")))
    }

    /// Grants a team a role on a repo of the org (or changes it). `role` is a built-in role or
    /// a custom role id.
    pub async fn grant_repo(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        repo: &Repo,
        role: &str,
    ) -> Result<TeamRepoAccess> {
        if repo.org_id != org.id {
            return Err(ForgeError::invalid(
                "repo_not_in_org",
                format!("{} does not belong to {}", repo.full_name, org.name),
            ));
        }
        if role.is_empty() {
            return Err(ForgeError::missing("role"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        Roles::resolve_in(&mut tx, &org.id, role).await?;
        let existing = tx.get(&TEAM_REPOS, &t.id, &repo.id).await?;
        let access = TeamRepoAccess {
            team_id: t.id.clone(),
            repo: Some(bee_proto::v1::RepoRef {
                repo: Some(bee_proto::v1::repo_ref::Repo::Id(repo.id.clone())),
            }),
            role: role.into(),
            inherited: false,
            via_team_id: String::new(),
            granted_at: Some(ts(tx.now_ms())),
        };
        let row = Row::new(t.id.clone(), repo.id.clone())
            .k1(repo.id.clone())
            .k2(role.to_string())
            .msg(&access);
        match existing {
            Some(r) => {
                tx.update(
                    &TEAM_REPOS,
                    &Row {
                        version: r.version,
                        ..row
                    },
                )
                .await?;
            }
            None => tx.insert(&TEAM_REPOS, &row).await?,
        }
        let after =
            serde_json::json!({"team_id": t.id, "repo_id": repo.id, "role": role}).to_string();
        common::emit(
            &mut tx,
            "team.repo_granted",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            after.clone(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.repo_grant", &org.id)
                .repo_id(&repo.id)
                .target(SubjectKind::Team, &t.id)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(access)
    }

    /// Removes a team's access to a repo.
    pub async fn revoke_repo(&self, cx: &Cx, org: &Org, team: &str, repo: &Repo) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !tx.delete(&TEAM_REPOS, &t.id, &repo.id).await? {
            return Err(ForgeError::not_found(format!(
                "access of team {} to {}",
                t.slug, repo.full_name
            )));
        }
        let after = serde_json::json!({"team_id": t.id, "repo_id": repo.id}).to_string();
        common::emit(
            &mut tx,
            "team.repo_revoked",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            after.clone(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.repo_revoke", &org.id)
                .repo_id(&repo.id)
                .target(SubjectKind::Team, &t.id)
                .after(&after),
        )
        .await?;
        tx.commit().await
    }

    /// The repos a team can access; with `include_inherited`, also those of its ancestors.
    pub async fn team_repos(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        include_inherited: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<TeamRepoAccess>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        let mut all: BTreeMap<String, TeamRepoAccess> = BTreeMap::new();
        if include_inherited {
            for a in Self::ancestors(&mut tx, &org.id, &t)
                .await?
                .into_iter()
                .rev()
            {
                for (mut g, r) in
                    scan_all::<TeamRepoAccess>(&mut tx, &TEAM_REPOS, &a.id, None).await?
                {
                    g.inherited = true;
                    g.via_team_id = a.id.clone();
                    g.team_id = t.id.clone();
                    all.insert(r.id, g);
                }
            }
        }
        for (g, r) in scan_all::<TeamRepoAccess>(&mut tx, &TEAM_REPOS, &t.id, None).await? {
            all.insert(r.id, g);
        }
        let mut out: Vec<TeamRepoAccess> = all
            .into_iter()
            .filter(|(id, _)| id.as_str() > after)
            .map(|(_, g)| g)
            .collect();
        let more = out.len() > limit;
        out.truncate(limit);
        let next = if more {
            out.last().map(repo_id_of).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// The teams with direct access to a repo.
    pub async fn repo_teams(
        &self,
        cx: &Cx,
        repo_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<TeamRepoAccess>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<TeamRepoAccess>(
                &TEAM_REPOS,
                &Scan {
                    key: Some(Key::K1(repo_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut out: Vec<TeamRepoAccess> = rows
            .into_iter()
            .map(|(g, _)| g)
            .filter(|g| g.team_id.as_str() > after)
            .collect();
        out.sort_by(|a, b| a.team_id.cmp(&b.team_id));
        let more = out.len() > limit;
        out.truncate(limit);
        let next = if more {
            out.last().map(|g| g.team_id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// Roles a principal holds on a repo through teams, with a label per team.
    pub async fn team_grants(
        tx: &mut Tx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(ResolvedRole, String)>> {
        let mut out = Vec::new();
        let ids = Self::team_ids_of(tx, &repo.org_id, principal_id, true).await?;
        for id in ids {
            let Some((g, _)) = tx
                .get_msg::<TeamRepoAccess>(&TEAM_REPOS, &id, &repo.id)
                .await?
            else {
                continue;
            };
            let slug = tx
                .get_msg::<Team>(&TEAMS, &repo.org_id, &id)
                .await?
                .map(|(t, _)| t.slug)
                .unwrap_or_default();
            if let Ok(role) = Roles::resolve_in(tx, &repo.org_id, &g.role).await {
                out.push((role, format!("team:{slug}")));
            }
        }
        Ok(out)
    }

    /// The review assignment settings of a team.
    pub async fn review_assignment(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
    ) -> Result<TeamReviewAssignment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        Ok(Self::review_settings(&mut tx, &org.id, &t.id).await?.0)
    }

    async fn review_settings(
        tx: &mut Tx,
        org_id: &str,
        team_id: &str,
    ) -> Result<(TeamReviewAssignment, Option<Row>)> {
        Ok(
            match tx
                .get_msg::<TeamReviewAssignment>(&TEAM_REVIEW, org_id, team_id)
                .await?
            {
                Some((s, r)) => (s, Some(r)),
                None => (
                    TeamReviewAssignment {
                        team_id: team_id.into(),
                        enabled: false,
                        algorithm: TeamReviewAlgorithm::RoundRobin as i32,
                        member_count: 1,
                        ..Default::default()
                    },
                    None,
                ),
            },
        )
    }

    /// Changes the review assignment settings.
    pub async fn update_review_assignment(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        p: ReviewPatch,
    ) -> Result<TeamReviewAssignment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        let (mut s, row) = Self::review_settings(&mut tx, &org.id, &t.id).await?;
        let before = common::json(&s);
        if let Some(v) = p.enabled {
            s.enabled = v;
        }
        if let Some(a) = p
            .algorithm
            .filter(|a| *a != TeamReviewAlgorithm::Unspecified)
        {
            s.algorithm = a as i32;
        }
        if let Some(n) = p.member_count {
            if n == 0 || n > 100 {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "member_count is between 1 and 100",
                ));
            }
            s.member_count = n;
        }
        if let Some(mut ex) = p.excluded {
            ex.sort();
            ex.dedup();
            s.excluded_member_ids = ex;
        }
        if let Some(v) = p.notify_team {
            s.notify_team = v;
        }
        if let Some(v) = p.skip_agents {
            s.skip_agents = v;
        }
        if let Some(v) = p.count_child_team_members {
            s.count_child_team_members = v;
        }
        match row {
            Some(r) => {
                tx.update(&TEAM_REVIEW, &r.msg(&s)).await?;
            }
            None => {
                tx.insert(
                    &TEAM_REVIEW,
                    &Row::new(org.id.clone(), t.id.clone()).msg(&s),
                )
                .await?
            }
        }
        common::emit(
            &mut tx,
            "team.review_assignment_updated",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            common::json(&s),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.review_assignment_update", &org.id)
                .target(SubjectKind::Team, &t.id)
                .before(&before)
                .after(&common::json(&s)),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Picks reviewers for a review request to a team. Eligible: members (and, when enabled,
    /// child team members) minus the excluded, the requester-supplied `exclude` (for example
    /// the author) and, with `skip_agents`, agents. Round robin picks the members assigned
    /// longest ago; load balancing the members with the fewest `outstanding` requests, ties
    /// broken by last assignment then id. Records the assignment time. Empty when review
    /// assignment is disabled.
    pub async fn pick_reviewers(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        exclude: &[String],
        outstanding: &BTreeMap<String, u32>,
    ) -> Result<Vec<String>> {
        let (t, settings, mut candidates) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (t, _) = Self::load(&mut tx, &org.id, team).await?;
            let (s, _) = Self::review_settings(&mut tx, &org.id, &t.id).await?;
            let mut ids: BTreeSet<String> =
                scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &t.id, None)
                    .await?
                    .into_iter()
                    .map(|(_, r)| r.id)
                    .collect();
            if s.count_child_team_members {
                for d in Self::descendants(&mut tx, &org.id, &t.id).await? {
                    for (_, r) in
                        scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &d.id, None).await?
                    {
                        ids.insert(r.id);
                    }
                }
            }
            (t, s, ids)
        };
        if !settings.enabled {
            return Ok(Vec::new());
        }
        for e in exclude.iter().chain(settings.excluded_member_ids.iter()) {
            candidates.remove(e);
        }
        if settings.skip_agents {
            let mut humans = BTreeSet::new();
            for c in &candidates {
                let p = principal_of(&self.forge, c).await;
                if p.kind != PrincipalKind::Agent as i32 {
                    humans.insert(c.clone());
                }
            }
            candidates = humans;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms() as i64;
        let mut ranked: Vec<(u32, i64, String)> = Vec::new();
        for c in candidates {
            let last = tx
                .get(&TEAM_ASSIGNED, &t.id, &c)
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0);
            let load = if settings.algorithm == TeamReviewAlgorithm::LoadBalance as i32 {
                outstanding.get(&c).copied().unwrap_or(0)
            } else {
                0
            };
            ranked.push((load, last, c));
        }
        ranked.sort();
        let picked: Vec<String> = ranked
            .into_iter()
            .take(settings.member_count.max(1) as usize)
            .map(|(_, _, c)| c)
            .collect();
        for p in &picked {
            tx.put(&TEAM_ASSIGNED, &Row::new(t.id.clone(), p.clone()).n1(now))
                .await?;
        }
        tx.commit().await?;
        Ok(picked)
    }

    /// Links an IdP group to a team. The caller re-syncs the team afterwards.
    pub async fn link_group(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        provider: &str,
        group_id: &str,
        group_name: &str,
    ) -> Result<TeamExternalGroup> {
        let provider = provider.to_lowercase();
        if provider.is_empty() || provider.contains(':') {
            return Err(ForgeError::invalid(
                "invalid_provider",
                "provider is a short name without `:`",
            ));
        }
        if group_id.is_empty() {
            return Err(ForgeError::missing("group_id"));
        }
        let key = group_key(&provider, group_id);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if tx.get(&TEAM_GROUPS, &t.id, &key).await?.is_some() {
            return Err(ForgeError::already_exists(format!("group link {key}")));
        }
        let g = TeamExternalGroup {
            team_id: t.id.clone(),
            provider,
            group_id: group_id.into(),
            group_name: group_name.into(),
            linked_at: Some(ts(tx.now_ms())),
            last_synced_at: None,
            member_count: 0,
        };
        tx.insert(
            &TEAM_GROUPS,
            &Row::new(t.id.clone(), key.clone()).k1(key.clone()).msg(&g),
        )
        .await?;
        common::emit(
            &mut tx,
            "team.group_linked",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            common::json(&g),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.group_link", &org.id)
                .target(SubjectKind::Team, &t.id)
                .after(&common::json(&g)),
        )
        .await?;
        tx.commit().await?;
        Ok(g)
    }

    /// Unlinks an IdP group; current members stay.
    pub async fn unlink_group(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        provider: &str,
        group_id: &str,
    ) -> Result<()> {
        let key = group_key(&provider.to_lowercase(), group_id);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        if !tx.delete(&TEAM_GROUPS, &t.id, &key).await? {
            return Err(ForgeError::not_found(format!("group link {key}")));
        }
        // Members keep their place but no longer belong to the group: they become manual.
        let members = scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &t.id, None).await?;
        for (mut m, row) in members {
            if m.external_group_id == key {
                m.external_group_id.clear();
                tx.update(&TEAM_MEMBERS, &row.msg(&m)).await?;
            }
        }
        common::emit(
            &mut tx,
            "team.group_unlinked",
            &org.id,
            Some((SubjectKind::Team, &t.id)),
            serde_json::json!({"team_id": t.id, "group": key}).to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("team.group_unlink", &org.id)
                .target(SubjectKind::Team, &t.id)
                .after(&serde_json::json!({"group": key}).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// The IdP groups linked to a team, with synced member counts.
    pub async fn linked_groups(
        &self,
        cx: &Cx,
        org: &Org,
        team: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<TeamExternalGroup>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, &org.id, team).await?;
        let (rows, next) =
            scan_page::<TeamExternalGroup>(&mut tx, &TEAM_GROUPS, &t.id, None, after, limit)
                .await?;
        let members = scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &t.id, None).await?;
        let out = rows
            .into_iter()
            .map(|(mut g, r)| {
                g.member_count = members
                    .iter()
                    .filter(|(m, _)| m.external_group_id == r.id)
                    .count() as u32;
                g
            })
            .collect();
        Ok((out, next))
    }

    /// Every (org id, team id, group key) link of an IdP group, across orgs.
    pub async fn teams_linked_to(
        &self,
        cx: &Cx,
        provider: &str,
        group_id: &str,
    ) -> Result<Vec<(String, String)>> {
        let key = group_key(&provider.to_lowercase(), group_id);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &TEAM_GROUPS,
                &Scan {
                    key: Some(Key::K1(key)),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut out = Vec::new();
        for r in rows {
            if let Some(org) = tx.get(&TEAM_ORG, "", &r.scope).await?.and_then(|t| t.k1) {
                out.push((org, r.scope));
            }
        }
        Ok(out)
    }

    /// The `(provider, group_id)` links of a team.
    pub async fn group_links(&self, cx: &Cx, team_id: &str) -> Result<Vec<(String, String)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(
            scan_all::<TeamExternalGroup>(&mut tx, &TEAM_GROUPS, team_id, None)
                .await?
                .into_iter()
                .map(|(g, _)| (g.provider, g.group_id))
                .collect(),
        )
    }

    /// Makes a synced team's membership equal to `desired` (principal id to the group key it
    /// comes from): adds missing members, removes members that came from a group and are no
    /// longer desired. Manually added members stay. Desired principals that are not org
    /// members are skipped.
    pub async fn sync(
        &self,
        cx: &Cx,
        org: &Org,
        team_id: &str,
        desired: &BTreeMap<String, String>,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        let mut eligible = BTreeMap::new();
        for (p, g) in desired {
            if self.forge.orgs.member(cx, &org.id, p).await?.is_some() {
                eligible.insert(p.clone(), g.clone());
            } else {
                report.skipped.push(p.clone());
            }
        }
        let current: BTreeMap<String, TeamMember> = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            get_or_404::<Team>(&mut tx, &TEAMS, &org.id, team_id, "team").await?;
            scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, team_id, None)
                .await?
                .into_iter()
                .map(|(m, r)| (r.id, m))
                .collect()
        };
        for (p, g) in &eligible {
            match current.get(p) {
                Some(m) if m.external_group_id == *g || m.external_group_id.is_empty() => {}
                Some(_) | None => {
                    let role = current
                        .get(p)
                        .map(|m| TeamMemberRole::try_from(m.role).unwrap_or(TeamMemberRole::Member))
                        .unwrap_or(TeamMemberRole::Member);
                    self.add_member(
                        cx,
                        org,
                        team_id,
                        Invitee::Principal(p.clone()),
                        role,
                        Some(g),
                    )
                    .await?;
                    report.added.push(p.clone());
                }
            }
        }
        for (p, m) in &current {
            if !m.external_group_id.is_empty() && !eligible.contains_key(p) {
                self.remove_member(cx, org, team_id, p, true).await?;
                report.removed.push(p.clone());
            }
        }
        if !report.added.is_empty() || !report.removed.is_empty() {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            common::emit(
                &mut tx,
                "team.synced",
                &org.id,
                Some((SubjectKind::Team, team_id)),
                serde_json::json!({"team_id": team_id, "added": report.added, "removed": report.removed})
                    .to_string(),
            )
            .await?;
            let links = scan_all::<TeamExternalGroup>(&mut tx, &TEAM_GROUPS, team_id, None).await?;
            let now = tx.now_ms();
            for (mut g, r) in links {
                g.last_synced_at = Some(ts(now));
                tx.update(&TEAM_GROUPS, &r.msg(&g)).await?;
            }
            tx.commit().await?;
        }
        Ok(report)
    }

    /// Whether a principal is in a team, directly or through a descendant team.
    pub async fn is_member(&self, cx: &Cx, team_id: &str, principal_id: &str) -> Result<bool> {
        let org_id = self.org_of(cx, team_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if Self::is_member_row(&mut tx, team_id, principal_id).await? {
            return Ok(true);
        }
        for d in Self::descendants(&mut tx, &org_id, team_id).await? {
            if Self::is_member_row(&mut tx, &d.id, principal_id).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ids of the members of a team (slug or id), optionally including child teams' members.
    pub async fn member_ids(
        &self,
        cx: &Cx,
        org_id: &str,
        team: &str,
        include_children: bool,
    ) -> Result<Vec<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, org_id, team).await?;
        let mut ids: BTreeSet<String> = scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &t.id, None)
            .await?
            .into_iter()
            .map(|(_, r)| r.id)
            .collect();
        if include_children {
            for d in Self::descendants(&mut tx, org_id, &t.id).await? {
                for (_, r) in scan_all::<TeamMember>(&mut tx, &TEAM_MEMBERS, &d.id, None).await? {
                    ids.insert(r.id);
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// The review assignment settings of a team when some were stored.
    pub async fn configured_assignment(
        &self,
        cx: &Cx,
        org_id: &str,
        team: &str,
    ) -> Result<Option<TeamReviewAssignment>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::load(&mut tx, org_id, team).await?;
        Ok(tx
            .get_msg::<TeamReviewAssignment>(&TEAM_REVIEW, org_id, &t.id)
            .await?
            .map(|(s, _)| s))
    }

    /// The principal for a SCIM-style principal id lookup helper used by tests and services.
    pub async fn principal(&self, id: &str) -> Principal {
        principal_of(&self.forge, id).await
    }
}

fn repo_id_of(g: &TeamRepoAccess) -> String {
    match g.repo.as_ref().and_then(|r| r.repo.as_ref()) {
        Some(bee_proto::v1::repo_ref::Repo::Id(i)) => i.clone(),
        Some(bee_proto::v1::repo_ref::Repo::FullName(n)) => n.clone(),
        None => String::new(),
    }
}

fn cursor_of(items: &[Team]) -> String {
    items.last().map(|t| t.id.clone()).unwrap_or_default()
}

#[async_trait]
impl TeamDirectory for Teams {
    async fn is_team_member(&self, cx: &Cx, team_id: &str, principal_id: &str) -> Result<bool> {
        self.is_member(cx, team_id, principal_id).await
    }

    async fn team_org(&self, cx: &Cx, team_id: &str) -> Result<String> {
        self.org_of(cx, team_id).await
    }
}

/// Reports team repo grants to [`crate::orgs::Orgs::effective_role`].
pub struct TeamAccess {
    db: Db,
}

impl TeamAccess {
    /// An access source over the forge's store.
    pub fn new(forge: &Forge) -> TeamAccess {
        TeamAccess {
            db: forge.db.clone(),
        }
    }
}

#[async_trait]
impl AccessSource for TeamAccess {
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let grants = Teams::team_grants(&mut tx, repo, principal_id).await?;
        Ok(grants
            .into_iter()
            .filter(|(r, _)| role_rank(r.base) > 0)
            .map(|(r, label)| {
                let label = match &r.custom {
                    Some(c) => format!("{label}[{}]", c.name),
                    None => label,
                };
                (r.base, label)
            })
            .collect())
    }
}
