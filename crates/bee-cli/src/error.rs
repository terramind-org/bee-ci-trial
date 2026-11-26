//! CLI errors: a SPEC §12.2 reason, a message, `retryable`, a payload, and the fixed exit code.

use bee_proto::connect::ConnectError;
use bee_proto::errors::{self, default_reason, parse_reason, reason_info};
use bee_proto::v1::{ErrorDetail, ErrorReason};
use serde_json::{Map, Value, json};

/// The fixed process exit codes (SPEC §12.4, PRODUCT §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCode {
    /// Success.
    Ok = 0,
    /// Any error without a more specific code.
    Error = 1,
    /// Bad flags, arguments or input; nothing was sent.
    Usage = 2,
    /// The operation produced conflicts (`MERGE_CONFLICT`).
    Conflicts = 3,
    /// Refused because the working copy has uncommitted changes.
    DirtyRefusal = 4,
    /// Refused by policy: `POLICY_DENIED`, `LEASED`, `SECRET_DETECTED`, `BUDGET_EXHAUSTED`.
    PolicyDenied = 5,
}

impl ExitCode {
    /// Every code with a one-line meaning, for help and the agent index.
    pub const TABLE: [(ExitCode, &'static str); 6] = [
        (ExitCode::Ok, "ok"),
        (ExitCode::Error, "error (see the error document)"),
        (
            ExitCode::Usage,
            "usage: bad flags or input, nothing was sent",
        ),
        (ExitCode::Conflicts, "conflicts (MERGE_CONFLICT)"),
        (
            ExitCode::DirtyRefusal,
            "refused: working copy has uncommitted changes",
        ),
        (
            ExitCode::PolicyDenied,
            "policy denied (POLICY_DENIED, LEASED, SECRET_DETECTED, BUDGET_EXHAUSTED)",
        ),
    ];

    /// The numeric code.
    pub fn code(self) -> i32 {
        self as i32
    }

    /// A one-word label: `ok`, `error`, `usage`, `conflicts`, `dirty`, `policy-denied`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Usage => "usage",
            Self::Conflicts => "conflicts",
            Self::DirtyRefusal => "dirty",
            Self::PolicyDenied => "policy-denied",
        }
    }

    /// The exit code of a server-reported reason.
    pub fn for_reason(reason: &str) -> ExitCode {
        match reason {
            "MERGE_CONFLICT" => ExitCode::Conflicts,
            "POLICY_DENIED" | "LEASED" | "SECRET_DETECTED" | "BUDGET_EXHAUSTED" => {
                ExitCode::PolicyDenied
            }
            _ => ExitCode::Error,
        }
    }
}

/// A failed command, as written to the error document.
#[derive(Clone, Debug, PartialEq)]
pub struct CliError {
    /// SPEC §12.2 reason name, e.g. `MERGE_CONFLICT`.
    pub reason: String,
    /// Human-readable message.
    pub message: String,
    /// Whether the identical request may succeed if retried.
    pub retryable: bool,
    /// Typed payload: `subcode`, `retryAfterMs`, `requestId` and the reason's payload field.
    pub payload: Map<String, Value>,
    /// Process exit code.
    pub exit: ExitCode,
}

impl CliError {
    /// An error for `reason` with `retryable` from the SPEC table and the reason's exit code.
    pub fn new(reason: ErrorReason, message: impl Into<String>) -> Self {
        let name = errors::reason_name(reason).to_owned();
        Self {
            exit: ExitCode::for_reason(&name),
            retryable: reason_info(reason).is_some_and(|info| info.retryable),
            reason: name,
            message: message.into(),
            payload: Map::new(),
        }
    }

    /// A usage error (`INVALID_ARGUMENT`, exit 2) with a snake_case subcode.
    pub fn usage(subcode: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorReason::InvalidArgument, message)
            .with_exit(ExitCode::Usage)
            .with_payload("subcode", json!(subcode))
    }

    /// Refusal because the working copy is dirty (`INVALID_STATE`, exit 4).
    pub fn dirty(message: impl Into<String>) -> Self {
        Self::new(ErrorReason::InvalidState, message)
            .with_exit(ExitCode::DirtyRefusal)
            .with_payload("subcode", json!("dirty_working_copy"))
    }

    /// The endpoint could not be reached (`UNAVAILABLE`, retryable).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorReason::Unavailable, message)
    }

    /// A client-side failure that is a bug or a broken response (`INTERNAL`).
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorReason::Internal, message)
    }

    /// Replaces the exit code.
    pub fn with_exit(mut self, exit: ExitCode) -> Self {
        self.exit = exit;
        self
    }

    /// Adds one payload entry.
    pub fn with_payload(mut self, key: &str, value: Value) -> Self {
        self.payload.insert(key.to_owned(), value);
        self
    }

    /// The error a server reported in a `bee.v1.ErrorDetail`.
    pub fn from_detail(detail: &ErrorDetail, fallback_message: &str) -> Self {
        let mut payload = match serde_json::to_value(detail) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        for key in ["reason", "message", "retryable"] {
            payload.remove(key);
        }
        let message = if detail.message.is_empty() {
            fallback_message.to_owned()
        } else {
            detail.message.clone()
        };
        let reason = if detail.reason.is_empty() {
            "INTERNAL".to_owned()
        } else {
            detail.reason.clone()
        };
        Self {
            exit: ExitCode::for_reason(&reason),
            reason,
            message,
            retryable: detail.retryable,
            payload,
        }
    }

    /// The error in a Connect error body; without a detail, the default reason of its code.
    pub fn from_connect(error: &ConnectError) -> Self {
        match error.error_detail() {
            Some(detail) => Self::from_detail(&detail, &error.message),
            None => Self::new(default_reason(error.grpc_code()), error.message.clone()),
        }
    }

    /// The error for a non-Connect HTTP failure, from the status code alone.
    pub fn from_http_status(status: u16, body: &str) -> Self {
        let reason = match status {
            400 => ErrorReason::InvalidArgument,
            401 => ErrorReason::Unauthenticated,
            403 => ErrorReason::PermissionDenied,
            404 => ErrorReason::Unimplemented,
            408 | 504 => ErrorReason::DeadlineExceeded,
            409 => ErrorReason::AlreadyExists,
            429 | 502 | 503 => ErrorReason::Unavailable,
            _ => ErrorReason::Internal,
        };
        let snippet: String = body.chars().take(200).collect();
        Self::new(reason, format!("HTTP {status} from server: {snippet}"))
    }

    /// Whether `reason` names a reason this build knows.
    pub fn is_known_reason(&self) -> bool {
        parse_reason(&self.reason).is_some()
    }

    /// The `error` object of the error document.
    pub fn to_json(&self) -> Value {
        json!({
            "reason": self.reason,
            "message": self.message,
            "retryable": self.retryable,
            "payload": Value::Object(self.payload.clone()),
        })
    }

    /// The whole error document: `{"schema": "bee.v1.error", "error": {...}}`.
    pub fn document(&self) -> Value {
        json!({"schema": "bee.v1.error", "error": self.to_json()})
    }

    /// Human-readable rendering for stderr.
    pub fn render_human(&self) -> String {
        let mut out = format!("error: {}: {}", self.reason, self.message);
        if self.retryable {
            out.push_str(" (retryable)");
        }
        for (key, value) in &self.payload {
            let shown = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str(&format!("\n  {key}: {shown}"));
        }
        out
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_reasons() {
        assert_eq!(ExitCode::for_reason("MERGE_CONFLICT"), ExitCode::Conflicts);
        for reason in [
            "POLICY_DENIED",
            "LEASED",
            "SECRET_DETECTED",
            "BUDGET_EXHAUSTED",
        ] {
            assert_eq!(ExitCode::for_reason(reason), ExitCode::PolicyDenied);
        }
        assert_eq!(ExitCode::for_reason("NOT_FOUND"), ExitCode::Error);
        assert_eq!(CliError::usage("x", "y").exit.code(), 2);
        assert_eq!(CliError::dirty("y").exit.code(), 4);
    }

    #[test]
    fn detail_payload_keeps_typed_fields() {
        let mut detail = errors::error_detail(ErrorReason::ResourceExhausted, "slow down");
        detail.retry_after_ms = 250;
        detail.subcode = "throttled".into();
        let error = CliError::from_detail(&detail, "");
        assert_eq!(error.reason, "RESOURCE_EXHAUSTED");
        assert!(error.retryable);
        assert_eq!(error.payload["subcode"], "throttled");
        assert!(error.payload.contains_key("retryAfterMs"));
        assert!(!error.payload.contains_key("reason"));
        assert!(error.is_known_reason());
    }
}
