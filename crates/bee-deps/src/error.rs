//! Errors returned by this crate and their mapping to the SPEC §12.2 contract.

use std::fmt;

use bee_core::{BeeError, Reason};

/// Largest single manifest, lockfile or advisory file the parsers accept, in bytes.
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

/// What kind of failure a [`DepsError`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepsErrorKind {
    /// The input is malformed (`INVALID_ARGUMENT`).
    Invalid,
    /// The input is above [`MAX_FILE_BYTES`] (`TOO_LARGE`).
    TooLarge,
}

/// A manifest, lockfile, advisory record or package URL could not be processed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepsError {
    /// The failure kind.
    pub kind: DepsErrorKind,
    /// The file the problem is in; empty when the input is not a file.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl DepsError {
    /// An invalid-input error.
    pub fn invalid(path: &str, message: impl Into<String>) -> DepsError {
        DepsError {
            kind: DepsErrorKind::Invalid,
            path: path.to_owned(),
            message: message.into(),
        }
    }

    /// Maps the error to the client-visible contract: `INVALID_ARGUMENT` or `TOO_LARGE`.
    pub fn to_bee_error(&self) -> BeeError {
        let err = match self.kind {
            DepsErrorKind::Invalid => BeeError::new(Reason::InvalidArgument, self.to_string()),
            DepsErrorKind::TooLarge => BeeError::new(Reason::TooLarge, self.to_string())
                .with_payload("limit", "dependency_file_bytes"),
        };
        if self.path.is_empty() {
            err
        } else {
            err.with_payload("path", self.path.as_str())
        }
    }
}

impl fmt::Display for DepsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for DepsError {}

impl From<DepsError> for BeeError {
    fn from(e: DepsError) -> BeeError {
        e.to_bee_error()
    }
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, DepsError>;
