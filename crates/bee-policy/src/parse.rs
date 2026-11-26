//! Parsing `.beehive/policy.toml` (and org defaults, which use the same format) into a
//! normalized [`PolicyDoc`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use toml::Spanned;

use crate::action::{Action, BUILTIN_ROLES, DiffClass};
use crate::error::{Location, Origin, PolicyError, PolicyErrorKind};
use crate::model::{
    AccessRule, AlertThreshold, ApprovalRule, ApproverSet, CodeScanningRule, GroupDef, LandRule,
    MemberGrant, OwnerRule, PathSets, PolicyDoc, PushRules, RefRule, RoleDef, RuleBody, Ruleset,
    RulesetMode, SecurityThreshold,
};
use crate::owners::OwnerRef;
use crate::path::{Glob, normalize_prefix};
use crate::pattern::RefPattern;
use crate::subject::{BUILTIN_GROUPS, Subject};

type SList = Vec<Spanned<String>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: Option<Spanned<i64>>,
    codeowners: Option<Spanned<String>>,
    #[serde(default)]
    groups: BTreeMap<String, Spanned<SList>>,
    #[serde(default)]
    roles: BTreeMap<String, Spanned<RawRole>>,
    #[serde(default)]
    members: BTreeMap<String, Spanned<SList>>,
    #[serde(default)]
    access: Vec<Spanned<RawAccess>>,
    #[serde(default)]
    refs: Vec<Spanned<RawRefRule>>,
    #[serde(default)]
    land: Vec<Spanned<RawLand>>,
    #[serde(default)]
    owners: Vec<Spanned<RawOwners>>,
    push: Option<Spanned<RawPush>>,
    paths: Option<Spanned<RawPaths>>,
    #[serde(default)]
    rulesets: Vec<Spanned<RawRuleset>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    base: Option<Spanned<String>>,
    #[serde(default)]
    actions: SList,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccess {
    path: Spanned<String>,
    read: Option<SList>,
    write: Option<SList>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRefRule {
    pattern: Spanned<String>,
    create: Option<SList>,
    update: Option<SList>,
    delete: Option<SList>,
    force: Option<SList>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApproval {
    #[serde(default)]
    classes: SList,
    #[serde(default)]
    paths: SList,
    count: Option<Spanned<i64>>,
    from: Option<Spanned<RawFrom>>,
    #[serde(default)]
    include_agents: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawFrom {
    Keyword(String),
    Subjects(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPush {
    #[serde(default)]
    deny_paths: SList,
    #[serde(default)]
    deny_extensions: SList,
    max_file_size: Option<Spanned<i64>>,
    max_path_length: Option<Spanned<i64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPaths {
    #[serde(default)]
    sensitive: SList,
    #[serde(default)]
    generated: SList,
    #[serde(default)]
    exclusive_lease: SList,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodeScanning {
    tool: Spanned<String>,
    alerts: Option<Spanned<String>>,
    security_alerts: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOwners {
    path: Spanned<String>,
    owners: SList,
}

/// Fields shared by `[[land]]` and `[[rulesets]]`.
#[derive(Default)]
struct RawBody {
    allow: Option<SList>,
    required_checks: SList,
    approvals: Vec<Spanned<RawApproval>>,
    agents_may_land_without_human: SList,
    require_signed: bool,
    require_linear: bool,
    dismiss_stale_approvals: bool,
    require_last_push_approval: bool,
    push: Option<Spanned<RawPush>>,
    code_scanning: Vec<Spanned<RawCodeScanning>>,
    require_agent_review: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLand {
    #[serde(rename = "ref")]
    ref_pattern: Spanned<String>,
    allow: Option<SList>,
    #[serde(default)]
    required_checks: SList,
    #[serde(default)]
    approvals: Vec<Spanned<RawApproval>>,
    #[serde(default)]
    agents_may_land_without_human: SList,
    #[serde(default)]
    require_signed: bool,
    #[serde(default)]
    require_linear: bool,
    #[serde(default)]
    dismiss_stale_approvals: bool,
    #[serde(default)]
    require_last_push_approval: bool,
    push: Option<Spanned<RawPush>>,
    #[serde(default)]
    code_scanning: Vec<Spanned<RawCodeScanning>>,
    #[serde(default)]
    require_agent_review: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuleset {
    name: Spanned<String>,
    mode: Option<Spanned<String>>,
    refs: SList,
    #[serde(default)]
    bypass: SList,
    #[serde(default)]
    repo_properties: BTreeMap<String, SList>,
    allow: Option<SList>,
    #[serde(default)]
    required_checks: SList,
    #[serde(default)]
    approvals: Vec<Spanned<RawApproval>>,
    #[serde(default)]
    agents_may_land_without_human: SList,
    #[serde(default)]
    require_signed: bool,
    #[serde(default)]
    require_linear: bool,
    #[serde(default)]
    dismiss_stale_approvals: bool,
    #[serde(default)]
    require_last_push_approval: bool,
    push: Option<Spanned<RawPush>>,
    #[serde(default)]
    code_scanning: Vec<Spanned<RawCodeScanning>>,
    #[serde(default)]
    require_agent_review: bool,
}

struct Ctx<'a> {
    origin: Origin,
    source: &'a str,
}

impl Ctx<'_> {
    fn at<T>(&self, key: &str, v: &Spanned<T>) -> Location {
        Location::span(self.origin, key, self.source, v.span().start)
    }

    fn err<T>(&self, kind: PolicyErrorKind, key: &str, v: &Spanned<T>, msg: String) -> PolicyError {
        PolicyError::new(kind, msg, self.at(key, v))
    }

    fn subjects(&self, key: &str, list: &SList) -> Result<BTreeSet<Subject>, PolicyError> {
        list.iter()
            .enumerate()
            .map(|(i, s)| {
                Subject::parse(s.get_ref()).map_err(|m| {
                    self.err(
                        PolicyErrorKind::InvalidSubject,
                        &format!("{key}[{i}]"),
                        s,
                        m,
                    )
                })
            })
            .collect()
    }

    fn opt_subjects(
        &self,
        key: &str,
        list: &Option<SList>,
    ) -> Result<Option<BTreeSet<Subject>>, PolicyError> {
        list.as_ref().map(|l| self.subjects(key, l)).transpose()
    }

    fn prefix(&self, key: &str, v: &Spanned<String>) -> Result<String, PolicyError> {
        normalize_prefix(v.get_ref()).map_err(|m| self.err(PolicyErrorKind::InvalidPath, key, v, m))
    }

    fn globs(&self, key: &str, list: &SList) -> Result<BTreeSet<Glob>, PolicyError> {
        list.iter()
            .enumerate()
            .map(|(i, g)| {
                Glob::parse(g.get_ref()).map_err(|m| {
                    self.err(
                        PolicyErrorKind::InvalidPattern,
                        &format!("{key}[{i}]"),
                        g,
                        m,
                    )
                })
            })
            .collect()
    }

    fn ref_pattern(&self, key: &str, v: &Spanned<String>) -> Result<RefPattern, PolicyError> {
        RefPattern::parse(v.get_ref())
            .map_err(|m| self.err(PolicyErrorKind::InvalidPattern, key, v, m))
    }

    fn classes(&self, key: &str, list: &SList) -> Result<BTreeSet<DiffClass>, PolicyError> {
        list.iter()
            .enumerate()
            .map(|(i, c)| {
                DiffClass::parse(c.get_ref()).ok_or_else(|| {
                    self.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{key}[{i}]"),
                        c,
                        format!("unknown diff class `{}`", c.get_ref()),
                    )
                })
            })
            .collect()
    }

    fn non_negative(
        &self,
        key: &str,
        v: &Option<Spanned<i64>>,
    ) -> Result<Option<u64>, PolicyError> {
        match v {
            None => Ok(None),
            Some(n) => u64::try_from(*n.get_ref()).map(Some).map_err(|_| {
                self.err(
                    PolicyErrorKind::InvalidValue,
                    key,
                    n,
                    "must not be negative".into(),
                )
            }),
        }
    }

    fn push(&self, key: &str, raw: &Option<Spanned<RawPush>>) -> Result<PushRules, PolicyError> {
        let Some(raw) = raw else {
            return Ok(PushRules::default());
        };
        let p = raw.get_ref();
        let mut deny_extensions = BTreeSet::new();
        for (i, e) in p.deny_extensions.iter().enumerate() {
            let ext = e.get_ref().trim_start_matches('.').to_ascii_lowercase();
            if ext.is_empty() || ext.contains('/') {
                return Err(self.err(
                    PolicyErrorKind::InvalidValue,
                    &format!("{key}.deny_extensions[{i}]"),
                    e,
                    format!("invalid extension `{}`", e.get_ref()),
                ));
            }
            deny_extensions.insert(ext);
        }
        Ok(PushRules {
            deny_paths: self.globs(&format!("{key}.deny_paths"), &p.deny_paths)?,
            deny_extensions,
            max_file_size: self.non_negative(&format!("{key}.max_file_size"), &p.max_file_size)?,
            max_path_length: self
                .non_negative(&format!("{key}.max_path_length"), &p.max_path_length)?,
        })
    }

    fn approval(&self, key: &str, raw: &Spanned<RawApproval>) -> Result<ApprovalRule, PolicyError> {
        let a = raw.get_ref();
        let count = match &a.count {
            None => 1,
            Some(c) => match u32::try_from(*c.get_ref()) {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Err(self.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{key}.count"),
                        c,
                        "count must be at least 1".into(),
                    ));
                }
            },
        };
        let from = match &a.from {
            None => ApproverSet::Any,
            Some(f) => {
                match f.get_ref() {
                    RawFrom::Keyword(k) if k == "owners" => ApproverSet::Owners,
                    RawFrom::Keyword(k) if k == "any" => ApproverSet::Any,
                    RawFrom::Keyword(k) => {
                        return Err(self.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{key}.from"),
                        f,
                        format!("`from` must be \"owners\", \"any\" or a list of subjects, not `{k}`"),
                    ));
                    }
                    RawFrom::Subjects(list) => {
                        let mut set = BTreeSet::new();
                        for s in list {
                            set.insert(Subject::parse(s).map_err(|m| {
                                self.err(
                                    PolicyErrorKind::InvalidSubject,
                                    &format!("{key}.from"),
                                    f,
                                    m,
                                )
                            })?);
                        }
                        if set.is_empty() {
                            return Err(self.err(
                                PolicyErrorKind::InvalidValue,
                                &format!("{key}.from"),
                                f,
                                "`from` lists no subjects".into(),
                            ));
                        }
                        ApproverSet::Subjects(set)
                    }
                }
            }
        };
        let mut paths = BTreeSet::new();
        for (i, p) in a.paths.iter().enumerate() {
            paths.insert(self.prefix(&format!("{key}.paths[{i}]"), p)?);
        }
        Ok(ApprovalRule {
            classes: self.classes(&format!("{key}.classes"), &a.classes)?,
            paths,
            count,
            from,
            include_agents: a.include_agents,
        })
    }

    fn body(&self, key: &str, raw: RawBody) -> Result<RuleBody, PolicyError> {
        let mut checks = BTreeSet::new();
        for (i, c) in raw.required_checks.iter().enumerate() {
            if c.get_ref().trim().is_empty() {
                return Err(self.err(
                    PolicyErrorKind::InvalidValue,
                    &format!("{key}.required_checks[{i}]"),
                    c,
                    "check name is empty".into(),
                ));
            }
            checks.insert(c.get_ref().clone());
        }
        let mut approvals = raw
            .approvals
            .iter()
            .enumerate()
            .map(|(i, a)| self.approval(&format!("{key}.approvals[{i}]"), a))
            .collect::<Result<Vec<_>, _>>()?;
        approvals.sort();
        approvals.dedup();
        Ok(RuleBody {
            allow: self.opt_subjects(&format!("{key}.allow"), &raw.allow)?,
            required_checks: checks,
            approvals,
            agents_may_land_without_human: self.classes(
                &format!("{key}.agents_may_land_without_human"),
                &raw.agents_may_land_without_human,
            )?,
            require_signed: raw.require_signed,
            require_linear: raw.require_linear,
            dismiss_stale_approvals: raw.dismiss_stale_approvals,
            require_last_push_approval: raw.require_last_push_approval,
            push: self.push(&format!("{key}.push"), &raw.push)?,
            code_scanning: self
                .code_scanning(&format!("{key}.code_scanning"), &raw.code_scanning)?,
            require_agent_review: raw.require_agent_review,
        })
    }

    fn code_scanning(
        &self,
        key: &str,
        raw: &[Spanned<RawCodeScanning>],
    ) -> Result<Vec<CodeScanningRule>, PolicyError> {
        let mut out: Vec<CodeScanningRule> = Vec::new();
        for (i, c) in raw.iter().enumerate() {
            let k = format!("{key}[{i}]");
            let r = c.get_ref();
            let tool = r.tool.get_ref().trim().to_string();
            if tool.is_empty() {
                return Err(self.err(
                    PolicyErrorKind::InvalidValue,
                    &format!("{k}.tool"),
                    &r.tool,
                    "tool name is empty".into(),
                ));
            }
            let alerts = match &r.alerts {
                None => AlertThreshold::Errors,
                Some(v) => AlertThreshold::parse(v.get_ref()).ok_or_else(|| {
                    self.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{k}.alerts"),
                        v,
                        format!(
                            "alerts must be none, errors, errors_and_warnings or all, not `{}`",
                            v.get_ref()
                        ),
                    )
                })?,
            };
            let security_alerts = match &r.security_alerts {
                None => SecurityThreshold::HighOrHigher,
                Some(v) => SecurityThreshold::parse(v.get_ref()).ok_or_else(|| {
                    self.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{k}.security_alerts"),
                        v,
                        format!(
                            "security_alerts must be none, critical, high_or_higher, \
                             medium_or_higher or all, not `{}`",
                            v.get_ref()
                        ),
                    )
                })?,
            };
            if out.iter().any(|o| o.tool.eq_ignore_ascii_case(&tool)) {
                return Err(duplicate(
                    self,
                    &k,
                    c,
                    format!("code_scanning tool `{tool}`"),
                ));
            }
            out.push(CodeScanningRule {
                tool,
                alerts,
                security_alerts,
            });
        }
        out.sort();
        Ok(out)
    }
}

fn duplicate<T>(ctx: &Ctx<'_>, key: &str, v: &Spanned<T>, what: String) -> PolicyError {
    ctx.err(
        PolicyErrorKind::Duplicate,
        key,
        v,
        format!("duplicate {what}"),
    )
}

/// Parses one policy document. Checks syntax, types, unknown keys, names, paths and patterns;
/// cross-references (groups, roles, the `/` rule) are checked when building a
/// [`crate::Policy`], because an org default and a repo file may each be incomplete alone.
pub fn parse_doc(source: &str, origin: Origin) -> Result<PolicyDoc, PolicyError> {
    let raw: RawFile = toml::from_str(source).map_err(|e| {
        let loc = match e.span() {
            Some(span) => Location::span(origin, "", source, span.start),
            None => Location::key(origin, ""),
        };
        PolicyError::new(PolicyErrorKind::Parse, e.message().trim().to_string(), loc)
    })?;
    let ctx = Ctx { origin, source };

    match &raw.version {
        Some(v) if *v.get_ref() == 1 => {}
        Some(v) => {
            return Err(ctx.err(
                PolicyErrorKind::UnsupportedVersion,
                "version",
                v,
                format!("unsupported policy version {}; expected 1", v.get_ref()),
            ));
        }
        None => {
            return Err(PolicyError::new(
                PolicyErrorKind::UnsupportedVersion,
                "missing `version = 1`",
                Location::key(origin, "version"),
            ));
        }
    }

    let mut doc = PolicyDoc::default();

    if let Some(c) = &raw.codeowners {
        let p = normalize_prefix(c.get_ref())
            .ok()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                ctx.err(
                    PolicyErrorKind::InvalidPath,
                    "codeowners",
                    c,
                    "codeowners must name a file".into(),
                )
            })?;
        doc.codeowners = Some(p);
    }

    for (name, members) in &raw.groups {
        let key = format!("groups.{name}");
        check_name(&ctx, &key, members, name)?;
        if BUILTIN_GROUPS.contains(&name.as_str()) {
            return Err(ctx.err(
                PolicyErrorKind::ReservedName,
                &key,
                members,
                format!("`{name}` is a built-in group"),
            ));
        }
        let set = ctx.subjects(&key, members.get_ref())?;
        doc.groups.insert(
            name.clone(),
            GroupDef {
                members: set,
                loc: ctx.at(&key, members),
            },
        );
    }

    for (name, role) in &raw.roles {
        let key = format!("roles.{name}");
        check_name(&ctx, &key, role, name)?;
        if BUILTIN_ROLES.contains(&name.as_str()) {
            return Err(ctx.err(
                PolicyErrorKind::ReservedName,
                &key,
                role,
                format!("`{name}` is a built-in role"),
            ));
        }
        let r = role.get_ref();
        let mut actions = BTreeSet::new();
        for (i, a) in r.actions.iter().enumerate() {
            actions.insert(Action::parse(a.get_ref()).ok_or_else(|| {
                ctx.err(
                    PolicyErrorKind::InvalidValue,
                    &format!("{key}.actions[{i}]"),
                    a,
                    format!("unknown action `{}`", a.get_ref()),
                )
            })?);
        }
        doc.roles.insert(
            name.clone(),
            RoleDef {
                base: r.base.as_ref().map(|b| b.get_ref().clone()),
                actions,
                loc: ctx.at(&key, role),
            },
        );
    }

    for (role, subjects) in &raw.members {
        let key = format!("members.{role}");
        let set = ctx.subjects(&key, subjects.get_ref())?;
        doc.members.insert(
            role.clone(),
            MemberGrant {
                subjects: set,
                loc: ctx.at(&key, subjects),
            },
        );
    }

    for (i, rule) in raw.access.iter().enumerate() {
        let key = format!("access[{i}]");
        let r = rule.get_ref();
        let prefix = ctx.prefix(&format!("{key}.path"), &r.path)?;
        if doc.access.contains_key(&prefix) {
            return Err(duplicate(
                &ctx,
                &key,
                rule,
                format!("access rule for `{}`", r.path.get_ref()),
            ));
        }
        let read = ctx.opt_subjects(&format!("{key}.read"), &r.read)?;
        let write = ctx.opt_subjects(&format!("{key}.write"), &r.write)?;
        doc.access.insert(
            prefix,
            AccessRule {
                read,
                write,
                loc: ctx.at(&key, rule),
            },
        );
    }

    for (i, rule) in raw.refs.iter().enumerate() {
        let key = format!("refs[{i}]");
        let r = rule.get_ref();
        let pattern = ctx.ref_pattern(&format!("{key}.pattern"), &r.pattern)?;
        if doc.refs.contains_key(&pattern) {
            return Err(duplicate(
                &ctx,
                &key,
                rule,
                format!("ref rule for `{pattern}`"),
            ));
        }
        let rr = RefRule {
            create: ctx.opt_subjects(&format!("{key}.create"), &r.create)?,
            update: ctx.opt_subjects(&format!("{key}.update"), &r.update)?,
            delete: ctx.opt_subjects(&format!("{key}.delete"), &r.delete)?,
            force: ctx.opt_subjects(&format!("{key}.force"), &r.force)?,
            loc: ctx.at(&key, rule),
        };
        doc.refs.insert(pattern, rr);
    }

    for (i, rule) in raw.land.into_iter().enumerate() {
        let key = format!("land[{i}]");
        let loc = ctx.at(&key, &rule);
        let span_holder = Spanned::new(rule.span(), ());
        let r = rule.into_inner();
        let pattern = ctx.ref_pattern(&format!("{key}.ref"), &r.ref_pattern)?;
        if doc.land.contains_key(&pattern) {
            return Err(duplicate(
                &ctx,
                &key,
                &span_holder,
                format!("land rule for `{pattern}`"),
            ));
        }
        let body = ctx.body(
            &key,
            RawBody {
                allow: r.allow,
                required_checks: r.required_checks,
                approvals: r.approvals,
                agents_may_land_without_human: r.agents_may_land_without_human,
                require_signed: r.require_signed,
                require_linear: r.require_linear,
                dismiss_stale_approvals: r.dismiss_stale_approvals,
                require_last_push_approval: r.require_last_push_approval,
                push: r.push,
                code_scanning: r.code_scanning,
                require_agent_review: r.require_agent_review,
            },
        )?;
        doc.land.insert(pattern, LandRule { body, loc });
    }

    for (i, rule) in raw.owners.iter().enumerate() {
        let key = format!("owners[{i}]");
        let r = rule.get_ref();
        let prefix = ctx.prefix(&format!("{key}.path"), &r.path)?;
        if doc.owners.contains_key(&prefix) {
            return Err(duplicate(
                &ctx,
                &key,
                rule,
                format!("owners rule for `{}`", r.path.get_ref()),
            ));
        }
        let owners = ctx
            .subjects(&format!("{key}.owners"), &r.owners)?
            .into_iter()
            .map(OwnerRef::from_subject)
            .collect();
        doc.owners.insert(
            prefix,
            OwnerRule {
                owners,
                loc: ctx.at(&key, rule),
            },
        );
    }

    doc.push = ctx.push("push", &raw.push)?;

    if let Some(p) = &raw.paths {
        let p = p.get_ref();
        doc.paths = PathSets {
            sensitive: ctx.globs("paths.sensitive", &p.sensitive)?,
            generated: ctx.globs("paths.generated", &p.generated)?,
            exclusive_lease: ctx.globs("paths.exclusive_lease", &p.exclusive_lease)?,
        };
    }

    for (i, rs) in raw.rulesets.into_iter().enumerate() {
        let key = format!("rulesets[{i}]");
        let loc = ctx.at(&key, &rs);
        let r = rs.into_inner();
        let name = r.name.get_ref().clone();
        check_name(&ctx, &format!("{key}.name"), &r.name, &name)?;
        if doc.rulesets.contains_key(&name) {
            return Err(duplicate(
                &ctx,
                &format!("{key}.name"),
                &r.name,
                format!("ruleset `{name}`"),
            ));
        }
        let mode = match &r.mode {
            None => RulesetMode::Active,
            Some(m) => match m.get_ref().as_str() {
                "active" => RulesetMode::Active,
                "evaluate" => RulesetMode::Evaluate,
                "disabled" => RulesetMode::Disabled,
                other => {
                    return Err(ctx.err(
                        PolicyErrorKind::InvalidValue,
                        &format!("{key}.mode"),
                        m,
                        format!("mode must be active, evaluate or disabled, not `{other}`"),
                    ));
                }
            },
        };
        let mut refs = BTreeSet::new();
        for (j, p) in r.refs.iter().enumerate() {
            refs.insert(ctx.ref_pattern(&format!("{key}.refs[{j}]"), p)?);
        }
        if refs.is_empty() {
            return Err(ctx.err(
                PolicyErrorKind::InvalidValue,
                &format!("{key}.refs"),
                &r.name,
                "a ruleset must name at least one ref pattern".into(),
            ));
        }
        let bypass = ctx.subjects(&format!("{key}.bypass"), &r.bypass)?;
        let mut repo_properties = BTreeMap::new();
        for (prop, values) in &r.repo_properties {
            let pkey = format!("{key}.repo_properties.{prop}");
            let set: BTreeSet<String> = values
                .iter()
                .map(|v| v.get_ref().trim().to_lowercase())
                .filter(|v| !v.is_empty())
                .collect();
            if prop.trim().is_empty() || set.is_empty() {
                return Err(ctx.err(
                    PolicyErrorKind::InvalidValue,
                    &pkey,
                    &r.name,
                    "a repo property condition needs a name and at least one value".into(),
                ));
            }
            repo_properties.insert(prop.trim().to_lowercase(), set);
        }
        let body = ctx.body(
            &key,
            RawBody {
                allow: r.allow,
                required_checks: r.required_checks,
                approvals: r.approvals,
                agents_may_land_without_human: r.agents_may_land_without_human,
                require_signed: r.require_signed,
                require_linear: r.require_linear,
                dismiss_stale_approvals: r.dismiss_stale_approvals,
                require_last_push_approval: r.require_last_push_approval,
                push: r.push,
                code_scanning: r.code_scanning,
                require_agent_review: r.require_agent_review,
            },
        )?;
        doc.rulesets.insert(
            name,
            Ruleset {
                mode,
                refs,
                bypass,
                body,
                repo_properties,
                loc,
            },
        );
    }

    Ok(doc)
}

fn check_name<T>(ctx: &Ctx<'_>, key: &str, at: &Spanned<T>, name: &str) -> Result<(), PolicyError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ctx.err(
            PolicyErrorKind::InvalidValue,
            key,
            at,
            format!("name `{name}` may use only ASCII letters, digits, `-`, `_` and `.`"),
        ))
    }
}
