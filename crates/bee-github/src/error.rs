//! Errors. Everything a caller can see is a [`BeeError`] with a SPEC §12.2 reason; these
//! helpers build the ones this crate returns.

use bee_core::{BeeError, Reason};

pub use bee_core::Result;

/// `UNAVAILABLE` (retryable): GitHub or the network failed.
pub fn unavailable(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Unavailable, message)
}

/// `RESOURCE_EXHAUSTED` with the seconds to wait, after GitHub's rate limit stopped us for
/// longer than the client is allowed to wait.
pub fn rate_limited(message: impl Into<String>, retry_after_secs: u64) -> BeeError {
    BeeError::new(Reason::ResourceExhausted, message).with_payload(
        "retry_after_secs",
        bee_core::PayloadValue::Int(i64::try_from(retry_after_secs).unwrap_or(i64::MAX)),
    )
}

/// `UNAUTHENTICATED`: no usable GitHub credential.
pub fn unauthenticated(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Unauthenticated, message)
}

/// `PERMISSION_DENIED`: the credential cannot read what was asked.
pub fn denied(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::PermissionDenied, message)
}

/// `NOT_FOUND`.
pub fn not_found(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::NotFound, message)
}

/// `INVALID_ARGUMENT` with a `subcode` payload naming the problem.
pub fn invalid(subcode: &str, message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::InvalidArgument, message).with_payload("subcode", subcode)
}

/// `INVALID_STATE`: the import is in the wrong phase for the operation.
pub fn invalid_state(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::InvalidState, message)
}

/// `INTERNAL`.
pub fn internal(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Internal, message)
}

/// `INTEGRITY`: downloaded bytes do not match their declared digest.
pub fn integrity(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Integrity, message)
}

/// `UNIMPLEMENTED`: the target cannot take this kind of record.
pub fn unimplemented(message: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Unimplemented, message)
}

/// An I/O failure on local files (state, mirror, archive).
pub fn io(context: &str, e: std::io::Error) -> BeeError {
    match e.kind() {
        std::io::ErrorKind::NotFound => not_found(format!("{context}: {e}")),
        std::io::ErrorKind::PermissionDenied => denied(format!("{context}: {e}")),
        _ => unavailable(format!("{context}: {e}")),
    }
}

/// A JSON document that does not have the expected shape.
pub fn bad_json(context: &str, e: impl std::fmt::Display) -> BeeError {
    unavailable(format!("{context}: unexpected JSON from GitHub: {e}"))
}
