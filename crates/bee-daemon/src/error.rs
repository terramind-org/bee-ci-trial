//! Errors the daemon returns: always a SPEC §12.2 reason carried in a `bee.v1.ErrorDetail`.

use bee_proto::errors;
use bee_proto::v1::{ErrorDetail, ErrorReason};

/// Result with a [`ErrorDetail`] error.
pub type Result<T, E = Box<ErrorDetail>> = std::result::Result<T, E>;

/// An error detail for `reason`.
pub fn err(reason: ErrorReason, message: impl Into<String>) -> Box<ErrorDetail> {
    Box::new(errors::error_detail(reason, message))
}

/// `INVALID_ARGUMENT` with a subcode.
pub fn invalid(subcode: &str, message: impl Into<String>) -> Box<ErrorDetail> {
    let mut d = errors::error_detail(ErrorReason::InvalidArgument, message);
    d.subcode = subcode.to_owned();
    Box::new(d)
}

/// `INTERNAL` for a local I/O failure.
pub fn io(what: impl std::fmt::Display, e: &std::io::Error) -> Box<ErrorDetail> {
    err(ErrorReason::Internal, format!("{what}: {e}"))
}

/// `UNAVAILABLE` (retryable) for an unreachable upstream.
pub fn unavailable(message: impl Into<String>) -> Box<ErrorDetail> {
    err(ErrorReason::Unavailable, message)
}

/// The reason of a detail, parsed (`INTERNAL` for an unknown name).
pub fn reason_of(detail: &ErrorDetail) -> ErrorReason {
    errors::parse_reason(&detail.reason).unwrap_or(ErrorReason::Internal)
}

/// The tonic status carrying `detail`.
pub fn to_status(detail: &ErrorDetail) -> tonic::Status {
    errors::status_from_detail(detail)
}

/// The detail in a tonic status (one built from its code when it has none).
pub fn from_status(status: &tonic::Status) -> Box<ErrorDetail> {
    Box::new(errors::detail_from_status(status).unwrap_or_else(|| {
        errors::error_detail(errors::default_reason(status.code()), status.message())
    }))
}

/// HTTP status of a detail's reason (500 for an unknown reason).
pub fn http_status(detail: &ErrorDetail) -> u16 {
    errors::parse_reason(&detail.reason)
        .and_then(errors::reason_info)
        .map_or(500, |info| info.http_status())
}
