//! Typed policy errors with locations, and the reason codes evaluator decisions carry.

use std::fmt;

/// Which input an error points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The org-level default policy stored server-side.
    Org,
    /// `.beehive/policy.toml` at the tip of the target ref.
    Repo,
    /// A `CODEOWNERS` file.
    Codeowners,
    /// The merged (effective) policy; used for errors that only exist after overlaying.
    Effective,
}

impl Origin {
    /// Short name used in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Org => "org policy",
            Origin::Repo => ".beehive/policy.toml",
            Origin::Codeowners => "CODEOWNERS",
            Origin::Effective => "effective policy",
        }
    }
}

/// Where an error is: the input, a TOML key path such as `access[2].path`, and the 1-based
/// line and column when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Which input.
    pub origin: Origin,
    /// TOML key path (empty for the whole document).
    pub key: String,
    /// 1-based line.
    pub line: Option<u32>,
    /// 1-based column, in characters.
    pub column: Option<u32>,
}

impl Location {
    /// A location with a key path and no line information.
    pub fn key(origin: Origin, key: impl Into<String>) -> Location {
        Location {
            origin,
            key: key.into(),
            line: None,
            column: None,
        }
    }

    /// A location computed from a byte span into `source`.
    pub fn span(origin: Origin, key: impl Into<String>, source: &str, offset: usize) -> Location {
        let (line, column) = line_col(source, offset);
        Location {
            origin,
            key: key.into(),
            line: Some(line),
            column: Some(column),
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.origin.as_str())?;
        if let (Some(l), Some(c)) = (self.line, self.column) {
            write!(f, ":{l}:{c}")?;
        }
        if !self.key.is_empty() {
            write!(f, " ({})", self.key)?;
        }
        Ok(())
    }
}

fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let before = source.get(..offset).unwrap_or(source);
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |s| s.chars().count()) + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(col).unwrap_or(u32::MAX),
    )
}

/// What is wrong with a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyErrorKind {
    /// Not valid TOML, an unknown key, or a value of the wrong type.
    Parse,
    /// `version` is missing or not 1.
    UnsupportedVersion,
    /// A path prefix is malformed.
    InvalidPath,
    /// A glob or ref pattern is malformed.
    InvalidPattern,
    /// A subject is malformed.
    InvalidSubject,
    /// A `@group` reference names a policy group that is not defined.
    UnknownGroup,
    /// Groups include each other in a cycle.
    GroupCycle,
    /// A role name is not built in and not defined.
    UnknownRole,
    /// Custom roles extend each other in a cycle.
    RoleCycle,
    /// A built-in name was redefined.
    ReservedName,
    /// There is no access rule for `/` defining both `read` and `write`.
    MissingRootRule,
    /// Two rules have the same key.
    Duplicate,
    /// Any other invalid value (unknown action or class, zero count, bad CODEOWNERS line).
    InvalidValue,
}

impl PolicyErrorKind {
    /// Stable snake-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyErrorKind::Parse => "parse",
            PolicyErrorKind::UnsupportedVersion => "unsupported_version",
            PolicyErrorKind::InvalidPath => "invalid_path",
            PolicyErrorKind::InvalidPattern => "invalid_pattern",
            PolicyErrorKind::InvalidSubject => "invalid_subject",
            PolicyErrorKind::UnknownGroup => "unknown_group",
            PolicyErrorKind::GroupCycle => "group_cycle",
            PolicyErrorKind::UnknownRole => "unknown_role",
            PolicyErrorKind::RoleCycle => "role_cycle",
            PolicyErrorKind::ReservedName => "reserved_name",
            PolicyErrorKind::MissingRootRule => "missing_root_rule",
            PolicyErrorKind::Duplicate => "duplicate",
            PolicyErrorKind::InvalidValue => "invalid_value",
        }
    }
}

/// An invalid policy. Clients see it as reason [`Reason::InvalidPolicy`].
///
/// The evaluator never falls back on its own: keeping the last good policy when a new one is
/// invalid is the caller's job (SPEC §9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    /// What is wrong.
    pub kind: PolicyErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// Where.
    pub location: Location,
}

impl PolicyError {
    pub(crate) fn new(
        kind: PolicyErrorKind,
        message: impl Into<String>,
        location: Location,
    ) -> Self {
        PolicyError {
            kind,
            message: message.into(),
            location,
        }
    }

    /// The client-visible reason code.
    pub fn reason(&self) -> Reason {
        Reason::InvalidPolicy
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} [{}]",
            self.location,
            self.message,
            self.kind.as_str()
        )
    }
}

impl std::error::Error for PolicyError {}

/// SPEC §12.2 reason codes produced by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// Policy does not allow the request.
    PolicyDenied,
    /// A path is exclusively leased by someone else (SPEC §11.5).
    Leased,
    /// A policy file is invalid.
    InvalidPolicy,
}

impl Reason {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::PolicyDenied => "POLICY_DENIED",
            Reason::Leased => "LEASED",
            Reason::InvalidPolicy => "INVALID_POLICY",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column() {
        let src = "a = 1\nbb = 2\n";
        assert_eq!(line_col(src, 0), (1, 1));
        assert_eq!(line_col(src, 6), (2, 1));
        assert_eq!(line_col(src, 9), (2, 4));
        let loc = Location::span(Origin::Repo, "bb", src, 6);
        assert_eq!(loc.to_string(), ".beehive/policy.toml:2:1 (bb)");
    }
}
