//! The effective policy: overlay, cross-reference validation, and the access, ref, owner and
//! push evaluators.

use std::collections::{BTreeMap, BTreeSet};

use crate::action::{Action, PathMode, builtin_role_actions};
use crate::canon;
use crate::error::{Location, Origin, PolicyError, PolicyErrorKind, Reason};
use crate::model::{AccessRule, PolicyDoc, PushRules, RulesetMode};
use crate::owners::{Codeowners, OwnerRef};
use crate::parse::parse_doc;
use crate::path::{display_prefix, normalize_request_path, prefix_covers, prefix_depth};
use crate::pattern::most_specific;
use crate::subject::{Actor, BUILTIN_GROUPS, Subject};

/// The result of an access or ref check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request is allowed.
    pub allowed: bool,
    /// The rule that decided, e.g. `access[/docs].write`, `members`, `refs[release/*].delete`.
    pub rule: String,
    /// Set when denied.
    pub reason: Option<Reason>,
    /// Human-readable explanation.
    pub detail: String,
}

impl Decision {
    fn allow(rule: String, detail: String) -> Decision {
        Decision {
            allowed: true,
            rule,
            reason: None,
            detail,
        }
    }

    fn deny(rule: String, detail: String) -> Decision {
        Decision {
            allowed: false,
            rule,
            reason: Some(Reason::PolicyDenied),
            detail,
        }
    }
}

/// Ref operations governed by ref rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAction {
    /// Create a ref.
    Create,
    /// Move a ref by plain CAS (`UpdateRef`). Never allowed on protected refs; those move only
    /// through Land.
    Update,
    /// Delete a ref.
    Delete,
    /// Move a ref to a target that does not descend from its current one.
    ForceUpdate,
}

impl RefAction {
    fn as_str(self) -> &'static str {
        match self {
            RefAction::Create => "create",
            RefAction::Update => "update",
            RefAction::Delete => "delete",
            RefAction::ForceUpdate => "force",
        }
    }
}

/// A file added or modified by a push, for push-rule checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedFile {
    /// Repository path.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// One push-rule violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushViolation {
    /// Stable id: `push:deny_path:<path>`, `push:extension:<path>`,
    /// `push:max_file_size:<path>` or `push:max_path_length:<path>`.
    pub id: String,
    /// Explanation.
    pub detail: String,
}

/// The effective policy of one repo: org defaults overlaid by `.beehive/policy.toml`, validated,
/// with roles resolved and the epoch computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub(crate) doc: PolicyDoc,
    pub(crate) role_actions: BTreeMap<String, BTreeSet<Action>>,
    pub(crate) codeowners: Option<Codeowners>,
    epoch: u64,
}

impl Policy {
    /// Parses and validates a single repo policy with no org defaults.
    pub fn parse(source: &str) -> Result<Policy, PolicyError> {
        Policy::from_sources(None, Some(source))
    }

    /// Parses org defaults and a repo file, overlays them and validates the result.
    pub fn from_sources(org: Option<&str>, repo: Option<&str>) -> Result<Policy, PolicyError> {
        let org = org.map(|s| parse_doc(s, Origin::Org)).transpose()?;
        let repo = repo.map(|s| parse_doc(s, Origin::Repo)).transpose()?;
        Policy::from_docs(org, repo)
    }

    /// Overlays parsed documents and validates the result.
    pub fn from_docs(
        org: Option<PolicyDoc>,
        repo: Option<PolicyDoc>,
    ) -> Result<Policy, PolicyError> {
        let doc = match (org, repo) {
            (Some(o), Some(r)) => overlay(o, r)?,
            (Some(d), None) | (None, Some(d)) => d,
            (None, None) => PolicyDoc::default(),
        };
        validate(&doc)?;
        let role_actions = resolve_roles(&doc)?;
        let mut p = Policy {
            doc,
            role_actions,
            codeowners: None,
            epoch: 0,
        };
        p.epoch = canon::epoch_of(&p);
        Ok(p)
    }

    /// Attaches the parsed `CODEOWNERS` file named by the policy's `codeowners` key (read by the
    /// caller from the same tip). It is consulted for paths no `[[owners]]` rule covers, and it
    /// is part of the epoch.
    pub fn with_codeowners(mut self, codeowners: Codeowners) -> Policy {
        self.codeowners = Some(codeowners);
        self.epoch = canon::epoch_of(&self);
        self
    }

    /// Whether some ruleset is limited by repo custom properties.
    pub fn has_property_conditions(&self) -> bool {
        self.doc
            .rulesets
            .values()
            .any(|r| !r.repo_properties.is_empty())
    }

    /// The policy for a repo with these custom properties (name to values): rulesets whose
    /// `repo_properties` the repo does not match are dropped, and the epoch is recomputed.
    pub fn for_repo_properties(&self, properties: &BTreeMap<String, Vec<String>>) -> Policy {
        let mut p = self.clone();
        p.doc
            .rulesets
            .retain(|_, r| r.covers_properties(properties));
        p.epoch = canon::epoch_of(&p);
        p
    }

    /// The normalized document.
    pub fn doc(&self) -> &PolicyDoc {
        &self.doc
    }

    /// The policy epoch: the first 8 bytes of BLAKE3 over the canonical form, big-endian.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The canonical bytes the epoch is computed from.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canon::canonical_bytes(self)
    }

    /// Every group the actor belongs to: its external groups, `everyone`, the built-in group
    /// of its kind, and every policy group that includes it directly or through nesting.
    pub fn groups_of(&self, actor: &Actor) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = actor
            .groups
            .iter()
            .map(|g| g.trim_start_matches('@').to_string())
            .collect();
        set.insert("everyone".to_string());
        set.insert(actor.kind.builtin_group().to_string());
        loop {
            let mut changed = false;
            for (name, def) in &self.doc.groups {
                if set.contains(name) {
                    continue;
                }
                let member = def.members.iter().any(|m| match m {
                    Subject::Principal(p) => *p == actor.id,
                    Subject::Group(g) => set.contains(g),
                });
                if member {
                    set.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                return set;
            }
        }
    }

    pub(crate) fn matches_any(
        actor: &Actor,
        groups: &BTreeSet<String>,
        subjects: &BTreeSet<Subject>,
    ) -> bool {
        subjects.iter().any(|s| match s {
            Subject::Principal(p) => *p == actor.id,
            Subject::Group(g) => groups.contains(g),
        })
    }

    /// Whether `actor` is named by `subjects` (directly or through a group).
    pub fn is_member(&self, actor: &Actor, subjects: &BTreeSet<Subject>) -> bool {
        Self::matches_any(actor, &self.groups_of(actor), subjects)
    }

    /// The actions the actor's roles grant.
    pub fn actions_of(&self, actor: &Actor) -> BTreeSet<Action> {
        let groups = self.groups_of(actor);
        self.actions_with(actor, &groups)
    }

    pub(crate) fn actions_with(
        &self,
        actor: &Actor,
        groups: &BTreeSet<String>,
    ) -> BTreeSet<Action> {
        let mut out = BTreeSet::new();
        for (role, grant) in &self.doc.members {
            if Self::matches_any(actor, groups, &grant.subjects)
                && let Some(actions) = self.role_actions.get(role)
            {
                out.extend(actions.iter().copied());
            }
        }
        out
    }

    fn access_rule(&self, path: &str, write: bool) -> (&String, &BTreeSet<Subject>) {
        fn pick(r: &AccessRule, write: bool) -> Option<&BTreeSet<Subject>> {
            if write {
                r.write.as_ref()
            } else {
                r.read.as_ref()
            }
        }
        self.doc
            .access
            .iter()
            .filter(|(prefix, rule)| pick(rule, write).is_some() && prefix_covers(prefix, path))
            .max_by_key(|(prefix, _)| prefix_depth(prefix))
            .and_then(|(prefix, rule)| pick(rule, write).map(|s| (prefix, s)))
            .unwrap_or_else(unreachable_root)
    }

    /// Checks one actor, one action and one path.
    ///
    /// The actor's roles must grant the action; then, for path-scoped actions, the longest
    /// component-wise prefix rule that sets `read` (for `read`, `review`) or `write` (for
    /// `commit`, `lease`, `land`) must name the actor. Repository-wide actions ignore `path`.
    pub fn check_access(&self, actor: &Actor, action: Action, path: &str) -> Decision {
        let groups = self.groups_of(actor);
        self.check_access_with(actor, &groups, action, path)
    }

    pub(crate) fn check_access_with(
        &self,
        actor: &Actor,
        groups: &BTreeSet<String>,
        action: Action,
        path: &str,
    ) -> Decision {
        if !self.actions_with(actor, groups).contains(&action) {
            return Decision::deny(
                "members".to_string(),
                format!("{} holds no role granting `{action}`", actor.id),
            );
        }
        let write = match action.path_mode() {
            PathMode::None => {
                return Decision::allow("members".to_string(), format!("role grants `{action}`"));
            }
            PathMode::Read => false,
            PathMode::Write => true,
        };
        let path = normalize_request_path(path);
        let (prefix, subjects) = self.access_rule(path, write);
        let mode = if write { "write" } else { "read" };
        let rule = format!("access[{}].{mode}", display_prefix(prefix));
        if Self::matches_any(actor, groups, subjects) {
            Decision::allow(rule, format!("{} may {mode} /{path}", actor.id))
        } else {
            Decision::deny(rule, format!("{} may not {mode} /{path}", actor.id))
        }
    }

    /// Checks a delegated call: the acting principal first, then each delegator. Authority is
    /// the intersection, so every principal in the chain must be allowed. Returns the first
    /// denial, or the acting principal's decision.
    pub fn check_access_chain(&self, chain: &[Actor], action: Action, path: &str) -> Decision {
        let mut first = None;
        for actor in chain {
            let d = self.check_access(actor, action, path);
            if !d.allowed {
                return d;
            }
            first.get_or_insert(d);
        }
        first.unwrap_or_else(|| Decision::deny("chain".into(), "empty principal chain".into()))
    }

    /// [`Policy::check_access_chain`]'s verdict for one chain and action over many paths:
    /// each actor's groups and roles are resolved once, so a path costs only its prefix-rule
    /// lookup. `allows(path)` equals `check_access_chain(chain, action, path).allowed`.
    pub fn access_filter<'a>(&'a self, chain: &'a [Actor], action: Action) -> AccessFilter<'a> {
        let mut granted = !chain.is_empty();
        let actors = chain
            .iter()
            .map(|actor| {
                let groups = self.groups_of(actor);
                granted &= self.actions_with(actor, &groups).contains(&action);
                (actor, groups)
            })
            .collect();
        AccessFilter {
            policy: self,
            actors,
            granted,
            mode: action.path_mode(),
        }
    }

    /// Whether a ref is protected: a land rule or an active ruleset matches it. Protected refs
    /// move only through Land.
    pub fn is_protected(&self, ref_name: &str) -> bool {
        self.doc.land.keys().any(|p| p.matches(ref_name))
            || self.doc.rulesets.values().any(|r| {
                r.mode == RulesetMode::Active && r.refs.iter().any(|p| p.matches(ref_name))
            })
    }

    /// Checks a ref operation for one actor.
    ///
    /// `ws/<id>/…` refs belong to principal `<id>`, who may do anything to them. Direct updates
    /// of protected refs are denied. Otherwise the most specific `[[refs]]` rule (exact beats
    /// pattern, longest pattern wins) decides when it sets a list for the operation; with no
    /// list, the actor needs `manage_refs` (`force_update` for force).
    pub fn check_ref(&self, actor: &Actor, action: RefAction, ref_name: &str) -> Decision {
        if is_own_workspace(actor, ref_name) {
            return Decision::allow(
                "workspace".into(),
                format!("{ref_name} is {}'s workspace", actor.id),
            );
        }
        if action == RefAction::Update && self.is_protected(ref_name) {
            return Decision::deny(
                "protected".into(),
                format!("{ref_name} is protected and moves only through Land"),
            );
        }
        let groups = self.groups_of(actor);
        if let Some((pattern, rule)) = most_specific(self.doc.refs.iter(), |(p, _)| *p, ref_name) {
            let list = match action {
                RefAction::Create => &rule.create,
                RefAction::Update => &rule.update,
                RefAction::Delete => &rule.delete,
                RefAction::ForceUpdate => &rule.force,
            };
            if let Some(list) = list {
                let label = format!("refs[{pattern}].{}", action.as_str());
                return if Self::matches_any(actor, &groups, list) {
                    Decision::allow(
                        label,
                        format!("{} may {} {ref_name}", actor.id, action.as_str()),
                    )
                } else {
                    Decision::deny(
                        label,
                        format!("{} may not {} {ref_name}", actor.id, action.as_str()),
                    )
                };
            }
        }
        let needed = if action == RefAction::ForceUpdate {
            Action::ForceUpdate
        } else {
            Action::ManageRefs
        };
        if self.actions_with(actor, &groups).contains(&needed) {
            Decision::allow("members".into(), format!("role grants `{needed}`"))
        } else {
            Decision::deny(
                "members".into(),
                format!("{} holds no role granting `{needed}`", actor.id),
            )
        }
    }

    /// Checks a ref operation for a delegation chain. A workspace ref of any principal in the
    /// chain is allowed (a principal's workspaces are writable by those it delegated to);
    /// otherwise every principal must be allowed.
    pub fn check_ref_chain(&self, chain: &[Actor], action: RefAction, ref_name: &str) -> Decision {
        if let Some(owner) = chain.iter().find(|a| is_own_workspace(a, ref_name)) {
            return Decision::allow(
                "workspace".into(),
                format!("{ref_name} is {}'s workspace", owner.id),
            );
        }
        let mut first = None;
        for actor in chain {
            let d = self.check_ref(actor, action, ref_name);
            if !d.allowed {
                return d;
            }
            first.get_or_insert(d);
        }
        first.unwrap_or_else(|| Decision::deny("chain".into(), "empty principal chain".into()))
    }

    /// The owners of a path and the rule that names them: the longest `[[owners]]` prefix, else
    /// the last matching `CODEOWNERS` line. `None` when the path is unowned.
    pub fn owners_of(&self, path: &str) -> Option<(String, BTreeSet<OwnerRef>)> {
        let path = normalize_request_path(path);
        if let Some((prefix, rule)) = self
            .doc
            .owners
            .iter()
            .filter(|(prefix, _)| prefix_covers(prefix, path))
            .max_by_key(|(prefix, _)| prefix_depth(prefix))
        {
            return if rule.owners.is_empty() {
                None
            } else {
                Some((
                    format!("owners[{}]", display_prefix(prefix)),
                    rule.owners.clone(),
                ))
            };
        }
        let entry = self.codeowners.as_ref()?.lookup(path)?;
        if entry.owners.is_empty() {
            None
        } else {
            Some((format!("CODEOWNERS:{}", entry.line), entry.owners.clone()))
        }
    }

    /// Whether an actor is one of `owners`.
    pub fn is_owner(&self, actor: &Actor, owners: &BTreeSet<OwnerRef>) -> bool {
        let groups = self.groups_of(actor);
        owner_matches(actor, &groups, owners)
    }

    /// Whether policy marks the path sensitive.
    pub fn is_sensitive(&self, path: &str) -> bool {
        self.doc.paths.sensitive.iter().any(|g| g.matches(path))
    }

    /// Whether policy marks the path generated.
    pub fn is_generated(&self, path: &str) -> bool {
        self.doc.paths.generated.iter().any(|g| g.matches(path))
    }

    /// Whether landing a change to the path needs an exclusive lease.
    pub fn requires_exclusive_lease(&self, path: &str) -> bool {
        self.doc
            .paths
            .exclusive_lease
            .iter()
            .any(|g| g.matches(path))
    }

    /// Checks files against the top-level push rules.
    pub fn check_push(&self, files: &[PushedFile]) -> Vec<PushViolation> {
        push_violations(&self.doc.push, files)
    }
}

pub(crate) fn owner_matches(
    actor: &Actor,
    groups: &BTreeSet<String>,
    owners: &BTreeSet<OwnerRef>,
) -> bool {
    owners.iter().any(|o| match o {
        OwnerRef::Principal(p) => *p == actor.id,
        OwnerRef::Group(g) => groups.contains(g),
        OwnerRef::Login(l) | OwnerRef::Email(l) => actor.aliases.iter().any(|a| a == l),
    })
}

fn is_own_workspace(actor: &Actor, ref_name: &str) -> bool {
    ref_name
        .strip_prefix("ws/")
        .and_then(|rest| rest.strip_prefix(actor.id.as_str()))
        .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
}

fn unreachable_root<'a>() -> (&'a String, &'a BTreeSet<Subject>) {
    // Validation guarantees a `/` rule defining both read and write, and every path is under
    // `/`. Should that invariant ever break, deny everything rather than panic.
    static EMPTY_PREFIX: String = String::new();
    static EMPTY: BTreeSet<Subject> = BTreeSet::new();
    (&EMPTY_PREFIX, &EMPTY)
}

/// Checks files against a set of push rules.
pub fn push_violations(rules: &PushRules, files: &[PushedFile]) -> Vec<PushViolation> {
    let mut out = Vec::new();
    for f in files {
        let path = normalize_request_path(&f.path);
        if let Some(g) = rules.deny_paths.iter().find(|g| g.matches(path)) {
            out.push(PushViolation {
                id: format!("push:deny_path:{path}"),
                detail: format!("/{path} matches denied path {}", g.as_str()),
            });
        }
        let file_name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        if let Some(ext) = rules
            .deny_extensions
            .iter()
            .find(|ext| file_name.ends_with(&format!(".{ext}")))
        {
            out.push(PushViolation {
                id: format!("push:extension:{path}"),
                detail: format!("/{path} has denied extension .{ext}"),
            });
        }
        if let Some(max) = rules.max_file_size.filter(|max| f.size > *max) {
            out.push(PushViolation {
                id: format!("push:max_file_size:{path}"),
                detail: format!("/{path} is {} bytes; the limit is {max}", f.size),
            });
        }
        let len = path.len() as u64;
        if let Some(max) = rules.max_path_length.filter(|max| len > *max) {
            out.push(PushViolation {
                id: format!("push:max_path_length:{path}"),
                detail: format!("path is {len} bytes; the limit is {max}"),
            });
        }
    }
    out
}

/// Overlays a repo document on org defaults.
///
/// - Groups, custom roles, `[members]` roles, ref rules, land rules and owner rules: the repo
///   entry with the same key replaces the org entry.
/// - Access rules: keyed by path; for a path in both, `read` and `write` are overridden
///   separately, so a repo rule that sets only `write` keeps the org's `read`.
/// - Path sets (`sensitive`, `generated`, `exclusive_lease`) and push deny lists: union.
/// - Push size limits and `codeowners`: the repo value replaces the org value when set.
/// - Rulesets: union; org rulesets cannot be replaced or removed by a repo, and a repo ruleset
///   with the same name as an org ruleset is an error.
pub fn overlay(org: PolicyDoc, repo: PolicyDoc) -> Result<PolicyDoc, PolicyError> {
    let mut out = org;
    if repo.codeowners.is_some() {
        out.codeowners = repo.codeowners;
    }
    out.groups.extend(repo.groups);
    out.roles.extend(repo.roles);
    out.members.extend(repo.members);
    for (prefix, rule) in repo.access {
        match out.access.get_mut(&prefix) {
            Some(existing) => {
                if rule.read.is_some() {
                    existing.read = rule.read;
                }
                if rule.write.is_some() {
                    existing.write = rule.write;
                }
                existing.loc = rule.loc;
            }
            None => {
                out.access.insert(prefix, rule);
            }
        }
    }
    out.refs.extend(repo.refs);
    out.land.extend(repo.land);
    out.owners.extend(repo.owners);
    out.push.deny_paths.extend(repo.push.deny_paths);
    out.push.deny_extensions.extend(repo.push.deny_extensions);
    if repo.push.max_file_size.is_some() {
        out.push.max_file_size = repo.push.max_file_size;
    }
    if repo.push.max_path_length.is_some() {
        out.push.max_path_length = repo.push.max_path_length;
    }
    out.paths.sensitive.extend(repo.paths.sensitive);
    out.paths.generated.extend(repo.paths.generated);
    out.paths.exclusive_lease.extend(repo.paths.exclusive_lease);
    for (name, rs) in repo.rulesets {
        if out.rulesets.contains_key(&name) {
            return Err(PolicyError::new(
                PolicyErrorKind::Duplicate,
                format!("ruleset `{name}` is defined by the org and cannot be redefined"),
                rs.loc,
            ));
        }
        out.rulesets.insert(name, rs);
    }
    Ok(out)
}

fn validate(doc: &PolicyDoc) -> Result<(), PolicyError> {
    match doc.access.get("") {
        Some(r) if r.read.is_some() && r.write.is_some() => {}
        Some(r) => {
            return Err(PolicyError::new(
                PolicyErrorKind::MissingRootRule,
                "the access rule for `/` must set both `read` and `write`",
                r.loc.clone(),
            ));
        }
        None => {
            return Err(PolicyError::new(
                PolicyErrorKind::MissingRootRule,
                "policy needs an access rule for `/` setting both `read` and `write`",
                Location::key(Origin::Effective, "access"),
            ));
        }
    }

    // Every `@group` reference must be built in, external (contains `/`) or defined.
    let check = |s: &Subject, loc: &Location| -> Result<(), PolicyError> {
        if let Subject::Group(g) = s
            && !g.contains('/')
            && !BUILTIN_GROUPS.contains(&g.as_str())
            && !doc.groups.contains_key(g)
        {
            return Err(PolicyError::new(
                PolicyErrorKind::UnknownGroup,
                format!("group `@{g}` is not defined"),
                loc.clone(),
            ));
        }
        Ok(())
    };
    let check_set =
        |set: &BTreeSet<Subject>, loc: &Location| set.iter().try_for_each(|s| check(s, loc));
    let check_opt = |set: &Option<BTreeSet<Subject>>, loc: &Location| match set {
        Some(s) => check_set(s, loc),
        None => Ok(()),
    };
    let check_body = |b: &crate::model::RuleBody, loc: &Location| -> Result<(), PolicyError> {
        check_opt(&b.allow, loc)?;
        for a in &b.approvals {
            if let crate::model::ApproverSet::Subjects(s) = &a.from {
                check_set(s, loc)?;
            }
        }
        Ok(())
    };
    for g in doc.groups.values() {
        check_set(&g.members, &g.loc)?;
    }
    for m in doc.members.values() {
        check_set(&m.subjects, &m.loc)?;
    }
    for r in doc.access.values() {
        check_opt(&r.read, &r.loc)?;
        check_opt(&r.write, &r.loc)?;
    }
    for r in doc.refs.values() {
        for l in [&r.create, &r.update, &r.delete, &r.force] {
            check_opt(l, &r.loc)?;
        }
    }
    for r in doc.land.values() {
        check_body(&r.body, &r.loc)?;
    }
    for r in doc.owners.values() {
        for o in &r.owners {
            if let OwnerRef::Group(g) = o {
                check(&Subject::Group(g.clone()), &r.loc)?;
            }
        }
    }
    for r in doc.rulesets.values() {
        check_set(&r.bypass, &r.loc)?;
        check_body(&r.body, &r.loc)?;
    }

    // Group cycles.
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = visiting, 2 = done
    fn visit<'a>(
        doc: &'a PolicyDoc,
        name: &'a str,
        state: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Result<(), PolicyError> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(_) => {
                let start = stack.iter().position(|s| *s == name).unwrap_or(0);
                let mut cycle: Vec<String> =
                    stack[start..].iter().map(|s| format!("@{s}")).collect();
                cycle.push(format!("@{name}"));
                let loc = doc.groups.get(name).map(|g| g.loc.clone());
                return Err(PolicyError::new(
                    PolicyErrorKind::GroupCycle,
                    format!("groups form a cycle: {}", cycle.join(" -> ")),
                    loc.unwrap_or_else(|| Location::key(Origin::Effective, "groups")),
                ));
            }
            None => {}
        }
        state.insert(name, 1);
        stack.push(name);
        if let Some(def) = doc.groups.get(name) {
            for m in &def.members {
                if let Subject::Group(g) = m
                    && doc.groups.contains_key(g)
                {
                    visit(doc, g, state, stack)?;
                }
            }
        }
        stack.pop();
        state.insert(name, 2);
        Ok(())
    }
    for name in doc.groups.keys() {
        visit(doc, name, &mut state, &mut Vec::new())?;
    }

    for (role, grant) in &doc.members {
        if builtin_role_actions(role).is_none() && !doc.roles.contains_key(role) {
            return Err(PolicyError::new(
                PolicyErrorKind::UnknownRole,
                format!("role `{role}` is not built in and not defined in [roles]"),
                grant.loc.clone(),
            ));
        }
    }
    Ok(())
}

fn resolve_roles(doc: &PolicyDoc) -> Result<BTreeMap<String, BTreeSet<Action>>, PolicyError> {
    let mut out: BTreeMap<String, BTreeSet<Action>> = BTreeMap::new();
    for name in crate::action::BUILTIN_ROLES {
        out.insert(
            name.to_string(),
            builtin_role_actions(name).unwrap_or_default(),
        );
    }
    for (name, def) in &doc.roles {
        let mut actions = def.actions.clone();
        let mut seen = vec![name.as_str()];
        let mut base = def.base.as_deref();
        while let Some(b) = base {
            if let Some(builtin) = builtin_role_actions(b) {
                actions.extend(builtin);
                break;
            }
            let Some(next) = doc.roles.get(b) else {
                return Err(PolicyError::new(
                    PolicyErrorKind::UnknownRole,
                    format!("role `{name}` extends unknown role `{b}`"),
                    def.loc.clone(),
                ));
            };
            if seen.contains(&b) {
                seen.push(b);
                return Err(PolicyError::new(
                    PolicyErrorKind::RoleCycle,
                    format!("roles form a cycle: {}", seen.join(" -> ")),
                    def.loc.clone(),
                ));
            }
            seen.push(b);
            actions.extend(next.actions.iter().copied());
            base = next.base.as_deref();
        }
        out.insert(name.clone(), actions);
    }
    Ok(out)
}

/// Read access for many paths at once (see [`Policy::access_filter`]).
pub struct AccessFilter<'a> {
    policy: &'a Policy,
    actors: Vec<(&'a Actor, BTreeSet<String>)>,
    granted: bool,
    mode: PathMode,
}

impl AccessFilter<'_> {
    /// Whether every principal in the chain may perform the action on `path`.
    pub fn allows(&self, path: &str) -> bool {
        if !self.granted {
            return false;
        }
        let write = match self.mode {
            PathMode::None => return true,
            PathMode::Read => false,
            PathMode::Write => true,
        };
        let (_, subjects) = self.policy.access_rule(normalize_request_path(path), write);
        self.actors
            .iter()
            .all(|(actor, groups)| Policy::matches_any(actor, groups, subjects))
    }
}
