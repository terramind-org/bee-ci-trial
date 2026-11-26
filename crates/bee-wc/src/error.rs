//! Working-copy errors. Every error a client can see names a SPEC §12.2 reason; the CLI maps
//! [`WcError::exit_class`] to its fixed exit codes.

use bee_proto::v1::ErrorDetail;

/// Result alias for working-copy operations.
pub type Result<T> = std::result::Result<T, WcError>;

/// Which fixed CLI outcome an error belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitClass {
    /// Any error without a more specific class (exit 1).
    Error,
    /// Bad arguments or input; nothing was changed (exit 2).
    Usage,
    /// The operation stopped on conflicts (exit 3).
    Conflicts,
    /// Refused because the working copy has uncommitted changes (exit 4).
    Dirty,
    /// Refused by policy (exit 5).
    PolicyDenied,
}

/// A failed working-copy operation.
#[derive(Clone, Debug, PartialEq)]
pub struct WcError {
    /// SPEC §12.2 reason name, e.g. `INVALID_STATE`.
    pub reason: String,
    /// Snake-case subcode naming the rule, e.g. `dirty_working_copy`.
    pub subcode: String,
    /// Human-readable message.
    pub message: String,
    /// Paths the error is about (dirty paths, conflicting paths), sorted.
    pub paths: Vec<String>,
    /// The server's error detail, when the server reported the error.
    pub detail: Option<Box<ErrorDetail>>,
    /// The CLI outcome.
    pub class: ExitClass,
    /// The operation's report when it changed the working copy before stopping (for
    /// example a rebase that stopped on conflicts), as JSON.
    pub report: Option<Box<serde_json::Value>>,
}

impl WcError {
    fn make(reason: &str, subcode: &str, message: impl Into<String>, class: ExitClass) -> Self {
        Self {
            reason: reason.to_owned(),
            subcode: subcode.to_owned(),
            message: message.into(),
            paths: Vec::new(),
            detail: None,
            class,
            report: None,
        }
    }

    /// A usage error (`INVALID_ARGUMENT`): the request was malformed and nothing changed.
    pub fn usage(subcode: &str, message: impl Into<String>) -> Self {
        Self::make("INVALID_ARGUMENT", subcode, message, ExitClass::Usage)
    }

    /// The working copy is in the wrong state for the operation (`INVALID_STATE`).
    pub fn state(subcode: &str, message: impl Into<String>) -> Self {
        Self::make("INVALID_STATE", subcode, message, ExitClass::Error)
    }

    /// Refusal because uncommitted changes would be lost or mixed in (`INVALID_STATE`, exit 4).
    pub fn dirty(message: impl Into<String>, paths: Vec<String>) -> Self {
        let mut e = Self::make(
            "INVALID_STATE",
            "dirty_working_copy",
            message,
            ExitClass::Dirty,
        );
        e.paths = paths;
        e
    }

    /// Unresolved conflicts stop the operation (`MERGE_CONFLICT`, exit 3).
    pub fn conflicts(message: impl Into<String>, paths: Vec<String>) -> Self {
        let mut e = Self::make("MERGE_CONFLICT", "conflicts", message, ExitClass::Conflicts);
        e.paths = paths;
        e
    }

    /// Local bytes did not match their id, or a local file is corrupt (`INTEGRITY`).
    pub fn integrity(subcode: &str, message: impl Into<String>) -> Self {
        Self::make("INTEGRITY", subcode, message, ExitClass::Error)
    }

    /// A local I/O failure (`INTERNAL`, subcode `io`).
    pub fn io(context: impl std::fmt::Display, err: &std::io::Error) -> Self {
        Self::make(
            "INTERNAL",
            "io",
            format!("{context}: {err}"),
            ExitClass::Error,
        )
    }

    /// A client-side bug or an unusable server response (`INTERNAL`).
    pub fn internal(subcode: &str, message: impl Into<String>) -> Self {
        Self::make("INTERNAL", subcode, message, ExitClass::Error)
    }

    /// The process stopped at an armed fault point (tests simulate a crash with it).
    pub fn fault(point: &str) -> Self {
        Self::make(
            "INTERNAL",
            "fault_injected",
            format!("fault injected at {point}; the next command recovers the working copy"),
            ExitClass::Error,
        )
    }

    /// The error the server reported.
    pub fn remote(detail: ErrorDetail) -> Self {
        let class = match detail.reason.as_str() {
            "MERGE_CONFLICT" => ExitClass::Conflicts,
            "POLICY_DENIED" | "LEASED" | "SECRET_DETECTED" | "BUDGET_EXHAUSTED" => {
                ExitClass::PolicyDenied
            }
            _ => ExitClass::Error,
        };
        let reason = if detail.reason.is_empty() {
            "INTERNAL".to_owned()
        } else {
            detail.reason.clone()
        };
        Self {
            reason,
            subcode: detail.subcode.clone(),
            message: detail.message.clone(),
            paths: Vec::new(),
            detail: Some(Box::new(detail)),
            class,
            report: None,
        }
    }

    /// Adds the paths the error is about.
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }

    /// Attaches the operation's report.
    pub fn with_report(mut self, report: serde_json::Value) -> Self {
        self.report = Some(Box::new(report));
        self
    }

    /// The CLI outcome class.
    pub fn exit_class(&self) -> ExitClass {
        self.class
    }

    /// True when the server reported this reason.
    pub fn is_remote_reason(&self, reason: &str) -> bool {
        self.detail.as_ref().is_some_and(|d| d.reason == reason)
    }
}

impl std::fmt::Display for WcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for WcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_reasons_pick_their_class() {
        let detail = |reason: &str| ErrorDetail {
            reason: reason.into(),
            ..Default::default()
        };
        assert_eq!(
            WcError::remote(detail("MERGE_CONFLICT")).exit_class(),
            ExitClass::Conflicts
        );
        assert_eq!(
            WcError::remote(detail("POLICY_DENIED")).exit_class(),
            ExitClass::PolicyDenied
        );
        assert_eq!(
            WcError::remote(detail("NOT_FOUND")).exit_class(),
            ExitClass::Error
        );
        assert_eq!(WcError::remote(detail("")).reason, "INTERNAL");
        assert_eq!(WcError::dirty("x", vec![]).exit_class(), ExitClass::Dirty);
        assert!(WcError::remote(detail("NOT_FOUND")).is_remote_reason("NOT_FOUND"));
    }
}
