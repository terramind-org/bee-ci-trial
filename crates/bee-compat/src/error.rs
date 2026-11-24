//! Errors in GitHub's shape, carrying the SPEC §12.2 reason they came from.
//!
//! GitHub answers `{"message", "documentation_url", "errors"?, "status"}`; the gateway adds
//! `"bee_reason"` (the SPEC §12.2 reason name) so agents get the contract reason without
//! parsing messages. The status mapping is in `docs/spec/github-compat.md` §Errors.

use bee_proto::v1::ErrorReason;
use serde_json::{Value, json};

/// Documentation root linked from every error.
pub const DOCS: &str = "https://docs.beehive.dev/compat/github";

/// A failed request: HTTP status, GitHub's error body and the bee reason.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// HTTP status.
    pub status: u16,
    /// `message`.
    pub message: String,
    /// SPEC §12.2 reason.
    pub reason: ErrorReason,
    /// `errors` (validation details), omitted when empty.
    pub errors: Vec<Value>,
    /// Seconds to wait before retrying (`Retry-After`), when known.
    pub retry_after: Option<u64>,
}

/// The result of a handler.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    /// An error with `status`, `reason` and `message`.
    pub fn new(status: u16, reason: ErrorReason, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
            reason,
            errors: Vec::new(),
            retry_after: None,
        }
    }

    /// `404 Not Found`.
    pub fn not_found() -> ApiError {
        ApiError::new(404, ErrorReason::NotFound, "Not Found")
    }

    /// `401 Requires authentication`.
    pub fn unauthenticated() -> ApiError {
        ApiError::new(401, ErrorReason::Unauthenticated, "Requires authentication")
    }

    /// `401 Bad credentials`.
    pub fn bad_credentials() -> ApiError {
        ApiError::new(401, ErrorReason::Unauthenticated, "Bad credentials")
    }

    /// `403` with `message`.
    pub fn forbidden(message: impl Into<String>) -> ApiError {
        ApiError::new(403, ErrorReason::PermissionDenied, message)
    }

    /// `400 Problems parsing JSON` and friends.
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, ErrorReason::InvalidArgument, message)
    }

    /// `422 Validation Failed` with one GitHub validation error.
    pub fn validation(resource: &str, field: &str, code: &str, message: &str) -> ApiError {
        let mut e = ApiError::new(422, ErrorReason::InvalidArgument, "Validation Failed");
        let mut item = json!({"resource": resource, "field": field, "code": code});
        if !message.is_empty() {
            item["message"] = Value::String(message.to_string());
        }
        e.errors.push(item);
        e
    }

    /// `422 Validation Failed` for a missing field.
    pub fn missing_field(resource: &str, field: &str) -> ApiError {
        ApiError::validation(resource, field, "missing_field", "")
    }

    /// `422` with a custom message and no details (GitHub's "Reference already exists").
    pub fn unprocessable(message: impl Into<String>) -> ApiError {
        ApiError::new(422, ErrorReason::InvalidArgument, message)
    }

    /// Overrides the status, keeping message and reason.
    pub fn with_status(mut self, status: u16) -> ApiError {
        self.status = status;
        self
    }

    /// Overrides the message.
    pub fn with_message(mut self, message: impl Into<String>) -> ApiError {
        self.message = message.into();
        self
    }

    /// The JSON body.
    pub fn body(&self) -> Value {
        let mut v = json!({
            "message": self.message,
            "documentation_url": DOCS,
            "status": self.status.to_string(),
            "bee_reason": bee_proto::errors::reason_name(self.reason),
        });
        if !self.errors.is_empty() {
            v["errors"] = Value::Array(self.errors.clone());
        }
        v
    }

    /// Maps a forge `tonic::Status` (with its `ErrorDetail`) to GitHub's status and message.
    pub fn from_status(st: &tonic::Status) -> ApiError {
        let detail = bee_proto::errors::detail_from_status(st);
        let reason = detail
            .as_ref()
            .and_then(|d| bee_proto::errors::parse_reason(&d.reason))
            .unwrap_or_else(|| bee_proto::errors::default_reason(st.code()));
        let message = detail
            .as_ref()
            .map(|d| d.message.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| st.message().to_string());
        let subcode = detail
            .as_ref()
            .map(|d| d.subcode.clone())
            .unwrap_or_default();
        let retry = detail
            .as_ref()
            .map(|d| d.retry_after_ms)
            .filter(|ms| *ms > 0)
            .map(|ms| ms.div_ceil(1000));
        let mut e = ApiError::from_reason(reason, &message, &subcode);
        e.retry_after = retry;
        e
    }

    /// GitHub's status and message for a SPEC reason.
    pub fn from_reason(reason: ErrorReason, message: &str, subcode: &str) -> ApiError {
        use ErrorReason as R;
        let (status, text): (u16, String) = match reason {
            R::Unauthenticated if message.contains("missing") => {
                (401, "Requires authentication".into())
            }
            R::Unauthenticated | R::TokenExpired | R::TokenRevoked => {
                (401, "Bad credentials".into())
            }
            R::NotFound | R::Redacted => (404, "Not Found".into()),
            R::PermissionDenied | R::PolicyDenied => (403, message.to_string()),
            R::BudgetExhausted | R::QuotaExceeded => (403, message.to_string()),
            R::ResourceExhausted => (429, message.to_string()),
            R::AlreadyExists => {
                let mut e = ApiError::new(422, reason, "Validation Failed");
                e.errors
                    .push(json!({"code": "already_exists", "message": message}));
                return e;
            }
            R::InvalidArgument | R::TooLarge | R::AmbiguousPrefix => {
                let mut e = ApiError::new(422, reason, "Validation Failed");
                e.errors.push(json!({
                    "code": if subcode.is_empty() { "invalid" } else { subcode },
                    "message": message,
                }));
                return e;
            }
            R::VersionMismatch => (409, message.to_string()),
            R::MergeConflict | R::AlreadyLanded | R::EmptyChange | R::MergeInStack => {
                (405, message.to_string())
            }
            R::NotFastForward => (422, "Update is not a fast forward".into()),
            R::Leased => (409, message.to_string()),
            R::MissingObject | R::RootTreeMismatch | R::MultiParent | R::AmbiguousMergeBase => {
                (422, message.to_string())
            }
            R::Unavailable | R::LandContention => (503, message.to_string()),
            R::DeadlineExceeded => (504, message.to_string()),
            R::Cancelled => (499, message.to_string()),
            R::Unimplemented => (501, message.to_string()),
            R::Archived => (403, "Repository was archived so is read-only.".into()),
            R::Locked
            | R::Blocked
            | R::InteractionLimited
            | R::ApprovalRequired
            | R::SsoRequired
            | R::TwoFactorRequired
            | R::IpNotAllowed
            | R::GitDisabled => (403, message.to_string()),
            R::Draft => (405, "Pull Request is still a draft".into()),
            R::InvalidState | R::SecretDetected | R::NotQuarantined | R::NotRedactable => {
                (422, message.to_string())
            }
            _ => (500, message.to_string()),
        };
        ApiError::new(status, reason, text)
    }
}

impl From<tonic::Status> for ApiError {
    fn from(st: tonic::Status) -> ApiError {
        ApiError::from_status(&st)
    }
}

impl From<bee_forge::ForgeError> for ApiError {
    fn from(e: bee_forge::ForgeError) -> ApiError {
        ApiError::from_reason(e.reason, &e.message, &e.subcode)
    }
}

impl From<bee_core::BeeError> for ApiError {
    fn from(e: bee_core::BeeError) -> ApiError {
        let reason =
            bee_proto::errors::parse_reason(e.reason.as_str()).unwrap_or(ErrorReason::Internal);
        ApiError::from_reason(reason, &e.message, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_github() {
        let s = bee_proto::errors::status(ErrorReason::NotFound, "repo x");
        let e = ApiError::from(s);
        assert_eq!(e.status, 404);
        assert_eq!(e.body()["message"], "Not Found");
        assert_eq!(e.body()["bee_reason"], "NOT_FOUND");
        let e = ApiError::from(bee_proto::errors::status(
            ErrorReason::AlreadyExists,
            "name taken",
        ));
        assert_eq!(e.status, 422);
        assert_eq!(e.body()["errors"][0]["code"], "already_exists");
        let e = ApiError::from(bee_proto::errors::status(
            ErrorReason::Unauthenticated,
            "missing bearer token",
        ));
        assert_eq!(
            (e.status, e.message.as_str()),
            (401, "Requires authentication")
        );
    }
}
