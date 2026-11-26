//! The stable error type every client-visible failure uses (SPEC §12.2).
//!
//! A [`BeeError`] carries a [`Reason`] (the contract), a human message (not the contract), a
//! `retryable` flag and an optional typed payload. `INVALID_ARGUMENT` errors name a
//! [`Subcode`] in their payload under the key `subcode`, so the JSON error document keeps the
//! four fields of `schemas/v1/error.schema.json`.

use std::collections::BTreeMap;
use std::fmt;

/// Standard gRPC status codes, with their wire numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum GrpcCode {
    /// `OK` (0).
    Ok = 0,
    /// `CANCELLED` (1).
    Cancelled = 1,
    /// `UNKNOWN` (2).
    Unknown = 2,
    /// `INVALID_ARGUMENT` (3).
    InvalidArgument = 3,
    /// `DEADLINE_EXCEEDED` (4).
    DeadlineExceeded = 4,
    /// `NOT_FOUND` (5).
    NotFound = 5,
    /// `ALREADY_EXISTS` (6).
    AlreadyExists = 6,
    /// `PERMISSION_DENIED` (7).
    PermissionDenied = 7,
    /// `RESOURCE_EXHAUSTED` (8).
    ResourceExhausted = 8,
    /// `FAILED_PRECONDITION` (9).
    FailedPrecondition = 9,
    /// `ABORTED` (10).
    Aborted = 10,
    /// `OUT_OF_RANGE` (11).
    OutOfRange = 11,
    /// `UNIMPLEMENTED` (12).
    Unimplemented = 12,
    /// `INTERNAL` (13).
    Internal = 13,
    /// `UNAVAILABLE` (14).
    Unavailable = 14,
    /// `DATA_LOSS` (15).
    DataLoss = 15,
    /// `UNAUTHENTICATED` (16).
    Unauthenticated = 16,
}

impl GrpcCode {
    /// The numeric gRPC status code.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

macro_rules! reasons {
    ($( $(#[$doc:meta])* $variant:ident => $text:literal, $code:ident, $retry:literal; )*) => {
        /// A stable, machine-readable error reason (SPEC §12.2).
        ///
        /// The string form ([`Reason::as_str`]) is part of the wire contract and never changes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Reason {
            $( $(#[$doc])* $variant, )*
        }

        impl Reason {
            /// Every reason, in declaration order.
            pub const ALL: &'static [Reason] = &[ $( Reason::$variant, )* ];

            /// The stable string form, e.g. `MERGE_CONFLICT`.
            pub fn as_str(self) -> &'static str {
                match self { $( Reason::$variant => $text, )* }
            }

            /// The gRPC status code this reason is sent with.
            pub fn grpc_code(self) -> GrpcCode {
                match self { $( Reason::$variant => GrpcCode::$code, )* }
            }

            /// Whether retrying the identical request can succeed without any other action.
            pub fn default_retryable(self) -> bool {
                match self { $( Reason::$variant => $retry, )* }
            }

            /// Parses the stable string form.
            pub fn parse(text: &str) -> Option<Reason> {
                match text { $( $text => Some(Reason::$variant), )* _ => None }
            }
        }
    };
}

reasons! {
    /// The request is malformed; the payload's `subcode` says how.
    InvalidArgument => "INVALID_ARGUMENT", InvalidArgument, false;
    /// A named thing (ref, repo, revision) does not exist or is not visible.
    NotFound => "NOT_FOUND", NotFound, false;
    /// An object id the request depends on is absent or unreadable (SPEC §7.2).
    MissingObject => "MISSING_OBJECT", FailedPrecondition, false;
    /// An abbreviated id matches more than one object.
    AmbiguousPrefix => "AMBIGUOUS_PREFIX", InvalidArgument, false;
    /// The caller is authenticated but lacks the permission.
    PermissionDenied => "PERMISSION_DENIED", PermissionDenied, false;
    /// No valid credentials were presented.
    Unauthenticated => "UNAUTHENTICATED", Unauthenticated, false;
    /// Repository policy forbids the operation (SPEC §9.4).
    PolicyDenied => "POLICY_DENIED", PermissionDenied, false;
    /// A delegated token's budget is spent (SPEC §9.2).
    BudgetExhausted => "BUDGET_EXHAUSTED", ResourceExhausted, false;
    /// Throttled; retry after the indicated delay (SPEC §15.4).
    ResourceExhausted => "RESOURCE_EXHAUSTED", ResourceExhausted, true;
    /// A value exceeds a limit in SPEC §4.6.
    TooLarge => "TOO_LARGE", InvalidArgument, false;
    /// Bytes do not match the id they were sent or stored under.
    Integrity => "INTEGRITY", DataLoss, false;
    /// The object was redacted (SPEC §13).
    Redacted => "REDACTED", FailedPrecondition, false;
    /// A compare-and-swap's expected version did not match.
    VersionMismatch => "VERSION_MISMATCH", Aborted, true;
    /// The update is not a fast-forward and force was not allowed.
    NotFastForward => "NOT_FAST_FORWARD", FailedPrecondition, false;
    /// A three-way merge or rebase produced conflicts; the payload carries them.
    MergeConflict => "MERGE_CONFLICT", FailedPrecondition, false;
    /// The ChangeId already landed on the ref from a different source (SPEC §7.4).
    AlreadyLanded => "ALREADY_LANDED", AlreadyExists, false;
    /// The result would have an empty delta (SPEC §7.3).
    EmptyChange => "EMPTY_CHANGE", FailedPrecondition, false;
    /// A merge has several best merge bases (SPEC §7.3).
    AmbiguousMergeBase => "AMBIGUOUS_MERGE_BASE", FailedPrecondition, false;
    /// A path is exclusively leased by another principal (SPEC §11.5).
    Leased => "LEASED", Aborted, true;
    /// Land ran out of attempts under contention (SPEC §7.4).
    LandAttemptsExhausted => "LAND_ATTEMPTS_EXHAUSTED", Aborted, true;
    /// The thing being created already exists.
    AlreadyExists => "ALREADY_EXISTS", AlreadyExists, false;
    /// Content was rejected by secret scanning.
    SecretDetected => "SECRET_DETECTED", FailedPrecondition, false;
    /// The service is temporarily unavailable.
    Unavailable => "UNAVAILABLE", Unavailable, true;
    /// The request's deadline passed before it completed.
    DeadlineExceeded => "DEADLINE_EXCEEDED", DeadlineExceeded, true;
    /// The operation is not supported by this server.
    Unimplemented => "UNIMPLEMENTED", Unimplemented, false;
    /// An internal invariant failed; a bug.
    Internal => "INTERNAL", Internal, false;
    /// Rebase of a multi-parent changeset (SPEC §7.3).
    MultiParent => "MULTI_PARENT", FailedPrecondition, false;
    /// A merge changeset in a stack the land method cannot take (SPEC §7.4).
    MergeInStack => "MERGE_IN_STACK", FailedPrecondition, false;
    /// The computed root tree differs from the request's `expected_root_tree` (SPEC §7.2).
    RootTreeMismatch => "ROOT_TREE_MISMATCH", FailedPrecondition, false;
    /// The token is past its expiry (SPEC §9.2).
    TokenExpired => "TOKEN_EXPIRED", Unauthenticated, false;
    /// The token or one it was derived from was revoked (SPEC §9.2).
    TokenRevoked => "TOKEN_REVOKED", Unauthenticated, false;
    /// A quota or spending limit was reached (SPEC §15.4).
    QuotaExceeded => "QUOTA_EXCEEDED", ResourceExhausted, false;
    /// The land attempt budget ran out under contention (SPEC §7.4, §12.2).
    LandContention => "LAND_CONTENTION", Unavailable, true;
    /// The caller cancelled the request.
    Cancelled => "CANCELLED", Cancelled, true;
    /// Unredact or purge of an object that is not in quarantine (SPEC §13).
    NotQuarantined => "NOT_QUARANTINED", FailedPrecondition, false;
    /// The object kind cannot be redacted (SPEC §13).
    NotRedactable => "NOT_REDACTABLE", FailedPrecondition, false;
    /// The object is in the wrong state for the operation.
    InvalidState => "INVALID_STATE", FailedPrecondition, false;
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! subcodes {
    ($( $(#[$doc:meta])* $variant:ident => $text:literal; )*) => {
        /// Refines `INVALID_ARGUMENT`; carried in the payload under `subcode`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Subcode {
            $( $(#[$doc])* $variant, )*
        }

        impl Subcode {
            /// Every subcode, in declaration order.
            pub const ALL: &'static [Subcode] = &[ $( Subcode::$variant, )* ];

            /// The stable string form.
            pub fn as_str(self) -> &'static str {
                match self { $( Subcode::$variant => $text, )* }
            }

            /// Parses the stable string form.
            pub fn parse(text: &str) -> Option<Subcode> {
                match text { $( $text => Some(Subcode::$variant), )* _ => None }
            }
        }
    };
}

subcodes! {
    /// A path breaks SPEC §4.5.
    InvalidPath => "INVALID_PATH";
    /// A tree entry name breaks SPEC §4.2.
    InvalidName => "INVALID_NAME";
    /// A ref name breaks the SPEC §5 grammar.
    InvalidRefName => "INVALID_REF_NAME";
    /// A revision expression does not parse.
    InvalidRevision => "INVALID_REVISION";
    /// An object id, ChangeId or ULID is malformed.
    InvalidId => "INVALID_ID";
    /// An object id has the wrong kind for its position.
    WrongKind => "WRONG_KIND";
    /// A mode value is unknown or not allowed here.
    InvalidMode => "INVALID_MODE";
    /// Encoded bytes are not canonical sCBOR v1 (SPEC §4).
    NonCanonical => "NON_CANONICAL";
    /// The object's `format_version` is not supported.
    UnknownFormatVersion => "UNKNOWN_FORMAT_VERSION";
    /// The object carries an odd (critical) extension key this decoder does not know.
    UnknownCriticalExtension => "UNKNOWN_CRITICAL_EXTENSION";
    /// The object has a missing, unknown or mistyped field.
    InvalidField => "INVALID_FIELD";
    /// Tree entries are unsorted, duplicated or inconsistent.
    InvalidTree => "INVALID_TREE";
    /// Delta entries are unsorted, duplicated or contradictory, or do not apply to the parent.
    InvalidDelta => "INVALID_DELTA";
    /// The delta is not the exact difference from the first parent (SPEC §4.4 invariant D).
    DeltaMismatch => "DELTA_MISMATCH";
    /// Changeset parents are duplicated or wrong.
    InvalidParents => "INVALID_PARENTS";
}

impl fmt::Display for Subcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A JSON-compatible payload value attached to an error.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PayloadValue {
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// A string.
    Str(String),
    /// A list of values.
    List(Vec<PayloadValue>),
    /// A string-keyed map (sorted, so rendering is deterministic).
    Map(BTreeMap<String, PayloadValue>),
}

impl From<&str> for PayloadValue {
    fn from(value: &str) -> Self {
        PayloadValue::Str(value.to_owned())
    }
}

impl From<String> for PayloadValue {
    fn from(value: String) -> Self {
        PayloadValue::Str(value)
    }
}

impl From<i64> for PayloadValue {
    fn from(value: i64) -> Self {
        PayloadValue::Int(value)
    }
}

impl From<bool> for PayloadValue {
    fn from(value: bool) -> Self {
        PayloadValue::Bool(value)
    }
}

/// The payload key that carries an `INVALID_ARGUMENT` [`Subcode`].
pub const SUBCODE_KEY: &str = "subcode";

/// The error every client-visible operation returns (SPEC §12.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeeError {
    /// The stable reason; what a client switches on.
    pub reason: Reason,
    /// A human-readable explanation; not part of the contract.
    pub message: String,
    /// Whether the identical request may succeed if retried.
    pub retryable: bool,
    /// Typed details; empty when there are none.
    pub payload: BTreeMap<String, PayloadValue>,
}

impl BeeError {
    /// An error with the reason's default `retryable` flag and no payload.
    pub fn new(reason: Reason, message: impl Into<String>) -> Self {
        BeeError {
            reason,
            message: message.into(),
            retryable: reason.default_retryable(),
            payload: BTreeMap::new(),
        }
    }

    /// An `INVALID_ARGUMENT` error with the given subcode.
    pub fn invalid(subcode: Subcode, message: impl Into<String>) -> Self {
        BeeError::new(Reason::InvalidArgument, message).with_payload(SUBCODE_KEY, subcode.as_str())
    }

    /// A `TOO_LARGE` error naming the limit and the offending value.
    pub fn too_large(limit: &str, max: u64, actual: u64) -> Self {
        BeeError::new(
            Reason::TooLarge,
            format!("{limit} is {actual}, above the limit of {max}"),
        )
        .with_payload("limit", limit)
        .with_payload(
            "max",
            PayloadValue::Int(i64::try_from(max).unwrap_or(i64::MAX)),
        )
        .with_payload(
            "actual",
            PayloadValue::Int(i64::try_from(actual).unwrap_or(i64::MAX)),
        )
    }

    /// Adds or replaces one payload entry.
    pub fn with_payload(mut self, key: &str, value: impl Into<PayloadValue>) -> Self {
        self.payload.insert(key.to_owned(), value.into());
        self
    }

    /// Overrides the `retryable` flag.
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// The `INVALID_ARGUMENT` subcode, if the payload names a known one.
    pub fn subcode(&self) -> Option<Subcode> {
        match self.payload.get(SUBCODE_KEY) {
            Some(PayloadValue::Str(s)) => Subcode::parse(s),
            _ => None,
        }
    }

    /// The gRPC status code for this error.
    pub fn grpc_code(&self) -> GrpcCode {
        self.reason.grpc_code()
    }

    /// True when this is `INVALID_ARGUMENT` with the given subcode.
    pub fn is_invalid(&self, subcode: Subcode) -> bool {
        self.reason == Reason::InvalidArgument && self.subcode() == Some(subcode)
    }
}

impl fmt::Display for BeeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.subcode() {
            Some(sub) if self.reason == Reason::InvalidArgument => {
                write!(f, "{} ({}): {}", self.reason, sub, self.message)
            }
            _ => write!(f, "{}: {}", self.reason, self.message),
        }
    }
}

impl std::error::Error for BeeError {}

/// Shorthand for results whose error is [`BeeError`].
pub type Result<T, E = BeeError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn reason_strings_are_unique_and_round_trip() {
        let mut seen = BTreeSet::new();
        for &r in Reason::ALL {
            assert!(seen.insert(r.as_str()), "duplicate {r}");
            assert_eq!(Reason::parse(r.as_str()), Some(r));
            assert!(
                r.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_')
            );
            assert_eq!(r.to_string(), r.as_str());
        }
        assert_eq!(Reason::parse("NOPE"), None);
    }

    #[test]
    fn spec_reasons_are_all_present() {
        for name in [
            "MISSING_OBJECT",
            "MERGE_CONFLICT",
            "ALREADY_LANDED",
            "EMPTY_CHANGE",
            "AMBIGUOUS_MERGE_BASE",
            "LEASED",
            "POLICY_DENIED",
            "BUDGET_EXHAUSTED",
            "VERSION_MISMATCH",
            "REDACTED",
            "NOT_FOUND",
            "RESOURCE_EXHAUSTED",
            "PERMISSION_DENIED",
            "INVALID_ARGUMENT",
            "UNAUTHENTICATED",
            "TOO_LARGE",
            "INTEGRITY",
            "NOT_FAST_FORWARD",
            "UNAVAILABLE",
            "AMBIGUOUS_PREFIX",
            "SECRET_DETECTED",
            "MULTI_PARENT",
            "MERGE_IN_STACK",
            "ROOT_TREE_MISMATCH",
            "TOKEN_EXPIRED",
            "TOKEN_REVOKED",
            "QUOTA_EXCEEDED",
            "LAND_CONTENTION",
            "CANCELLED",
            "NOT_QUARANTINED",
            "NOT_REDACTABLE",
            "INVALID_STATE",
        ] {
            assert!(Reason::parse(name).is_some(), "{name} missing");
        }
    }

    #[test]
    fn grpc_mapping_is_stable() {
        assert_eq!(Reason::InvalidArgument.grpc_code().as_u8(), 3);
        assert_eq!(Reason::NotFound.grpc_code().as_u8(), 5);
        assert_eq!(Reason::AlreadyLanded.grpc_code(), GrpcCode::AlreadyExists);
        assert_eq!(Reason::VersionMismatch.grpc_code(), GrpcCode::Aborted);
        assert_eq!(Reason::Unauthenticated.grpc_code().as_u8(), 16);
        assert_eq!(Reason::ResourceExhausted.grpc_code().as_u8(), 8);
        assert_eq!(Reason::Integrity.grpc_code(), GrpcCode::DataLoss);
        assert_eq!(GrpcCode::Ok.as_u8(), 0);
        for &r in Reason::ALL {
            assert_ne!(r.grpc_code(), GrpcCode::Ok);
        }
    }

    #[test]
    fn retryable_defaults() {
        assert!(Reason::Unavailable.default_retryable());
        assert!(Reason::VersionMismatch.default_retryable());
        assert!(!Reason::MergeConflict.default_retryable());
        assert!(!BeeError::new(Reason::NotFound, "x").retryable);
        assert!(
            BeeError::new(Reason::NotFound, "x")
                .with_retryable(true)
                .retryable
        );
    }

    #[test]
    fn subcodes_round_trip() {
        let mut seen = BTreeSet::new();
        for &s in Subcode::ALL {
            assert!(seen.insert(s.as_str()));
            assert_eq!(Subcode::parse(s.as_str()), Some(s));
            assert_eq!(s.to_string(), s.as_str());
        }
        assert_eq!(Subcode::parse("x"), None);
    }

    #[test]
    fn invalid_carries_subcode_in_payload() {
        let e = BeeError::invalid(Subcode::InvalidPath, "bad path");
        assert_eq!(e.reason, Reason::InvalidArgument);
        assert_eq!(e.subcode(), Some(Subcode::InvalidPath));
        assert!(e.is_invalid(Subcode::InvalidPath));
        assert!(!e.is_invalid(Subcode::InvalidName));
        assert_eq!(e.grpc_code(), GrpcCode::InvalidArgument);
        assert_eq!(e.to_string(), "INVALID_ARGUMENT (INVALID_PATH): bad path");
        assert_eq!(
            e.payload.get(SUBCODE_KEY),
            Some(&PayloadValue::Str("INVALID_PATH".into()))
        );
    }

    #[test]
    fn display_and_payload_helpers() {
        let e = BeeError::new(Reason::Leased, "held")
            .with_payload("path", String::from("a/b"))
            .with_payload("count", 2i64)
            .with_payload("exclusive", true);
        assert_eq!(e.to_string(), "LEASED: held");
        assert_eq!(e.subcode(), None);
        assert_eq!(e.payload.len(), 3);
        let t = BeeError::too_large("message", 10, 11);
        assert_eq!(t.reason, Reason::TooLarge);
        assert_eq!(t.payload.get("actual"), Some(&PayloadValue::Int(11)));
        let big = BeeError::too_large("x", u64::MAX, u64::MAX);
        assert_eq!(big.payload.get("max"), Some(&PayloadValue::Int(i64::MAX)));
        let err: &dyn std::error::Error = &e;
        assert!(err.source().is_none());
        let list = PayloadValue::List(vec![PayloadValue::Map(BTreeMap::new())]);
        assert_ne!(list, PayloadValue::Bool(false));
    }
}
