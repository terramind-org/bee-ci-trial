//! Gateway errors: a SPEC §12.2 reason name and a message.
//!
//! Git clients cannot read a `bee.v1.ErrorDetail`, so the gateway carries the reason as text
//! and puts it in front of every message a git client shows: `ng <ref> <REASON>: <message>` in
//! a push report, `ERR <REASON>: <message>` in a fetch, and the response body of a refused
//! HTTP request (with the reason's HTTP status from the SPEC table).

use std::fmt;

use bee_core::{BeeError, Reason};

/// A client-visible gateway error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitError {
    /// The SPEC §12.2 reason name (`POLICY_DENIED`, `SECRET_DETECTED`, ...).
    pub reason: String,
    /// A human-readable explanation.
    pub message: String,
}

/// Shorthand for gateway results.
pub type GitResult<T> = Result<T, GitError>;

impl GitError {
    /// An error with a reason from the shared table.
    pub fn new(reason: Reason, message: impl Into<String>) -> GitError {
        GitError {
            reason: reason.as_str().to_string(),
            message: message.into(),
        }
    }

    /// An error with a reason given by name (as a server reports it).
    pub fn named(reason: impl Into<String>, message: impl Into<String>) -> GitError {
        GitError {
            reason: reason.into(),
            message: message.into(),
        }
    }

    /// `INVALID_ARGUMENT`: the client sent something malformed.
    pub fn invalid(message: impl Into<String>) -> GitError {
        GitError::new(Reason::InvalidArgument, message)
    }

    /// `UNAUTHENTICATED`.
    pub fn unauthenticated(message: impl Into<String>) -> GitError {
        GitError::new(Reason::Unauthenticated, message)
    }

    /// `NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> GitError {
        GitError::new(Reason::NotFound, message)
    }

    /// `INTERNAL`.
    pub fn internal(message: impl Into<String>) -> GitError {
        GitError::new(Reason::Internal, message)
    }

    /// The HTTP status of the reason (SPEC §12.2 table); 500 for an unknown name.
    pub fn http_status(&self) -> u16 {
        bee_proto::errors::parse_reason(&self.reason)
            .and_then(bee_proto::errors::reason_info)
            .map_or(500, |i| i.http_status())
    }

    /// True for the reasons that mean "send credentials" (HTTP 401).
    pub fn is_unauthenticated(&self) -> bool {
        self.http_status() == 401
    }

    /// The one-line form git clients show: `<REASON>: <message>`, newlines flattened.
    pub fn line(&self) -> String {
        let msg: String = self
            .message
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        format!("{}: {}", self.reason, msg.trim())
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line())
    }
}

impl std::error::Error for GitError {}

impl From<BeeError> for GitError {
    fn from(e: BeeError) -> GitError {
        GitError::new(e.reason, e.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_the_reason_table() {
        assert_eq!(
            GitError::unauthenticated("x").http_status(),
            401,
            "credentials needed"
        );
        assert_eq!(GitError::named("POLICY_DENIED", "x").http_status(), 403);
        assert_eq!(GitError::not_found("x").http_status(), 404);
        assert_eq!(GitError::named("SECRET_DETECTED", "x").http_status(), 400);
        assert_eq!(GitError::named("NO_SUCH_REASON", "x").http_status(), 500);
        assert!(GitError::named("TOKEN_EXPIRED", "x").is_unauthenticated());
    }

    #[test]
    fn line_is_single_line() {
        let e = GitError::named("SECRET_DETECTED", "found\na secret ");
        assert_eq!(e.line(), "SECRET_DETECTED: found a secret");
        assert_eq!(e.to_string(), e.line());
    }
}
