//! How media is served: the headers every response carries and `Range` requests.

/// Headers every media response carries: the browser must not guess another type, and a
/// response opened as a document runs nothing (sandboxed, no sources).
pub const SAFETY_HEADERS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("content-security-policy", "sandbox; default-src 'none'"),
    ("cross-origin-resource-policy", "cross-origin"),
    ("accept-ranges", "bytes"),
];

/// The `Content-Disposition` of a response of `kind`: `None` for what a page may show
/// (images, video, audio), `attachment` for everything else (PDF, text, captions), so a
/// document is downloaded rather than opened in the forge's origin.
pub fn content_disposition(kind: super::Kind) -> Option<&'static str> {
    (!kind.is_inline()).then_some("attachment")
}

/// `Cache-Control` of a media response: private (the URL decides access), briefly cached.
pub const CACHE_CONTROL: &str = "private, max-age=300";

/// What a `Range` header asks of a body of `len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeAsk {
    /// No range, or one the server ignores (several ranges, other units): the whole body.
    Full,
    /// Bytes `start..=end`.
    Part(u64, u64),
    /// A range outside the body: `416` with `Content-Range: bytes */<len>`.
    Unsatisfiable,
}

/// Parses a `Range` header (`bytes=a-b`, `bytes=a-`, `bytes=-n`) for a body of `len` bytes.
pub fn parse_range(header: Option<&str>, len: u64) -> RangeAsk {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeAsk::Full;
    };
    if spec.contains(',') {
        return RangeAsk::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeAsk::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    let parsed = match (a.is_empty(), b.is_empty()) {
        // Suffix: the last `b` bytes.
        (true, false) => match b.parse::<u64>() {
            Ok(0) => return RangeAsk::Unsatisfiable,
            Ok(n) => Some((len.saturating_sub(n), len.saturating_sub(1))),
            Err(_) => None,
        },
        (false, true) => a.parse::<u64>().ok().map(|s| (s, len.saturating_sub(1))),
        (false, false) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e => Some((s, e.min(len.saturating_sub(1)))),
            (Ok(_), Ok(_)) => return RangeAsk::Unsatisfiable,
            _ => None,
        },
        (true, true) => None,
    };
    match parsed {
        None => RangeAsk::Full,
        Some((s, _)) if len == 0 || s >= len => RangeAsk::Unsatisfiable,
        Some((s, e)) => RangeAsk::Part(s, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 10), RangeAsk::Full);
        assert_eq!(parse_range(Some("bytes=0-3"), 10), RangeAsk::Part(0, 3));
        assert_eq!(parse_range(Some("bytes=4-"), 10), RangeAsk::Part(4, 9));
        assert_eq!(parse_range(Some("bytes=-3"), 10), RangeAsk::Part(7, 9));
        assert_eq!(parse_range(Some("bytes=-30"), 10), RangeAsk::Part(0, 9));
        assert_eq!(parse_range(Some("bytes=5-100"), 10), RangeAsk::Part(5, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 10), RangeAsk::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=7-3"), 10), RangeAsk::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 10), RangeAsk::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), RangeAsk::Full);
        assert_eq!(parse_range(Some("items=0-1"), 10), RangeAsk::Full);
        assert_eq!(parse_range(Some("bytes=x-1"), 10), RangeAsk::Full);
        assert_eq!(parse_range(Some("bytes=0-"), 0), RangeAsk::Unsatisfiable);
    }

    #[test]
    fn documents_are_attachments() {
        use super::super::Kind;
        assert_eq!(content_disposition(Kind::Png), None);
        assert_eq!(content_disposition(Kind::Webm), None);
        assert_eq!(content_disposition(Kind::Wav), None);
        for k in [Kind::Pdf, Kind::Text, Kind::Markdown, Kind::Csv, Kind::Vtt] {
            assert_eq!(content_disposition(k), Some("attachment"));
        }
    }

    #[test]
    fn responses_are_inert() {
        let get = |n: &str| {
            SAFETY_HEADERS
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| *v)
        };
        assert_eq!(get("x-content-type-options"), Some("nosniff"));
        assert!(
            get("content-security-policy")
                .unwrap()
                .starts_with("sandbox")
        );
    }
}
