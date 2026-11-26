//! Who is asking: principal ids, kinds, actors and the subjects policy rules name.

use std::fmt;

/// A principal id (a ULID in text form, SPEC §3.2). Kept as a string here; the shared domain
/// type lives in `bee-core` and is converted at the integration boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrincipalId(pub String);

impl PrincipalId {
    /// Wraps a principal id.
    pub fn new(id: impl Into<String>) -> PrincipalId {
        PrincipalId(id.into())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The three kinds of principal (SPEC §9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrincipalKind {
    /// A person.
    Human,
    /// An AI agent; always owned by a human or service.
    Agent,
    /// CI, the importer, bots.
    Service,
}

impl PrincipalKind {
    /// Lower-case name used in policy files and tokens.
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Human => "human",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Service => "service",
        }
    }

    /// Parses `human`, `agent` or `service`.
    pub fn parse(s: &str) -> Option<PrincipalKind> {
        match s {
            "human" => Some(PrincipalKind::Human),
            "agent" => Some(PrincipalKind::Agent),
            "service" => Some(PrincipalKind::Service),
            _ => None,
        }
    }

    /// The built-in group every principal of this kind belongs to.
    pub fn builtin_group(self) -> &'static str {
        match self {
            PrincipalKind::Human => "humans",
            PrincipalKind::Agent => "agents",
            PrincipalKind::Service => "services",
        }
    }
}

/// One principal as the evaluator sees it: id, kind, the external groups it belongs to (for
/// example GitHub teams, `org/team`) and aliases that `CODEOWNERS` entries may name (a GitHub
/// login or an email address). Groups defined in the policy file are resolved by the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// Principal id.
    pub id: PrincipalId,
    /// Principal kind.
    pub kind: PrincipalKind,
    /// External group names, without `@`.
    pub groups: Vec<String>,
    /// Aliases matched by `CODEOWNERS` owners (`login`, `user@example.com`).
    pub aliases: Vec<String>,
}

impl Actor {
    /// An actor with no external groups or aliases.
    pub fn new(id: impl Into<String>, kind: PrincipalKind) -> Actor {
        Actor {
            id: PrincipalId::new(id),
            kind,
            groups: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// Adds external groups.
    pub fn with_groups<I, S>(mut self, groups: I) -> Actor
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups.extend(groups.into_iter().map(Into::into));
        self
    }

    /// Sets the external group that carries the actor's forge repo role
    /// (`repo-role/<role>`, [`REPO_ROLE_GROUP_PREFIX`]): any such group the actor already had
    /// is dropped first, so only the caller's own decision counts; `None` leaves it without
    /// one.
    pub fn with_repo_role(mut self, role: Option<&str>) -> Actor {
        self.groups.retain(|g| !is_repo_role_group(g));
        if let Some(r) = role {
            self.groups.push(format!("{REPO_ROLE_GROUP_PREFIX}{r}"));
        }
        self
    }

    /// Adds `CODEOWNERS` aliases.
    pub fn with_aliases<I, S>(mut self, aliases: I) -> Actor
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aliases.extend(aliases.into_iter().map(Into::into));
        self
    }
}

/// Prefix of the external groups that carry a principal's forge repo role into policy
/// (`@repo-role/read` .. `@repo-role/admin`, SPEC §9.4). Only the server computes them, per
/// repo; a group of this shape from any other source (an identity provider, a forge team of an
/// org named `repo-role`) is dropped.
pub const REPO_ROLE_GROUP_PREFIX: &str = "repo-role/";

/// Whether `group` (with or without a leading `@`) is a repo-role group.
pub fn is_repo_role_group(group: &str) -> bool {
    group
        .trim_start_matches('@')
        .starts_with(REPO_ROLE_GROUP_PREFIX)
}

/// Names reserved for built-in groups; a policy file may not define them.
pub const BUILTIN_GROUPS: [&str; 4] = ["everyone", "humans", "agents", "services"];

/// What a policy rule names: a principal id or a group (`@name`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    /// A principal by id.
    Principal(PrincipalId),
    /// A group by name (without `@`). Names containing `/` are external groups.
    Group(String),
}

impl Subject {
    /// Parses `@group` or a principal id.
    pub fn parse(raw: &str) -> Result<Subject, String> {
        if raw.is_empty() || raw.chars().any(char::is_whitespace) {
            return Err(format!("subject `{raw}` is empty or contains whitespace"));
        }
        match raw.strip_prefix('@') {
            Some("") => Err("subject `@` names no group".to_string()),
            Some(g) => Ok(Subject::Group(g.to_string())),
            None => Ok(Subject::Principal(PrincipalId::new(raw))),
        }
    }

    /// Whether this is an external group (contains `/`, resolved by the identity provider).
    pub fn is_external_group(&self) -> bool {
        matches!(self, Subject::Group(g) if g.contains('/'))
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::Principal(p) => f.write_str(p.as_str()),
            Subject::Group(g) => write!(f, "@{g}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_role_replaces_any_earlier_repo_role_group() {
        let a = Actor::new("A", PrincipalKind::Agent)
            .with_groups(["acme/core", "repo-role/admin", "@repo-role/write"])
            .with_repo_role(Some("maintain"));
        assert_eq!(a.groups, vec!["acme/core", "repo-role/maintain"]);
        let a = a.with_repo_role(None);
        assert_eq!(a.groups, vec!["acme/core"]);
        assert!(is_repo_role_group("@repo-role/read"));
        assert!(!is_repo_role_group("repo-roles/read"));
    }
}
