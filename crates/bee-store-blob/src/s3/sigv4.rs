//! AWS Signature Version 4 for S3: header-signed requests and presigned query strings.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// What a request is signed with.
pub(crate) struct SigningKey<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
    pub region: &'a str,
}

/// The request time in the two forms SigV4 uses.
pub(crate) struct Stamp {
    /// `yyyymmddThhmmssZ`.
    pub stamp: String,
    /// `yyyymmdd`.
    pub date: String,
}

impl Stamp {
    /// The stamp of a Unix time in seconds.
    pub(crate) fn at(unix_secs: u64) -> Stamp {
        let (stamp, date) = super::dates::amz_date(unix_secs);
        Stamp { stamp, date }
    }
}

pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    bee_core::hex::encode(&Sha256::digest(data))
}

/// RFC 3986 percent-encoding as SigV4 defines it: only `A-Za-z0-9-_.~` stay; `/` stays when
/// `keep_slash`.
pub(crate) fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac_sha256(&k, region.as_bytes());
    let k = hmac_sha256(&k, b"s3");
    hmac_sha256(&k, b"aws4_request")
}

pub(crate) fn encode_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Sorts headers by lowercase name and returns `(canonical block, signed header list)`.
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    sorted.sort();
    let block = sorted
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let names = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (block, names)
}

fn sign_string(key: &SigningKey<'_>, when: &Stamp, canonical_request: &str) -> String {
    let scope = format!("{}/{}/s3/aws4_request", when.date, key.region);
    let to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        when.stamp,
        sha256_hex(canonical_request.as_bytes())
    );
    bee_core::hex::encode(&hmac_sha256(
        &signing_key(key.secret_key, &when.date, key.region),
        to_sign.as_bytes(),
    ))
}

/// The `Authorization` header value for a request. `path` is already percent-encoded;
/// `headers` must include `host`, `x-amz-date` and `x-amz-content-sha256` and everything else
/// the request sends that S3 should bind to the signature.
pub(crate) fn authorization(
    key: &SigningKey<'_>,
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_sha256: &str,
    when: &Stamp,
) -> String {
    let (block, names) = canonical_headers(headers);
    let canonical = format!(
        "{method}\n{path}\n{}\n{block}\n{names}\n{payload_sha256}",
        encode_query(query)
    );
    let signature = sign_string(key, when, &canonical);
    format!(
        "{ALGORITHM} Credential={}/{}/{}/s3/aws4_request, SignedHeaders={names}, Signature={signature}",
        key.access_key, when.date, key.region
    )
}

/// The full query string (signature last) of a presigned request for `host` and `path`
/// (already percent-encoded), valid `expires_secs` from `when`.
pub(crate) fn presign_query(
    key: &SigningKey<'_>,
    method: &str,
    host: &str,
    path: &str,
    expires_secs: u64,
    when: &Stamp,
) -> String {
    let mut query: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), ALGORITHM.into()),
        (
            "X-Amz-Credential".into(),
            format!(
                "{}/{}/{}/s3/aws4_request",
                key.access_key, when.date, key.region
            ),
        ),
        ("X-Amz-Date".into(), when.stamp.clone()),
        ("X-Amz-Expires".into(), expires_secs.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    if let Some(token) = key.session_token {
        query.push(("X-Amz-Security-Token".into(), token.into()));
    }
    let canonical_query = encode_query(&query);
    let canonical =
        format!("{method}\n{path}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let signature = sign_string(key, when, &canonical);
    format!("{canonical_query}&X-Amz-Signature={signature}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The worked examples from the AWS documentation for Signature Version 4 on S3
    // (bucket `examplebucket`, the documentation's example credentials, 2013-05-24).
    fn key() -> SigningKey<'static> {
        SigningKey {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: None,
            region: "us-east-1",
        }
    }

    #[test]
    fn presigned_get_matches_the_documented_signature() {
        let query = presign_query(
            &key(),
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            86_400,
            &Stamp::at(1_369_353_600),
        );
        assert_eq!(
            query,
            "X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn header_signed_get_matches_the_documented_signature() {
        let empty = sha256_hex(b"");
        assert_eq!(
            empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let headers = vec![
            (
                "Host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), empty.clone()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization(
            &key(),
            "GET",
            "/test.txt",
            &[],
            &headers,
            &empty,
            &Stamp::at(1_369_353_600),
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn encoding_follows_sigv4() {
        assert_eq!(uri_encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
        assert_eq!(uri_encode("é", false), "%C3%A9");
    }
}
