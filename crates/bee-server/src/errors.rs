//! Client-visible errors (SPEC §12.2): every failure leaves the server as a tonic status
//! carrying a `bee.v1.ErrorDetail` with a reason from the table, `retryable`, a subcode for
//! `INVALID_ARGUMENT`, and the typed payload the table names.

use bee_core::{BeeError, PayloadValue, Reason};
use bee_proto::errors as perr;
use bee_proto::v1::{self, ErrorDetail, ErrorReason, error_detail::Payload};

/// A client-visible error: an [`ErrorDetail`] ready to send.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// The detail.
    pub detail: Box<ErrorDetail>,
}

/// Shorthand for handler results.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// An error for `reason` with the table's `retryable`.
    pub fn new(reason: ErrorReason, message: impl Into<String>) -> ApiError {
        ApiError {
            detail: Box::new(perr::error_detail(reason, message)),
        }
    }

    /// `INVALID_ARGUMENT` with a snake_case subcode.
    pub fn invalid(subcode: &str, message: impl Into<String>) -> ApiError {
        let mut e = ApiError::new(ErrorReason::InvalidArgument, message);
        e.detail.subcode = subcode.to_string();
        e
    }

    /// `UNAUTHENTICATED`.
    pub fn unauthenticated(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorReason::Unauthenticated, message)
    }

    /// `PERMISSION_DENIED`.
    pub fn denied(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorReason::PermissionDenied, message)
    }

    /// `NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorReason::NotFound, message)
    }

    /// `UNIMPLEMENTED`.
    pub fn unimplemented(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorReason::Unimplemented, message)
    }

    /// `POLICY_DENIED` (or `LEASED`) with the missing requirements.
    pub fn policy(
        reason: ErrorReason,
        rule: &str,
        missing: Vec<String>,
        message: impl Into<String>,
    ) -> ApiError {
        let mut e = ApiError::new(reason, message);
        e.detail.payload = Some(Payload::PolicyDenial(v1::PolicyDenial {
            rule: rule.to_string(),
            missing,
        }));
        e
    }

    /// `RESOURCE_EXHAUSTED` with a retry delay.
    pub fn throttled(message: impl Into<String>, retry_after_ms: u64, limit: &str) -> ApiError {
        let mut e = ApiError::new(ErrorReason::ResourceExhausted, message);
        e.detail.retry_after_ms = retry_after_ms.max(1);
        e.detail.payload = Some(Payload::LimitExceeded(v1::LimitExceeded {
            limit: limit.to_string(),
            maximum: 0,
            actual: 0,
        }));
        e
    }

    /// Sets the request id.
    pub fn with_request_id(mut self, id: &str) -> ApiError {
        if self.detail.request_id.is_empty() {
            self.detail.request_id = id.to_string();
        }
        self
    }

    /// The reason.
    pub fn reason(&self) -> ErrorReason {
        perr::parse_reason(&self.detail.reason).unwrap_or(ErrorReason::Internal)
    }

    /// The tonic status, with `retry-after` metadata when a delay is set.
    pub fn into_status(self) -> tonic::Status {
        let mut status = perr::status_from_detail(&self.detail);
        if self.detail.retry_after_ms > 0 {
            let secs = self.detail.retry_after_ms.div_ceil(1000).max(1);
            if let Ok(v) = secs.to_string().parse() {
                status.metadata_mut().insert("retry-after", v);
            }
        }
        status
    }
}

impl From<ApiError> for tonic::Status {
    fn from(e: ApiError) -> tonic::Status {
        e.into_status()
    }
}

fn payload_str<'a>(e: &'a BeeError, key: &str) -> Option<&'a str> {
    match e.payload.get(key) {
        Some(PayloadValue::Str(s)) => Some(s),
        _ => None,
    }
}

fn payload_int(e: &BeeError, key: &str) -> Option<i64> {
    match e.payload.get(key) {
        Some(PayloadValue::Int(i)) => Some(*i),
        _ => None,
    }
}

fn object_id(text: &str) -> Option<v1::ObjectId> {
    bee_core::ObjectId::parse(text).ok().map(|id| v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    })
}

/// The proto reason for a domain error.
pub fn reason_of(e: &BeeError) -> ErrorReason {
    if e.reason == Reason::InvalidArgument {
        match payload_str(e, "rule") {
            Some("multi_parent") => return ErrorReason::MultiParent,
            Some("merge_in_stack") => return ErrorReason::MergeInStack,
            _ => {}
        }
    }
    match e.reason {
        Reason::LandAttemptsExhausted => ErrorReason::LandContention,
        r => perr::parse_reason(r.as_str()).unwrap_or(ErrorReason::Internal),
    }
}

impl From<BeeError> for ApiError {
    fn from(e: BeeError) -> ApiError {
        let reason = reason_of(&e);
        let mut detail = perr::error_detail(reason, e.message.clone());
        if e.retryable && reason == ErrorReason::VersionMismatch {
            // Store conflicts are safe to retry with a fresh read but not unchanged.
            detail.retryable = false;
        }
        if let Some(sub) = payload_str(&e, bee_core::error::SUBCODE_KEY) {
            detail.subcode = sub.to_ascii_lowercase();
        } else if let Some(rule) = payload_str(&e, "rule") {
            detail.subcode = rule.to_string();
        }
        detail.payload = match reason {
            ErrorReason::VersionMismatch => Some(Payload::VersionMismatch(v1::VersionMismatch {
                r#ref: payload_str(&e, "ref").unwrap_or_default().to_string(),
                expected_version: payload_int(&e, "expected_version").unwrap_or(0).max(0) as u64,
                current_version: payload_int(&e, "current_version").unwrap_or(0).max(0) as u64,
                current_target: None,
            })),
            ErrorReason::MissingObject => {
                let ids: Vec<v1::ObjectId> = ["object", "id"]
                    .iter()
                    .filter_map(|k| payload_str(&e, k))
                    .filter_map(object_id)
                    .collect();
                Some(Payload::MissingObjects(v1::MissingObjects { ids }))
            }
            ErrorReason::AmbiguousPrefix => {
                let candidates = match e.payload.get("candidates") {
                    Some(PayloadValue::List(items)) => items
                        .iter()
                        .filter_map(|v| match v {
                            PayloadValue::Str(s) => object_id(s),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Some(Payload::AmbiguousPrefix(v1::AmbiguousPrefix {
                    prefix: payload_str(&e, "prefix").unwrap_or_default().to_string(),
                    candidates,
                }))
            }
            ErrorReason::TooLarge => Some(Payload::LimitExceeded(v1::LimitExceeded {
                limit: payload_str(&e, "limit").unwrap_or_default().to_string(),
                maximum: payload_int(&e, "max").unwrap_or(0).max(0) as u64,
                actual: payload_int(&e, "actual").unwrap_or(0).max(0) as u64,
            })),
            _ => None,
        };
        ApiError {
            detail: Box::new(detail),
        }
    }
}

impl From<bee_auth::AuthError> for ApiError {
    fn from(e: bee_auth::AuthError) -> ApiError {
        let reason = perr::parse_reason(e.reason.as_str()).unwrap_or(ErrorReason::Unauthenticated);
        ApiError::new(reason, e.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Subcode;

    #[test]
    fn domain_errors_map_to_reasons_and_payloads() {
        let e: ApiError = BeeError::invalid(Subcode::InvalidPath, "bad").into();
        assert_eq!(e.detail.reason, "INVALID_ARGUMENT");
        assert_eq!(e.detail.subcode, "invalid_path");
        let e: ApiError = BeeError::invalid(Subcode::InvalidParents, "merge")
            .with_payload("rule", "merge_in_stack")
            .into();
        assert_eq!(e.detail.reason, "MERGE_IN_STACK");
        let e: ApiError = BeeError::new(Reason::LandAttemptsExhausted, "busy").into();
        assert_eq!(e.detail.reason, "LAND_CONTENTION");
        assert!(e.detail.retryable);
        let e: ApiError = BeeError::new(Reason::VersionMismatch, "stale")
            .with_payload("current_version", 7i64)
            .into();
        match e.detail.payload {
            Some(Payload::VersionMismatch(v)) => assert_eq!(v.current_version, 7),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn throttled_sets_retry_after() {
        let s = ApiError::throttled("slow down", 1500, "per_root_rps").into_status();
        assert_eq!(s.code(), tonic::Code::ResourceExhausted);
        assert_eq!(s.metadata().get("retry-after").unwrap(), "2");
        let d = perr::detail_from_status(&s).unwrap();
        assert_eq!(d.retry_after_ms, 1500);
        assert!(d.retryable);
    }
}
