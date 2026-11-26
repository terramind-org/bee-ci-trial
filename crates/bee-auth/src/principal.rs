//! Principals: humans, agents and services (SPEC §9.1).

use bee_policy::{Actor, PrincipalId, PrincipalKind};
use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthReason};

/// What an agent principal records about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// The human or service that created and owns the agent.
    pub owner: PrincipalId,
    /// The runtime, e.g. `claude-code` or `codex`.
    pub runtime: String,
    /// What the agent is for.
    pub purpose: String,
}

/// A principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// ULID in text form.
    pub id: PrincipalId,
    /// Kind.
    pub kind: PrincipalKind,
    /// Owning org, if any.
    pub org_id: Option<String>,
    /// Display name (a mutable label).
    pub display: String,
    /// Stable provider ids such as `github:12345`, never logins.
    pub external_ids: Vec<String>,
    /// Set exactly for agents.
    pub agent: Option<AgentInfo>,
}

impl Principal {
    /// A human principal.
    pub fn human(id: impl Into<String>, display: impl Into<String>) -> Principal {
        Principal::base(id, PrincipalKind::Human, display)
    }

    /// A service principal.
    pub fn service(id: impl Into<String>, display: impl Into<String>) -> Principal {
        Principal::base(id, PrincipalKind::Service, display)
    }

    /// An agent principal owned by `info.owner`.
    pub fn agent(id: impl Into<String>, display: impl Into<String>, info: AgentInfo) -> Principal {
        let mut p = Principal::base(id, PrincipalKind::Agent, display);
        p.agent = Some(info);
        p
    }

    fn base(id: impl Into<String>, kind: PrincipalKind, display: impl Into<String>) -> Principal {
        Principal {
            id: PrincipalId::new(id),
            kind,
            org_id: None,
            display: display.into(),
            external_ids: Vec::new(),
            agent: None,
        }
    }

    /// Checks the invariants: a non-empty id; agent info present exactly for agents; an agent
    /// is not its own owner and has a runtime; external ids look like `provider:id`.
    pub fn validate(&self) -> Result<(), AuthError> {
        let bad = |m: String| Err(AuthError::new(AuthReason::InvalidArgument, m));
        if self.id.as_str().is_empty() {
            return bad("principal id is empty".into());
        }
        match (self.kind, &self.agent) {
            (PrincipalKind::Agent, None) => return bad(format!("agent {} has no owner", self.id)),
            (PrincipalKind::Agent, Some(info)) => {
                if info.owner == self.id || info.owner.as_str().is_empty() {
                    return bad(format!(
                        "agent {} needs an owner other than itself",
                        self.id
                    ));
                }
                if info.runtime.is_empty() {
                    return bad(format!("agent {} has no runtime", self.id));
                }
            }
            (_, Some(_)) => return bad(format!("{} is not an agent but has agent info", self.id)),
            (_, None) => {}
        }
        for e in &self.external_ids {
            match e.split_once(':') {
                Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {}
                _ => return bad(format!("external id `{e}` is not `provider:id`")),
            }
        }
        Ok(())
    }

    /// The id and kind, as carried in tokens.
    pub fn to_ref(&self) -> PrincipalRef {
        PrincipalRef {
            id: self.id.as_str().to_string(),
            kind: self.kind,
        }
    }
}

/// A principal as named inside a token: id and kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrincipalRef {
    /// Principal id.
    pub id: String,
    /// Kind.
    #[serde(with = "kind_serde")]
    pub kind: PrincipalKind,
}

impl PrincipalRef {
    /// Builds a reference.
    pub fn new(id: impl Into<String>, kind: PrincipalKind) -> PrincipalRef {
        PrincipalRef {
            id: id.into(),
            kind,
        }
    }

    /// The policy evaluator's view, with external groups and `CODEOWNERS` aliases supplied by
    /// the identity store.
    pub fn to_actor(&self, groups: Vec<String>, aliases: Vec<String>) -> Actor {
        Actor {
            id: PrincipalId::new(self.id.clone()),
            kind: self.kind,
            groups,
            aliases,
        }
    }
}

mod kind_serde {
    use bee_policy::PrincipalKind;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(k: &PrincipalKind, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(k.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PrincipalKind, D::Error> {
        let s = String::deserialize(d)?;
        PrincipalKind::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown kind `{s}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_always_have_an_owner() {
        let mut a = Principal::agent(
            "A1",
            "helper",
            AgentInfo {
                owner: PrincipalId::new("H1"),
                runtime: "claude-code".into(),
                purpose: "docs".into(),
            },
        );
        assert!(a.validate().is_ok());
        a.agent = None;
        assert_eq!(
            a.validate().unwrap_err().reason,
            AuthReason::InvalidArgument
        );
        let mut h = Principal::human("H1", "Ada");
        h.external_ids.push("github:42".into());
        assert!(h.validate().is_ok());
        h.external_ids.push("ada".into());
        assert!(h.validate().is_err());
        let mut s = Principal::service("S1", "ci");
        s.agent = Some(AgentInfo {
            owner: PrincipalId::new("H1"),
            runtime: "x".into(),
            purpose: String::new(),
        });
        assert!(s.validate().is_err());
    }
}
