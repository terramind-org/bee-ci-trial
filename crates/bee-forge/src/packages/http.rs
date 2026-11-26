//! Small HTTP helpers shared by the registry handlers: building responses, reading
//! credentials, percent coding, query strings, status mapping.

use bee_proto::v1::ErrorReason;
use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};

use super::hash::b64_decode;
use crate::error::ForgeError;

pub(crate) type Req = Request<Bytes>;
pub(crate) type Resp = Response<Bytes>;

/// A response with headers and a body.
pub(crate) fn resp(status: u16, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Resp {
    let mut b = Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    for (k, v) in headers {
        if let Ok(v) = HeaderValue::from_str(v) {
            b = b.header(*k, v);
        }
    }
    b.body(body.into())
        .unwrap_or_else(|_| Response::new(Bytes::new()))
}

/// A JSON response.
pub(crate) fn json(status: u16, v: &serde_json::Value) -> Resp {
    resp(
        status,
        &[("content-type", "application/json")],
        v.to_string().into_bytes(),
    )
}

/// An empty response.
pub(crate) fn empty(status: u16) -> Resp {
    resp(status, &[], Bytes::new())
}

/// A request header, empty when absent or not text.
pub(crate) fn header<'a>(req: &'a Req, name: &str) -> &'a str {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// The HTTP status of a forge error.
pub(crate) fn status_of(e: &ForgeError) -> u16 {
    match e.reason {
        ErrorReason::Unauthenticated | ErrorReason::TokenExpired | ErrorReason::TokenRevoked => 401,
        ErrorReason::PermissionDenied
        | ErrorReason::PolicyDenied
        | ErrorReason::Blocked
        | ErrorReason::SsoRequired
        | ErrorReason::ApprovalRequired => 403,
        ErrorReason::NotFound | ErrorReason::Redacted => 404,
        ErrorReason::AlreadyExists
        | ErrorReason::InvalidState
        | ErrorReason::VersionMismatch
        | ErrorReason::Archived => 409,
        ErrorReason::TooLarge => 413,
        ErrorReason::QuotaExceeded
        | ErrorReason::BudgetExhausted
        | ErrorReason::ResourceExhausted => 429,
        ErrorReason::InvalidArgument | ErrorReason::Integrity | ErrorReason::MissingObject => 400,
        ErrorReason::Unavailable => 503,
        ErrorReason::DeadlineExceeded => 504,
        ErrorReason::Unimplemented => 501,
        _ => 500,
    }
}

/// The credential a registry client sent: a bearer token, the password (or, with an empty
/// password, the user name) of HTTP Basic, an `X-NuGet-ApiKey`, or the bare token Cargo and
/// RubyGems put in `Authorization`.
pub(crate) fn credential(req: &Req) -> Option<String> {
    let key = header(req, "x-nuget-apikey").trim();
    if !key.is_empty() {
        return Some(key.to_string());
    }
    let a = header(req, "authorization").trim();
    if a.is_empty() {
        return None;
    }
    let lower = a.to_ascii_lowercase();
    if lower.starts_with("bearer ") {
        return Some(a[7..].trim().to_string());
    }
    if lower.starts_with("token ") {
        return Some(a[6..].trim().to_string());
    }
    if lower.starts_with("basic ") {
        let raw = String::from_utf8(b64_decode(a[6..].trim())?).ok()?;
        let (user, pass) = raw.split_once(':').unwrap_or((&raw, ""));
        return Some(if pass.is_empty() { user } else { pass }.to_string())
            .filter(|s| !s.is_empty());
    }
    Some(a.to_string())
}

/// The tonic request carrying `token`, for `Authenticator::authenticate`.
pub(crate) fn tonic_request(req: &Req, token: &str) -> Result<tonic::Request<()>, ForgeError> {
    let mut r = tonic::Request::new(());
    let bearer = format!("Bearer {token}").parse().map_err(|_| {
        ForgeError::new(
            ErrorReason::Unauthenticated,
            "the credential is not a token",
        )
    })?;
    r.metadata_mut().insert("authorization", bearer);
    for (from, to) in [
        ("x-request-id", "x-request-id"),
        ("x-forwarded-for", "x-forwarded-for"),
        ("user-agent", "user-agent"),
    ] {
        let v = header(req, from);
        if !v.is_empty()
            && let Ok(v) = v.parse()
        {
            r.metadata_mut().insert(to, v);
        }
    }
    Ok(r)
}

/// Percent-decodes a URL path or query component (`+` is left alone).
pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decoded `key=value` pairs of the query string.
pub(crate) fn query(req: &Req) -> Vec<(String, String)> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                pct_decode(&k.replace('+', " ")),
                pct_decode(&v.replace('+', " ")),
            )
        })
        .collect()
}

/// The first value of query key `k`.
pub(crate) fn query_get(req: &Req, k: &str) -> Option<String> {
    query(req).into_iter().find(|(x, _)| x == k).map(|(_, v)| v)
}

/// The raw (still percent-encoded) path segments after `prefix`, without empty ones.
pub(crate) fn segments<'a>(path: &'a str, prefix: &str) -> Vec<&'a str> {
    path.strip_prefix(prefix)
        .unwrap_or(path)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` of milliseconds since the epoch.
pub(crate) fn iso8601(ms: u64) -> String {
    let day = (ms / 86_400_000) as i64;
    let (y, m, d) = crate::community::tz::civil(day);
    let rem = ms % 86_400_000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(h: &[(&str, &str)]) -> Req {
        let mut b = Request::builder().uri("/x?a=1&b=two%20words&c");
        for (k, v) in h {
            b = b.header(*k, *v);
        }
        b.body(Bytes::new()).unwrap()
    }

    #[test]
    fn credentials_come_from_every_scheme() {
        assert_eq!(
            credential(&req(&[("authorization", "Bearer abc")])).as_deref(),
            Some("abc")
        );
        assert_eq!(
            credential(&req(&[("authorization", "Basic dXNlcjpwdw==")])).as_deref(),
            Some("pw")
        );
        assert_eq!(
            credential(&req(&[("authorization", "Basic dG9rOg==")])).as_deref(),
            Some("tok")
        );
        assert_eq!(
            credential(&req(&[("authorization", "rawtoken")])).as_deref(),
            Some("rawtoken")
        );
        assert_eq!(
            credential(&req(&[("x-nuget-apikey", "k")])).as_deref(),
            Some("k")
        );
        assert_eq!(credential(&req(&[])), None);
    }

    #[test]
    fn query_and_percent() {
        let r = req(&[]);
        assert_eq!(query_get(&r, "b").as_deref(), Some("two words"));
        assert_eq!(query_get(&r, "c").as_deref(), Some(""));
        assert_eq!(pct_decode("%40scope%2fpkg"), "@scope/pkg");
    }
}
