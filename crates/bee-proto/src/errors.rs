//! The SPEC §12.2 error contract.
//!
//! Every failed RPC carries a `bee.v1.ErrorDetail` whose `reason` is an [`ErrorReason`] name
//! without its `ERROR_REASON_` prefix. On gRPC the detail travels in `grpc-status-details-bin`
//! as a `google.rpc.Status` detail; on the Connect HTTP/JSON surface it is a Connect error
//! detail of type `bee.v1.ErrorDetail` (see [`crate::connect`]).

use bytes::Bytes;
use prost::Message;
use tonic::Code;

use crate::v1::{ErrorDetail, ErrorReason};

/// Connect/gRPC detail type name of [`ErrorDetail`].
pub const ERROR_DETAIL_TYPE: &str = "bee.v1.ErrorDetail";

/// Prefix of `google.protobuf.Any` type URLs.
pub const TYPE_URL_PREFIX: &str = "type.googleapis.com/";

/// `google.rpc.Status`, the payload of the `grpc-status-details-bin` trailer.
#[derive(Clone, PartialEq, Message)]
pub struct RpcStatus {
    /// gRPC status code.
    #[prost(int32, tag = "1")]
    pub code: i32,
    /// Developer-facing message.
    #[prost(string, tag = "2")]
    pub message: String,
    /// Typed details, e.g. a `bee.v1.ErrorDetail`.
    #[prost(message, repeated, tag = "3")]
    pub details: Vec<pbjson_types::Any>,
}

/// How one reason maps onto transports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasonInfo {
    /// The reason.
    pub reason: ErrorReason,
    /// gRPC status code sent with it.
    pub code: Code,
    /// Whether the identical request may succeed if retried.
    pub retryable: bool,
}

impl ReasonInfo {
    /// Wire name, e.g. `MERGE_CONFLICT`.
    pub fn name(&self) -> &'static str {
        reason_name(self.reason)
    }

    /// Connect error code, e.g. `failed_precondition`.
    pub fn connect_code(&self) -> &'static str {
        connect_code_name(self.code)
    }

    /// HTTP status of a unary Connect error with this reason.
    pub fn http_status(&self) -> u16 {
        http_status(self.code)
    }
}

const fn row(reason: ErrorReason, code: Code, retryable: bool) -> ReasonInfo {
    ReasonInfo {
        reason,
        code,
        retryable,
    }
}

/// Every reason with its gRPC code and retryability. SPEC §12.2 carries the same table.
pub const REASONS: &[ReasonInfo] = &[
    row(ErrorReason::Unauthenticated, Code::Unauthenticated, false),
    row(ErrorReason::TokenExpired, Code::Unauthenticated, false),
    row(ErrorReason::TokenRevoked, Code::Unauthenticated, false),
    row(ErrorReason::PermissionDenied, Code::PermissionDenied, false),
    row(ErrorReason::PolicyDenied, Code::PermissionDenied, false),
    row(ErrorReason::BudgetExhausted, Code::ResourceExhausted, false),
    row(
        ErrorReason::ResourceExhausted,
        Code::ResourceExhausted,
        true,
    ),
    row(ErrorReason::QuotaExceeded, Code::ResourceExhausted, false),
    row(ErrorReason::NotFound, Code::NotFound, false),
    row(ErrorReason::MissingObject, Code::FailedPrecondition, false),
    row(ErrorReason::Redacted, Code::NotFound, false),
    row(ErrorReason::AlreadyExists, Code::AlreadyExists, false),
    row(ErrorReason::AlreadyLanded, Code::FailedPrecondition, false),
    row(ErrorReason::EmptyChange, Code::FailedPrecondition, false),
    row(ErrorReason::MergeConflict, Code::FailedPrecondition, false),
    row(
        ErrorReason::AmbiguousMergeBase,
        Code::FailedPrecondition,
        false,
    ),
    row(ErrorReason::Leased, Code::FailedPrecondition, false),
    row(ErrorReason::VersionMismatch, Code::Aborted, false),
    row(ErrorReason::NotFastForward, Code::FailedPrecondition, false),
    row(ErrorReason::AmbiguousPrefix, Code::InvalidArgument, false),
    row(ErrorReason::InvalidArgument, Code::InvalidArgument, false),
    row(ErrorReason::TooLarge, Code::InvalidArgument, false),
    row(ErrorReason::Integrity, Code::DataLoss, true),
    row(
        ErrorReason::RootTreeMismatch,
        Code::FailedPrecondition,
        false,
    ),
    row(ErrorReason::MultiParent, Code::FailedPrecondition, false),
    row(ErrorReason::MergeInStack, Code::FailedPrecondition, false),
    row(ErrorReason::LandContention, Code::Unavailable, true),
    row(ErrorReason::Unavailable, Code::Unavailable, true),
    row(ErrorReason::DeadlineExceeded, Code::DeadlineExceeded, true),
    row(ErrorReason::Cancelled, Code::Cancelled, true),
    row(ErrorReason::Unimplemented, Code::Unimplemented, false),
    row(ErrorReason::Internal, Code::Internal, false),
    row(ErrorReason::NotQuarantined, Code::FailedPrecondition, false),
    row(ErrorReason::NotRedactable, Code::FailedPrecondition, false),
    row(ErrorReason::SecretDetected, Code::FailedPrecondition, false),
    row(ErrorReason::Archived, Code::FailedPrecondition, false),
    row(ErrorReason::Locked, Code::FailedPrecondition, false),
    row(ErrorReason::Blocked, Code::PermissionDenied, false),
    row(
        ErrorReason::InteractionLimited,
        Code::PermissionDenied,
        false,
    ),
    row(ErrorReason::Draft, Code::FailedPrecondition, false),
    row(ErrorReason::GitDisabled, Code::FailedPrecondition, false),
    row(ErrorReason::SsoRequired, Code::PermissionDenied, false),
    row(
        ErrorReason::TwoFactorRequired,
        Code::PermissionDenied,
        false,
    ),
    row(ErrorReason::IpNotAllowed, Code::PermissionDenied, false),
    row(
        ErrorReason::ApprovalRequired,
        Code::FailedPrecondition,
        false,
    ),
    row(ErrorReason::InvalidState, Code::FailedPrecondition, false),
];

/// Wire name of a reason: the enum value name without `ERROR_REASON_`.
pub fn reason_name(reason: ErrorReason) -> &'static str {
    let full = reason.as_str_name();
    full.strip_prefix("ERROR_REASON_").unwrap_or(full)
}

/// Parses a wire name such as `MERGE_CONFLICT`.
pub fn parse_reason(name: &str) -> Option<ErrorReason> {
    ErrorReason::from_str_name(&format!("ERROR_REASON_{name}"))
        .filter(|r| *r != ErrorReason::Unspecified)
}

/// Transport mapping of a reason; `None` only for `UNSPECIFIED`.
pub fn reason_info(reason: ErrorReason) -> Option<&'static ReasonInfo> {
    REASONS.iter().find(|info| info.reason == reason)
}

/// The reason used when a status arrives without an `ErrorDetail` (for example tonic's own
/// `UNIMPLEMENTED` for an unknown method). SPEC §12.2 lists the same defaults.
pub fn default_reason(code: Code) -> ErrorReason {
    match code {
        Code::Cancelled => ErrorReason::Cancelled,
        Code::InvalidArgument | Code::OutOfRange => ErrorReason::InvalidArgument,
        Code::DeadlineExceeded => ErrorReason::DeadlineExceeded,
        Code::NotFound => ErrorReason::NotFound,
        Code::AlreadyExists => ErrorReason::AlreadyExists,
        Code::PermissionDenied => ErrorReason::PermissionDenied,
        Code::ResourceExhausted => ErrorReason::ResourceExhausted,
        Code::FailedPrecondition => ErrorReason::InvalidState,
        Code::Aborted => ErrorReason::VersionMismatch,
        Code::Unimplemented => ErrorReason::Unimplemented,
        Code::Unavailable => ErrorReason::Unavailable,
        Code::DataLoss => ErrorReason::Integrity,
        Code::Unauthenticated => ErrorReason::Unauthenticated,
        Code::Ok | Code::Unknown | Code::Internal => ErrorReason::Internal,
    }
}

/// An [`ErrorDetail`] for `reason` with `retryable` taken from the table.
pub fn error_detail(reason: ErrorReason, message: impl Into<String>) -> ErrorDetail {
    ErrorDetail {
        reason: reason_name(reason).to_owned(),
        message: message.into(),
        retryable: reason_info(reason).is_some_and(|info| info.retryable),
        ..ErrorDetail::default()
    }
}

/// A tonic status carrying `detail`, with the gRPC code the table gives its reason
/// (`UNKNOWN` for a reason this build does not know).
pub fn status_from_detail(detail: &ErrorDetail) -> tonic::Status {
    let code = parse_reason(&detail.reason)
        .and_then(reason_info)
        .map_or(Code::Unknown, |info| info.code);
    status_with_detail(code, detail)
}

/// A tonic status with an explicit code carrying `detail` in `grpc-status-details-bin`.
pub fn status_with_detail(code: Code, detail: &ErrorDetail) -> tonic::Status {
    let rpc = RpcStatus {
        code: code as i32,
        message: detail.message.clone(),
        details: vec![pbjson_types::Any {
            type_url: format!("{TYPE_URL_PREFIX}{ERROR_DETAIL_TYPE}"),
            value: detail.encode_to_vec().into(),
        }],
    };
    tonic::Status::with_details(
        code,
        detail.message.clone(),
        Bytes::from(rpc.encode_to_vec()),
    )
}

/// Shorthand for `status_from_detail(&error_detail(reason, message))`.
pub fn status(reason: ErrorReason, message: impl Into<String>) -> tonic::Status {
    status_from_detail(&error_detail(reason, message))
}

/// The `google.rpc.Status` in a tonic status's details, if any.
pub fn rpc_status(status: &tonic::Status) -> Option<RpcStatus> {
    if status.details().is_empty() {
        return None;
    }
    RpcStatus::decode(status.details()).ok()
}

/// The [`ErrorDetail`] carried by a status, if any.
pub fn detail_from_status(status: &tonic::Status) -> Option<ErrorDetail> {
    rpc_status(status)?
        .details
        .iter()
        .find(|any| any_type_name(&any.type_url) == ERROR_DETAIL_TYPE)
        .and_then(|any| ErrorDetail::decode(any.value.as_ref()).ok())
}

/// Type name of an `Any` type URL: the part after the last `/`.
pub fn any_type_name(type_url: &str) -> &str {
    type_url.rsplit('/').next().unwrap_or(type_url)
}

/// Connect error code name of a gRPC code (connectrpc.com/docs/protocol#error-codes).
pub fn connect_code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "canceled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}

/// Parses a Connect error code name. Unknown names give `None`.
pub fn code_from_connect_name(name: &str) -> Option<Code> {
    const ALL: [Code; 17] = [
        Code::Ok,
        Code::Cancelled,
        Code::Unknown,
        Code::InvalidArgument,
        Code::DeadlineExceeded,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::ResourceExhausted,
        Code::FailedPrecondition,
        Code::Aborted,
        Code::OutOfRange,
        Code::Unimplemented,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
        Code::Unauthenticated,
    ];
    ALL.into_iter().find(|c| connect_code_name(*c) == name)
}

/// HTTP status the Connect protocol assigns to a code for unary errors.
pub fn http_status(code: Code) -> u16 {
    match code {
        Code::Ok => 200,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
        Code::Unauthenticated => 401,
        Code::PermissionDenied => 403,
        Code::NotFound => 404,
        Code::AlreadyExists | Code::Aborted => 409,
        Code::ResourceExhausted => 429,
        Code::Cancelled => 499,
        Code::Unknown | Code::Internal | Code::DataLoss => 500,
        Code::Unimplemented => 501,
        Code::Unavailable => 503,
        Code::DeadlineExceeded => 504,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_has_exactly_one_row() {
        for value in 1..=i32::MAX {
            let Ok(reason) = ErrorReason::try_from(value) else {
                break;
            };
            let rows = REASONS.iter().filter(|r| r.reason == reason).count();
            assert_eq!(rows, 1, "{reason:?}");
        }
        assert!(reason_info(ErrorReason::Unspecified).is_none());
    }

    #[test]
    fn names_round_trip() {
        for info in REASONS {
            assert_eq!(parse_reason(info.name()), Some(info.reason));
        }
        assert_eq!(parse_reason("UNSPECIFIED"), None);
        assert_eq!(parse_reason("NOT_A_REASON"), None);
    }

    #[test]
    fn connect_codes_round_trip() {
        for info in REASONS {
            assert_eq!(code_from_connect_name(info.connect_code()), Some(info.code));
        }
    }

    #[test]
    fn default_reasons_use_their_own_code() {
        for code in [
            Code::Cancelled,
            Code::InvalidArgument,
            Code::DeadlineExceeded,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::ResourceExhausted,
            Code::FailedPrecondition,
            Code::Aborted,
            Code::Unimplemented,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
            Code::Unauthenticated,
        ] {
            let info = reason_info(default_reason(code)).expect("row");
            assert_eq!(info.code, code, "{code:?}");
        }
    }

    #[test]
    fn status_carries_detail() {
        let mut detail = error_detail(ErrorReason::LandContention, "busy");
        detail.retry_after_ms = 250;
        let status = status_from_detail(&detail);
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "busy");
        let back = detail_from_status(&status).expect("detail");
        assert_eq!(back, detail);
        assert!(back.retryable);
    }
}
