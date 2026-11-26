//! Forge errors: every client-visible failure is a SPEC §12.2 reason.
//!
//! [`ForgeError`] carries a [`bee_proto::v1::ErrorReason`], an optional snake_case subcode
//! and an optional typed payload. [`ForgeError::to_status`] turns it into a `tonic::Status`
//! whose `grpc-status-details-bin` holds the `bee.v1.ErrorDetail`, with the gRPC code and
//! `retryable` flag taken from the reason table in `bee_proto::errors`.

use std::fmt;

use bee_proto::errors::{reason_info, reason_name, status_with_detail};
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::{ErrorDetail, ErrorReason, LeaseConflict, PolicyDenial, VersionMismatch};

/// Result alias used throughout the crate.
pub type Result<T, E = ForgeError> = std::result::Result<T, E>;

/// A client-visible forge error.
#[derive(Clone, Debug, PartialEq)]
pub struct ForgeError {
    /// The SPEC §12.2 reason.
    pub reason: ErrorReason,
    /// Human-readable explanation; not part of the contract.
    pub message: String,
    /// Finer rule, snake_case (for example `missing_field`).
    pub subcode: String,
    /// Suggested retry delay for retryable reasons.
    pub retry_after_ms: u64,
    /// Typed payload, where the reason table names one.
    pub payload: Option<Box<Payload>>,
}

impl ForgeError {
    /// An error with no subcode and no payload.
    pub fn new(reason: ErrorReason, message: impl Into<String>) -> ForgeError {
        ForgeError {
            reason,
            message: message.into(),
            subcode: String::new(),
            retry_after_ms: 0,
            payload: None,
        }
    }

    /// `INVALID_ARGUMENT` with a subcode.
    pub fn invalid(subcode: &str, message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::InvalidArgument, message).with_subcode(subcode)
    }

    /// `INVALID_ARGUMENT` / `missing_field` naming the field.
    pub fn missing(field: &str) -> ForgeError {
        ForgeError::invalid("missing_field", format!("`{field}` is required"))
    }

    /// `NOT_FOUND`.
    pub fn not_found(what: impl fmt::Display) -> ForgeError {
        ForgeError::new(ErrorReason::NotFound, format!("{what} not found"))
    }

    /// `ALREADY_EXISTS`.
    pub fn already_exists(what: impl fmt::Display) -> ForgeError {
        ForgeError::new(ErrorReason::AlreadyExists, format!("{what} already exists"))
    }

    /// `PERMISSION_DENIED`.
    pub fn permission_denied(message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::PermissionDenied, message)
    }

    /// `POLICY_DENIED` with the rule and what is missing.
    pub fn policy_denied(
        rule: &str,
        missing: Vec<String>,
        message: impl Into<String>,
    ) -> ForgeError {
        ForgeError::new(ErrorReason::PolicyDenied, message).with_payload(Payload::PolicyDenial(
            PolicyDenial {
                rule: rule.to_string(),
                missing,
            },
        ))
    }

    /// `VERSION_MISMATCH` carrying the current version.
    pub fn version_mismatch(expected: u64, current: u64) -> ForgeError {
        ForgeError::new(
            ErrorReason::VersionMismatch,
            format!("expected version {expected}, current version is {current}"),
        )
        .with_payload(Payload::VersionMismatch(VersionMismatch {
            expected_version: expected,
            current_version: current,
            ..Default::default()
        }))
    }

    /// `LEASED` naming the conflicting leases and paths.
    pub fn leased(lease_ids: Vec<String>, paths: Vec<Vec<u8>>) -> ForgeError {
        ForgeError::new(
            ErrorReason::Leased,
            "an exclusive lease held by another principal overlaps",
        )
        .with_payload(Payload::LeaseConflict(LeaseConflict { lease_ids, paths }))
    }

    /// `INVALID_STATE`.
    pub fn invalid_state(message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::InvalidState, message)
    }

    /// `ARCHIVED`.
    pub fn archived(repo: &str) -> ForgeError {
        ForgeError::new(ErrorReason::Archived, format!("repo {repo} is archived"))
    }

    /// `UNAVAILABLE` (retryable).
    pub fn unavailable(message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::Unavailable, message)
    }

    /// `DEADLINE_EXCEEDED` (retryable).
    pub fn deadline() -> ForgeError {
        ForgeError::new(ErrorReason::DeadlineExceeded, "deadline exceeded")
    }

    /// `INTERNAL`.
    pub fn internal(message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::Internal, message)
    }

    /// `UNIMPLEMENTED`.
    pub fn unimplemented(message: impl Into<String>) -> ForgeError {
        ForgeError::new(ErrorReason::Unimplemented, message)
    }

    /// `TOO_LARGE` naming the limit.
    pub fn too_large(limit: &str, max: u64, actual: u64) -> ForgeError {
        ForgeError::new(
            ErrorReason::TooLarge,
            format!("{limit} is {actual}, above the limit of {max}"),
        )
        .with_payload(Payload::LimitExceeded(bee_proto::v1::LimitExceeded {
            limit: limit.to_string(),
            maximum: max,
            actual,
        }))
    }

    /// Sets the subcode.
    pub fn with_subcode(mut self, subcode: &str) -> ForgeError {
        self.subcode = subcode.to_string();
        self
    }

    /// Sets the payload.
    pub fn with_payload(mut self, payload: Payload) -> ForgeError {
        self.payload = Some(Box::new(payload));
        self
    }

    /// Sets the retry delay.
    pub fn with_retry_after(mut self, ms: u64) -> ForgeError {
        self.retry_after_ms = ms;
        self
    }

    /// Whether the identical request may succeed if retried (from the reason table).
    pub fn retryable(&self) -> bool {
        reason_info(self.reason).is_some_and(|i| i.retryable)
    }

    /// Wire name of the reason, e.g. `NOT_FOUND`.
    pub fn reason_name(&self) -> &'static str {
        reason_name(self.reason)
    }

    /// The `bee.v1.ErrorDetail` for this error.
    pub fn to_detail(&self, request_id: &str) -> ErrorDetail {
        ErrorDetail {
            reason: self.reason_name().to_string(),
            message: self.message.clone(),
            retryable: self.retryable(),
            subcode: self.subcode.clone(),
            retry_after_ms: self.retry_after_ms,
            request_id: request_id.to_string(),
            payload: self.payload.as_deref().cloned(),
        }
    }

    /// A `tonic::Status` with the detail attached.
    pub fn to_status(&self, request_id: &str) -> tonic::Status {
        let code = reason_info(self.reason)
            .map(|i| i.code)
            .unwrap_or(tonic::Code::Internal);
        status_with_detail(code, &self.to_detail(request_id))
    }
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.subcode.is_empty() {
            write!(f, "{}: {}", self.reason_name(), self.message)
        } else {
            write!(
                f,
                "{} ({}): {}",
                self.reason_name(),
                self.subcode,
                self.message
            )
        }
    }
}

impl std::error::Error for ForgeError {}

impl From<bee_auth::AuthError> for ForgeError {
    fn from(e: bee_auth::AuthError) -> ForgeError {
        use bee_auth::AuthReason as A;
        let reason = match e.reason {
            A::Unauthenticated => ErrorReason::Unauthenticated,
            A::TokenExpired => ErrorReason::TokenExpired,
            A::TokenRevoked => ErrorReason::TokenRevoked,
            A::PolicyDenied => ErrorReason::PolicyDenied,
            A::BudgetExhausted => ErrorReason::BudgetExhausted,
            A::InvalidArgument => ErrorReason::InvalidArgument,
        };
        ForgeError::new(reason, e.message)
    }
}

impl From<prost::DecodeError> for ForgeError {
    fn from(e: prost::DecodeError) -> ForgeError {
        ForgeError::new(
            ErrorReason::Internal,
            format!("stored record does not decode: {e}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::errors::detail_from_status;

    #[test]
    fn status_carries_the_detail() {
        let e = ForgeError::leased(vec!["L1".into()], vec![b"src".to_vec()]);
        let st = e.to_status("req-1");
        assert_eq!(st.code(), tonic::Code::FailedPrecondition);
        let d = detail_from_status(&st).expect("detail");
        assert_eq!(d.reason, "LEASED");
        assert_eq!(d.request_id, "req-1");
        assert!(!d.retryable);
        assert!(matches!(d.payload, Some(Payload::LeaseConflict(_))));
    }

    #[test]
    fn retryable_comes_from_the_table() {
        assert!(ForgeError::unavailable("x").retryable());
        assert!(!ForgeError::not_found("x").retryable());
        let st = ForgeError::version_mismatch(1, 2).to_status("");
        assert_eq!(st.code(), tonic::Code::Aborted);
    }
}
