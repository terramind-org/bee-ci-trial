//! The per-call context every store method takes.
//!
//! A [`Cx`] says who is acting (with the full delegation chain), where the call came from,
//! which policy decision allowed it, and the deadline. Store methods use it for the audit
//! entry they write and to bound every database wait (SPEC §15.3 "every call has a
//! deadline").

use std::time::Duration;

use bee_proto::v1::{Principal, PrincipalKind};
use tokio::time::Instant;

use crate::error::{ForgeError, Result};

/// Deadline applied when a request carries none.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// Who is calling, from where, why it was allowed, and until when.
#[derive(Clone, Debug)]
pub struct Cx {
    /// Acting principal id.
    pub actor_id: String,
    /// Acting principal kind.
    pub actor_kind: PrincipalKind,
    /// Principal ids from the actor back to the delegation root; `chain[0] == actor_id`.
    pub chain: Vec<String>,
    /// Id of the token used, if any.
    pub token_id: String,
    /// Request id for log correlation.
    pub request_id: String,
    /// Source address as reported by the transport.
    pub ip: String,
    /// User agent.
    pub user_agent: String,
    /// The policy decision that allowed the call (rule and detail), recorded in the audit log.
    pub decision: String,
    /// A reason the actor supplied, if any.
    pub reason: String,
    /// Absolute deadline.
    pub deadline: Instant,
}

impl Cx {
    /// A context for `actor` with the default deadline.
    pub fn new(actor_id: impl Into<String>, actor_kind: PrincipalKind) -> Cx {
        let actor_id = actor_id.into();
        Cx {
            chain: vec![actor_id.clone()],
            actor_id,
            actor_kind,
            token_id: String::new(),
            request_id: String::new(),
            ip: String::new(),
            user_agent: String::new(),
            decision: String::new(),
            reason: String::new(),
            deadline: Instant::now() + DEFAULT_DEADLINE,
        }
    }

    /// The server itself (background workers, expiry sweeps).
    pub fn system() -> Cx {
        let mut cx = Cx::new("system", PrincipalKind::Service);
        cx.decision = "system".into();
        cx
    }

    /// Replaces the deadline.
    pub fn with_deadline(mut self, deadline: Instant) -> Cx {
        self.deadline = deadline;
        self
    }

    /// Sets the deadline `d` from now.
    pub fn with_timeout(mut self, d: Duration) -> Cx {
        self.deadline = Instant::now() + d;
        self
    }

    /// Sets the delegation chain (actor first).
    pub fn with_chain(mut self, chain: Vec<String>) -> Cx {
        if chain.first().is_some_and(|c| *c == self.actor_id) {
            self.chain = chain;
        } else {
            let mut full = vec![self.actor_id.clone()];
            full.extend(chain);
            self.chain = full;
        }
        self
    }

    /// The delegation root: the last principal in the chain.
    pub fn root(&self) -> &str {
        self.chain
            .last()
            .map(String::as_str)
            .unwrap_or(&self.actor_id)
    }

    /// The acting principal as a proto message.
    pub fn principal(&self) -> Principal {
        Principal {
            id: self.actor_id.clone(),
            kind: self.actor_kind as i32,
            ..Default::default()
        }
    }

    /// Fails with `DEADLINE_EXCEEDED` if the deadline passed.
    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            Err(ForgeError::deadline())
        } else {
            Ok(())
        }
    }
}
