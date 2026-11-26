//! Workflow errors and their mapping to the SPEC §12.2 contract.

use std::fmt;

use bee_core::{BeeError, PayloadValue, Subcode};

use crate::yaml::Pos;

/// One problem found in a workflow file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    /// Where the problem is (line and column of the offending node).
    pub pos: Pos,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.pos, self.message)
    }
}

/// What kind of failure a [`WorkflowError`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The file is not a valid workflow, or an expression failed (`INVALID_ARGUMENT`).
    Invalid,
    /// A count limit was exceeded (`TOO_LARGE`).
    TooLarge {
        /// The limit's name, e.g. `matrix_jobs`.
        limit: String,
        /// The maximum allowed.
        max: u64,
        /// The count found.
        actual: u64,
    },
}

/// A workflow could not be parsed, validated or planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowError {
    /// The workflow file path.
    pub path: String,
    /// The kind of failure.
    pub kind: ErrorKind,
    /// Every problem found, sorted by position.
    pub diagnostics: Vec<Diagnostic>,
}

impl WorkflowError {
    /// An invalid-workflow error with one diagnostic.
    pub fn invalid(path: &str, pos: Pos, message: impl Into<String>) -> WorkflowError {
        WorkflowError {
            path: path.to_owned(),
            kind: ErrorKind::Invalid,
            diagnostics: vec![Diagnostic {
                pos,
                message: message.into(),
            }],
        }
    }

    /// The first diagnostic.
    pub fn first(&self) -> Option<&Diagnostic> {
        self.diagnostics.first()
    }

    /// Converts to the client-visible error: `INVALID_ARGUMENT` with subcode `INVALID_FIELD`,
    /// or `TOO_LARGE`, carrying `path`, `line`, `column` and every diagnostic.
    pub fn to_bee_error(&self) -> BeeError {
        let base = match &self.kind {
            ErrorKind::Invalid => BeeError::invalid(Subcode::InvalidField, self.to_string()),
            ErrorKind::TooLarge { limit, max, actual } => {
                let mut e = BeeError::too_large(limit, *max, *actual);
                e.message = self.to_string();
                e
            }
        };
        let mut e = base.with_payload("path", self.path.as_str());
        if let Some(d) = self.first() {
            e = e
                .with_payload("line", i64::from(d.pos.line))
                .with_payload("column", i64::from(d.pos.col));
        }
        let list = self
            .diagnostics
            .iter()
            .map(|d| PayloadValue::Str(format!("{}:{}: {}", self.path, d.pos, d.message)))
            .collect();
        e.with_payload("diagnostics", PayloadValue::List(list))
    }
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.diagnostics.first() {
            Some(d) => {
                write!(f, "{}:{}: {}", self.path, d.pos, d.message)?;
                if self.diagnostics.len() > 1 {
                    write!(f, " (and {} more)", self.diagnostics.len() - 1)?;
                }
                Ok(())
            }
            None => write!(f, "{}: invalid workflow", self.path),
        }
    }
}

impl std::error::Error for WorkflowError {}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::Reason;

    #[test]
    fn maps_to_contract_reasons() {
        let e = WorkflowError::invalid(".github/workflows/ci.yml", Pos { line: 3, col: 5 }, "bad");
        let s = e.to_bee_error();
        assert!(s.is_invalid(Subcode::InvalidField));
        assert_eq!(s.payload.get("line"), Some(&PayloadValue::Int(3)));
        assert_eq!(s.payload.get("column"), Some(&PayloadValue::Int(5)));
        assert_eq!(s.message, ".github/workflows/ci.yml:3:5: bad");
        let t = WorkflowError {
            path: "w.yml".into(),
            kind: ErrorKind::TooLarge {
                limit: "matrix_jobs".into(),
                max: 256,
                actual: 300,
            },
            diagnostics: vec![Diagnostic {
                pos: Pos { line: 1, col: 1 },
                message: "too many".into(),
            }],
        };
        assert_eq!(t.to_bee_error().reason, Reason::TooLarge);
    }
}
