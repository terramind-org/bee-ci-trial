//! Error constructors shared by the parsers. Every error a client can see carries a SPEC §12.2
//! reason: malformed git data is `INVALID_ARGUMENT` (with a subcode), and bytes that do not
//! match their checksum or id are `INTEGRITY`.

use bee_core::{BeeError, Reason, Subcode};

/// Malformed git object or pack structure: `INVALID_ARGUMENT`/`INVALID_FIELD`.
pub(crate) fn malformed(what: &str, why: impl std::fmt::Display) -> BeeError {
    BeeError::invalid(
        Subcode::InvalidField,
        format!("malformed git {what}: {why}"),
    )
    .with_payload("git", what.to_string())
}

/// Checksum, hash or delta-consistency failure: `INTEGRITY`.
pub(crate) fn integrity(why: impl Into<String>) -> BeeError {
    BeeError::new(Reason::Integrity, why)
}
