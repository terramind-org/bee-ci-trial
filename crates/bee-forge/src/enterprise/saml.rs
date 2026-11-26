//! SAML 2.0 service provider: SP metadata, IdP metadata parsing, AuthnRequests, and strict
//! validation of signed responses.
//!
//! # What is built, and the limits
//!
//! No audited pure-Rust XML-DSig library exists, and `samael` needs libxml2/xmlsec1. So this
//! module implements the verification itself, restricted to the profile real IdPs (Okta,
//! Entra ID, ADFS, Keycloak, Google) produce, and **fails closed on everything else**:
//!
//! - one `Signature` per signed element, enveloped, `Reference URI="#<ID>"`, transforms exactly
//!   `enveloped-signature` + `xml-exc-c14n#`, canonicalization `xml-exc-c14n#` (no comments);
//! - RSA PKCS#1 v1.5 with SHA-256/384/512 or ECDSA P-256 with SHA-256; SHA-1 is refused;
//! - the key comes only from the configured IdP certificate(s); `KeyInfo` in the message is
//!   ignored;
//! - the referenced element must be the parent of its `Signature` and its `ID` must be unique
//!   in the document (defeats signature wrapping); the assertion that is used must be the
//!   signed element or lie inside the signed `Response`; any signature that fails to verify
//!   rejects the whole message;
//! - DTDs are refused; encrypted assertions are refused.
//!
//! Semantic checks follow SAML Web Browser SSO: `Status` success, `Destination`, issuer,
//! `Audience` equal to the SP entity id, `SubjectConfirmation` bearer with `Recipient` and
//! `NotOnOrAfter`, `Conditions` window with 2 minutes of clock skew, `InResponseTo` equal to
//! the request id we issued (SP-initiated only). Replay protection (assertion id cache) is the
//! caller's ([`super::sso`]).
//!
//! Exclusive canonicalization is checked against libxml2's `xmllint --exc-c14n` vectors in the
//! tests, and signature verification against signatures made by OpenSSL.

use std::collections::BTreeMap;

use ring::digest;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384,
    RSA_PKCS1_2048_8192_SHA512, UnparsedPublicKey, VerificationAlgorithm,
};
use roxmltree::{Document, Node, ParsingOptions};

use crate::error::{ForgeError, Result};

const NS_DS: &str = "http://www.w3.org/2000/09/xmldsig#";
const NS_SAMLP: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const NS_SAML: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const NS_MD: &str = "urn:oasis:names:tc:SAML:2.0:metadata";
const EXC_C14N: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
const BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
/// Allowed clock skew when checking validity windows.
pub const CLOCK_SKEW_SECS: i64 = 120;

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_saml", msg)
}

// ------------------------------------------------------------------ base64

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Base64 decode; whitespace is ignored, padding optional.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Percent-encodes everything but unreserved characters.
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------------ time

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SSZ` of unix seconds.
pub fn format_instant(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Parses an `xs:dateTime` (`Z` or `±hh:mm`, optional fraction) to unix seconds.
pub fn parse_instant(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, offset) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0i64)
    } else {
        let idx = time.rfind(['+', '-'])?;
        let (c, o) = time.split_at(idx);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let (oh, om) = o[1..].split_once(':')?;
        (
            c,
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60),
        )
    };
    let clock = clock.split('.').next()?;
    let mut t = clock.split(':');
    let (h, mi, sec) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

// ------------------------------------------------------------------ X.509

/// The key algorithm of a certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAlg {
    /// RSA (the key bytes are the DER `RSAPublicKey`).
    Rsa,
    /// ECDSA P-256 (the key bytes are the uncompressed point).
    EcP256,
}

/// What is needed from an IdP signing certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    /// Key algorithm.
    pub alg: KeyAlg,
    /// Public key bytes as `ring` wants them.
    pub key: Vec<u8>,
    /// `notAfter` in unix seconds.
    pub not_after: u64,
    /// `notBefore` in unix seconds.
    pub not_before: u64,
    /// The certificate DER.
    pub der: Vec<u8>,
}

struct Der<'a> {
    b: &'a [u8],
}

impl<'a> Der<'a> {
    fn tlv(&mut self) -> Option<(u8, &'a [u8])> {
        let tag = *self.b.first()?;
        let l0 = *self.b.get(1)?;
        let (len, hdr) = if l0 < 0x80 {
            (usize::from(l0), 2)
        } else {
            let n = usize::from(l0 & 0x7f);
            if n == 0 || n > 4 {
                return None;
            }
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | usize::from(*self.b.get(2 + i)?);
            }
            (len, 2 + n)
        };
        let content = self.b.get(hdr..hdr.checked_add(len)?)?;
        self.b = &self.b[hdr + len..];
        Some((tag, content))
    }
}

fn der_time(tag: u8, c: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(c).ok()?.strip_suffix('Z')?;
    let (y, rest) = match tag {
        0x17 if s.len() == 12 => {
            let yy: i64 = s[..2].parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &s[2..])
        }
        0x18 if s.len() == 14 => (s[..4].parse().ok()?, &s[4..]),
        _ => return None,
    };
    let f = |i: usize| rest.get(i..i + 2)?.parse::<i64>().ok();
    let secs = days_from_civil(y, f(0)?, f(2)?) * 86_400 + f(4)? * 3600 + f(6)? * 60 + f(8)?;
    u64::try_from(secs).ok()
}

/// Parses an X.509 certificate (PEM or raw DER, or the base64 body from metadata).
pub fn parse_certificate(input: &str) -> Result<Certificate> {
    let body: String = input
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = b64_decode(&body).ok_or_else(|| bad("certificate is not base64"))?;
    parse_certificate_der(&der)
}

/// Parses a DER certificate.
pub fn parse_certificate_der(der: &[u8]) -> Result<Certificate> {
    let err = || bad("not a usable X.509 certificate (RSA or P-256 only)");
    let mut top = Der { b: der };
    let (t, cert) = top.tlv().ok_or_else(err)?;
    if t != 0x30 {
        return Err(err());
    }
    let mut c = Der { b: cert };
    let (t, tbs) = c.tlv().ok_or_else(err)?;
    if t != 0x30 {
        return Err(err());
    }
    let mut f = Der { b: tbs };
    let (tag, _) = f.tlv().ok_or_else(err)?;
    if tag == 0xA0 {
        f.tlv().ok_or_else(err)?; // the serial follows the explicit version
    }
    f.tlv().ok_or_else(err)?; // signature algorithm
    f.tlv().ok_or_else(err)?; // issuer
    let (_, validity) = f.tlv().ok_or_else(err)?;
    let mut v = Der { b: validity };
    let (t1, nb) = v.tlv().ok_or_else(err)?;
    let (t2, na) = v.tlv().ok_or_else(err)?;
    let not_before = der_time(t1, nb).ok_or_else(err)?;
    let not_after = der_time(t2, na).ok_or_else(err)?;
    f.tlv().ok_or_else(err)?; // subject
    let (_, spki) = f.tlv().ok_or_else(err)?;
    let mut s = Der { b: spki };
    let (_, alg) = s.tlv().ok_or_else(err)?;
    let (bt, bits) = s.tlv().ok_or_else(err)?;
    if bt != 0x03 || bits.first() != Some(&0) {
        return Err(err());
    }
    let mut a = Der { b: alg };
    let (_, oid) = a.tlv().ok_or_else(err)?;
    const RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
    const EC: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    const P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
    let key_alg = if oid == RSA {
        KeyAlg::Rsa
    } else if oid == EC {
        let (_, curve) = a.tlv().ok_or_else(err)?;
        if curve != P256 {
            return Err(err());
        }
        KeyAlg::EcP256
    } else {
        return Err(err());
    };
    Ok(Certificate {
        alg: key_alg,
        key: bits[1..].to_vec(),
        not_after,
        not_before,
        der: der.to_vec(),
    })
}

// ------------------------------------------------------------------ XML helpers

/// Parses XML with DTDs refused.
fn parse_xml(text: &str) -> Result<Document<'_>> {
    Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 200_000,
            entity_resolver: None,
        },
    )
    .map_err(|e| bad(format!("XML does not parse: {e}")))
}

fn is_el(n: Node, ns: &str, local: &str) -> bool {
    n.is_element() && n.tag_name().namespace() == Some(ns) && n.tag_name().name() == local
}

fn kids<'a, 'i>(n: Node<'a, 'i>, ns: &str, local: &str) -> Vec<Node<'a, 'i>> {
    n.children().filter(|c| is_el(*c, ns, local)).collect()
}

fn kid<'a, 'i>(n: Node<'a, 'i>, ns: &str, local: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| is_el(*c, ns, local))
}

fn text_of(n: Node) -> String {
    n.descendants()
        .filter(|d| d.is_text())
        .filter_map(|d| d.text())
        .collect::<String>()
        .trim()
        .to_string()
}

// ------------------------------------------------------------------ exclusive c14n

fn esc_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
}

fn esc_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
}

fn raw_qname<'a>(doc: &'a Document<'_>, n: Node) -> &'a str {
    let start = n.range().start + 1;
    let text = &doc.input_text()[start..];
    let end = text
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(text.len());
    &text[..end]
}

/// Exclusive canonical XML (without comments) of the element `n`, as the apex of the output.
/// `skip` drops one descendant subtree (the enveloped `Signature`); `inclusive` lists prefixes
/// (`#default` for the default namespace) to render as in inclusive canonicalization.
pub fn exc_c14n(
    doc: &Document<'_>,
    n: Node,
    skip: Option<Node>,
    inclusive: &[String],
) -> Result<String> {
    let mut out = String::new();
    c14n_el(doc, n, skip, inclusive, &BTreeMap::new(), &mut out)?;
    Ok(out)
}

fn c14n_el(
    doc: &Document<'_>,
    n: Node,
    skip: Option<Node>,
    inclusive: &[String],
    parent: &BTreeMap<String, String>,
    out: &mut String,
) -> Result<()> {
    let qname = raw_qname(doc, n);
    let prefix = qname.split_once(':').map(|(p, _)| p).unwrap_or("");
    let mut used: Vec<String> = vec![prefix.to_string()];
    let mut attrs: Vec<(String, String, String, String)> = Vec::new(); // (ns uri, local, qname, value)
    for a in n.attributes() {
        let q = &doc.input_text()[a.range_qname()];
        if let Some((p, _)) = q.split_once(':') {
            if p == "xmlns" {
                continue;
            }
            used.push(p.to_string());
        } else if q == "xmlns" {
            continue;
        }
        attrs.push((
            a.namespace().unwrap_or("").to_string(),
            a.name().to_string(),
            q.to_string(),
            a.value().to_string(),
        ));
    }
    for p in inclusive {
        used.push(if p == "#default" {
            String::new()
        } else {
            p.clone()
        });
    }
    used.sort();
    used.dedup();
    let mut rendered = parent.clone();
    let mut decls: Vec<(String, String)> = Vec::new();
    for p in used {
        let uri = if p == "xml" {
            continue;
        } else if p.is_empty() {
            n.lookup_namespace_uri(None).unwrap_or("").to_string()
        } else {
            match n.lookup_namespace_uri(Some(&p)) {
                Some(u) => u.to_string(),
                // An InclusiveNamespaces prefix that is not in scope is ignored (as the
                // exclusive c14n specification says); used prefixes are always declared.
                None => continue,
            }
        };
        if rendered.get(&p).map(String::as_str).unwrap_or("") != uri {
            decls.push((p.clone(), uri.clone()));
            rendered.insert(p, uri);
        }
    }
    out.push('<');
    out.push_str(qname);
    for (p, uri) in &decls {
        if p.is_empty() {
            out.push_str(" xmlns=\"");
        } else {
            out.push_str(" xmlns:");
            out.push_str(p);
            out.push_str("=\"");
        }
        esc_attr(uri, out);
        out.push('"');
    }
    attrs.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for (_, _, q, v) in &attrs {
        out.push(' ');
        out.push_str(q);
        out.push_str("=\"");
        esc_attr(v, out);
        out.push('"');
    }
    out.push('>');
    for c in n.children() {
        if skip.is_some_and(|s| s == c) {
            continue;
        }
        if c.is_element() {
            c14n_el(doc, c, skip, inclusive, &rendered, out)?;
        } else if c.is_text() {
            esc_text(c.text().unwrap_or(""), out);
        } else if c.is_pi() {
            return Err(bad(
                "processing instructions are not supported in signed content",
            ));
        }
    }
    out.push_str("</");
    out.push_str(qname);
    out.push('>');
    Ok(())
}

fn inclusive_prefixes(transform_or_method: Node) -> Vec<String> {
    kid(transform_or_method, EXC_C14N, "InclusiveNamespaces")
        .and_then(|n| n.attribute("PrefixList"))
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ XML-DSig verification

fn digest_alg(uri: &str) -> Option<&'static digest::Algorithm> {
    match uri {
        "http://www.w3.org/2001/04/xmlenc#sha256" => Some(&digest::SHA256),
        "http://www.w3.org/2001/04/xmldsig-more#sha384" => Some(&digest::SHA384),
        "http://www.w3.org/2001/04/xmlenc#sha512" => Some(&digest::SHA512),
        _ => None,
    }
}

fn sig_alg(uri: &str) -> Option<(KeyAlg, &'static dyn VerificationAlgorithm)> {
    match uri {
        "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256" => {
            Some((KeyAlg::Rsa, &RSA_PKCS1_2048_8192_SHA256))
        }
        "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384" => {
            Some((KeyAlg::Rsa, &RSA_PKCS1_2048_8192_SHA384))
        }
        "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512" => {
            Some((KeyAlg::Rsa, &RSA_PKCS1_2048_8192_SHA512))
        }
        "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256" => {
            Some((KeyAlg::EcP256, &ECDSA_P256_SHA256_FIXED))
        }
        _ => None,
    }
}

/// Verifies one `ds:Signature` and returns the id of the element it signs (its parent).
fn verify_signature<'a, 'i>(
    doc: &'a Document<'i>,
    sig: Node<'a, 'i>,
    certs: &[Certificate],
) -> Result<Node<'a, 'i>> {
    let signed_info =
        kid(sig, NS_DS, "SignedInfo").ok_or_else(|| bad("Signature has no SignedInfo"))?;
    let c14n = kid(signed_info, NS_DS, "CanonicalizationMethod")
        .and_then(|n| n.attribute("Algorithm"))
        .ok_or_else(|| bad("SignedInfo has no CanonicalizationMethod"))?;
    if c14n != EXC_C14N {
        return Err(bad(format!(
            "unsupported canonicalization {c14n}; exclusive c14n only"
        )));
    }
    let method = kid(signed_info, NS_DS, "SignatureMethod")
        .and_then(|n| n.attribute("Algorithm"))
        .ok_or_else(|| bad("SignedInfo has no SignatureMethod"))?;
    let (key_alg, ring_alg) =
        sig_alg(method).ok_or_else(|| bad(format!("unsupported signature method {method}")))?;
    let refs = kids(signed_info, NS_DS, "Reference");
    if refs.len() != 1 {
        return Err(bad("exactly one Reference is required"));
    }
    let r = refs[0];
    let uri = r
        .attribute("URI")
        .ok_or_else(|| bad("Reference has no URI"))?;
    let id = uri
        .strip_prefix('#')
        .filter(|i| !i.is_empty())
        .ok_or_else(|| bad("Reference URI must be #<ID>"))?;
    let transforms =
        kid(r, NS_DS, "Transforms").ok_or_else(|| bad("Reference has no Transforms"))?;
    let list = kids(transforms, NS_DS, "Transform");
    let algs: Vec<&str> = list
        .iter()
        .filter_map(|t| t.attribute("Algorithm"))
        .collect();
    if algs != [ENVELOPED, EXC_C14N] && algs != [EXC_C14N, ENVELOPED] && algs != [ENVELOPED] {
        return Err(bad(
            "transforms must be enveloped-signature and exclusive c14n",
        ));
    }
    let incl = list
        .iter()
        .find(|t| t.attribute("Algorithm") == Some(EXC_C14N))
        .map(|t| inclusive_prefixes(*t))
        .unwrap_or_default();
    let dig = kid(r, NS_DS, "DigestMethod")
        .and_then(|n| n.attribute("Algorithm"))
        .and_then(digest_alg)
        .ok_or_else(|| bad("unsupported digest method"))?;
    let want = b64_decode(&text_of(
        kid(r, NS_DS, "DigestValue").ok_or_else(|| bad("no DigestValue"))?,
    ))
    .ok_or_else(|| bad("DigestValue is not base64"))?;
    // The referenced element: unique ID, and it must be the parent of the signature.
    let holders: Vec<Node> = doc
        .descendants()
        .filter(|n| n.is_element() && n.attribute("ID") == Some(id))
        .collect();
    if holders.len() != 1 {
        return Err(bad("the signed ID is missing or not unique"));
    }
    let signed = holders[0];
    if sig.parent() != Some(signed) {
        return Err(bad(
            "the signature must be enveloped in the element it signs",
        ));
    }
    let canon = exc_c14n(doc, signed, Some(sig), &incl)?;
    let got = digest::digest(dig, canon.as_bytes());
    if !crate::enterprise::common::ct_eq(&crate::ids::hex(got.as_ref()), &crate::ids::hex(&want)) {
        return Err(bad("digest mismatch: the signed content was modified"));
    }
    let si_incl = kid(signed_info, NS_DS, "CanonicalizationMethod")
        .map(inclusive_prefixes)
        .unwrap_or_default();
    let si = exc_c14n(doc, signed_info, None, &si_incl)?;
    let value = b64_decode(&text_of(
        kid(sig, NS_DS, "SignatureValue").ok_or_else(|| bad("no SignatureValue"))?,
    ))
    .ok_or_else(|| bad("SignatureValue is not base64"))?;
    let ok = certs.iter().filter(|c| c.alg == key_alg).any(|c| {
        UnparsedPublicKey::new(ring_alg, &c.key)
            .verify(si.as_bytes(), &value)
            .is_ok()
    });
    if !ok {
        return Err(bad(
            "signature does not verify against the configured IdP certificates",
        ));
    }
    Ok(signed)
}

// ------------------------------------------------------------------ IdP metadata

/// What an IdP's metadata gives the SP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdpMetadata {
    /// Issuer.
    pub entity_id: String,
    /// Single sign-on URL (HTTP-Redirect preferred, else HTTP-POST).
    pub sso_url: String,
    /// The binding of `sso_url`.
    pub sso_binding: String,
    /// Signing certificates (PEM).
    pub certificates: Vec<String>,
}

/// Parses IdP metadata (`EntityDescriptor` with an `IDPSSODescriptor`).
pub fn parse_idp_metadata(xml: &str) -> Result<IdpMetadata> {
    let doc = parse_xml(xml)?;
    let root = doc.root_element();
    let ed = if is_el(root, NS_MD, "EntityDescriptor") {
        root
    } else if is_el(root, NS_MD, "EntitiesDescriptor") {
        kid(root, NS_MD, "EntityDescriptor").ok_or_else(|| bad("no EntityDescriptor"))?
    } else {
        return Err(bad("metadata root is not an EntityDescriptor"));
    };
    let entity_id = ed
        .attribute("entityID")
        .filter(|e| !e.is_empty())
        .ok_or_else(|| bad("EntityDescriptor has no entityID"))?
        .to_string();
    let idp = kid(ed, NS_MD, "IDPSSODescriptor").ok_or_else(|| bad("no IDPSSODescriptor"))?;
    let mut sso = kids(idp, NS_MD, "SingleSignOnService");
    sso.sort_by_key(|n| {
        !n.attribute("Binding")
            .is_some_and(|b| b.ends_with("HTTP-Redirect"))
    });
    let first = sso.first().ok_or_else(|| bad("no SingleSignOnService"))?;
    let sso_url = first
        .attribute("Location")
        .filter(|l| l.starts_with("https://") || l.starts_with("http://"))
        .ok_or_else(|| bad("SingleSignOnService has no usable Location"))?
        .to_string();
    let mut certificates = Vec::new();
    for kd in kids(idp, NS_MD, "KeyDescriptor") {
        if kd.attribute("use").is_some_and(|u| u != "signing") {
            continue;
        }
        for c in kd
            .descendants()
            .filter(|d| d.is_element() && d.tag_name().name() == "X509Certificate")
        {
            let body: String = text_of(c).split_whitespace().collect();
            certificates.push(format!(
                "-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----\n"
            ));
        }
    }
    if certificates.is_empty() {
        return Err(bad("no signing certificate in the metadata"));
    }
    Ok(IdpMetadata {
        entity_id,
        sso_url,
        sso_binding: first.attribute("Binding").unwrap_or("").to_string(),
        certificates,
    })
}

fn xml_text(s: &str) -> String {
    let mut o = String::new();
    esc_text(s, &mut o);
    o
}

fn xml_attr(s: &str) -> String {
    let mut o = String::new();
    esc_attr(s, &mut o);
    o
}

/// SP metadata for a scope: entity id, ACS (HTTP-POST) and the NameID formats accepted.
pub fn sp_metadata(entity_id: &str, acs_url: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<md:EntityDescriptor xmlns:md=\"{NS_MD}\" entityID=\"{e}\">\n\
  <md:SPSSODescriptor AuthnRequestsSigned=\"false\" WantAssertionsSigned=\"true\" protocolSupportEnumeration=\"{NS_SAMLP}\">\n\
    <md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</md:NameIDFormat>\n\
    <md:NameIDFormat>urn:oasis:names:tc:SAML:2.0:nameid-format:persistent</md:NameIDFormat>\n\
    <md:AssertionConsumerService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" Location=\"{a}\" index=\"0\" isDefault=\"true\"/>\n\
  </md:SPSSODescriptor>\n\
</md:EntityDescriptor>\n",
        e = xml_attr(entity_id),
        a = xml_attr(acs_url),
    )
}

/// An AuthnRequest document.
pub fn authn_request(
    id: &str,
    now: u64,
    destination: &str,
    sp_entity_id: &str,
    acs_url: &str,
) -> String {
    format!(
        "<samlp:AuthnRequest xmlns:samlp=\"{NS_SAMLP}\" xmlns:saml=\"{NS_SAML}\" ID=\"{id}\" Version=\"2.0\" \
IssueInstant=\"{t}\" Destination=\"{d}\" ProtocolBinding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" \
AssertionConsumerServiceURL=\"{a}\"><saml:Issuer>{e}</saml:Issuer>\
<samlp:NameIDPolicy AllowCreate=\"true\"/></samlp:AuthnRequest>",
        t = format_instant(now),
        d = xml_attr(destination),
        a = xml_attr(acs_url),
        e = xml_text(sp_entity_id),
    )
}

/// Raw DEFLATE using stored blocks (valid for every inflater; requests are tiny).
pub fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut chunks = data.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    out
}

/// The HTTP-Redirect binding URL for an AuthnRequest.
pub fn redirect_url(sso_url: &str, request_xml: &str, relay_state: &str) -> String {
    let sep = if sso_url.contains('?') { '&' } else { '?' };
    format!(
        "{sso_url}{sep}SAMLRequest={}&RelayState={}",
        url_encode(&b64_encode(&deflate_stored(request_xml.as_bytes()))),
        url_encode(relay_state)
    )
}

// ------------------------------------------------------------------ response validation

/// What the SP expects of a response.
#[derive(Clone, Debug)]
pub struct Expect {
    /// IdP entity id (the issuer).
    pub idp_entity_id: String,
    /// This SP's entity id (the audience).
    pub sp_entity_id: String,
    /// The assertion consumer service URL (destination and recipient).
    pub acs_url: String,
    /// IdP signing certificates.
    pub certs: Vec<Certificate>,
    /// Now, unix seconds.
    pub now: i64,
    /// The AuthnRequest id this answers (`None` accepts IdP-initiated responses).
    pub in_response_to: Option<String>,
}

/// A validated assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    /// The assertion's id (for replay protection).
    pub id: String,
    /// Subject NameID.
    pub name_id: String,
    /// Session index of the authentication, if any.
    pub session_index: String,
    /// Attributes by name.
    pub attributes: BTreeMap<String, Vec<String>>,
    /// Latest time the assertion may be accepted (unix seconds).
    pub not_on_or_after: i64,
    /// When the IdP session ends, if stated.
    pub session_not_on_or_after: Option<i64>,
    /// `InResponseTo` of the response, if any.
    pub in_response_to: String,
}

fn within(now: i64, not_before: Option<&str>, not_on_or_after: Option<&str>) -> Result<()> {
    if let Some(nb) = not_before {
        let nb = parse_instant(nb).ok_or_else(|| bad("bad NotBefore"))?;
        if now + CLOCK_SKEW_SECS < nb {
            return Err(bad("the assertion is not valid yet"));
        }
    }
    if let Some(na) = not_on_or_after {
        let na = parse_instant(na).ok_or_else(|| bad("bad NotOnOrAfter"))?;
        if now - CLOCK_SKEW_SECS >= na {
            return Err(bad("the assertion has expired"));
        }
    }
    Ok(())
}

/// Validates a base64 `SAMLResponse` (see the module docs) and returns the assertion.
pub fn validate_response(saml_response_b64: &str, e: &Expect) -> Result<Assertion> {
    let xml_bytes =
        b64_decode(saml_response_b64).ok_or_else(|| bad("SAMLResponse is not base64"))?;
    if xml_bytes.len() > 1 << 20 {
        return Err(bad("SAMLResponse is too large"));
    }
    let xml = std::str::from_utf8(&xml_bytes).map_err(|_| bad("SAMLResponse is not UTF-8"))?;
    let doc = parse_xml(xml)?;
    let resp = doc.root_element();
    if !is_el(resp, NS_SAMLP, "Response") {
        return Err(bad("the root element is not a samlp:Response"));
    }
    // Signatures first: every one must verify, and each names a unique signed element.
    let sigs: Vec<Node> = doc
        .descendants()
        .filter(|n| is_el(*n, NS_DS, "Signature"))
        .collect();
    if sigs.is_empty() || sigs.len() > 2 {
        return Err(bad("the response and/or its assertion must be signed"));
    }
    let mut signed: Vec<Node> = Vec::new();
    for s in &sigs {
        signed.push(verify_signature(&doc, *s, &e.certs)?);
    }
    if doc
        .descendants()
        .any(|n| is_el(n, NS_SAML, "EncryptedAssertion"))
    {
        return Err(bad("encrypted assertions are not supported"));
    }
    let status = kid(resp, NS_SAMLP, "Status")
        .and_then(|s| kid(s, NS_SAMLP, "StatusCode"))
        .and_then(|c| c.attribute("Value"))
        .ok_or_else(|| bad("the response has no status"))?;
    if status != STATUS_SUCCESS {
        return Err(bad(format!("the IdP reported {status}")));
    }
    if let Some(d) = resp.attribute("Destination")
        && d != e.acs_url
    {
        return Err(bad("the response destination is not this SP's ACS URL"));
    }
    if let Some(expected) = &e.in_response_to
        && resp.attribute("InResponseTo") != Some(expected.as_str())
    {
        return Err(bad("the response does not answer our request"));
    }
    if let Some(issuer) = kid(resp, NS_SAML, "Issuer")
        && text_of(issuer) != e.idp_entity_id
    {
        return Err(bad("the response issuer is not the configured IdP"));
    }
    let assertions = kids(resp, NS_SAML, "Assertion");
    if assertions.len() != 1 {
        return Err(bad("exactly one assertion is required"));
    }
    let a = assertions[0];
    let covered = signed.iter().any(|s| *s == a || *s == resp);
    if !covered {
        return Err(bad("the assertion is not covered by a verified signature"));
    }
    let id = a
        .attribute("ID")
        .filter(|i| !i.is_empty())
        .ok_or_else(|| bad("the assertion has no ID"))?;
    let issuer = kid(a, NS_SAML, "Issuer").map(text_of).unwrap_or_default();
    if issuer != e.idp_entity_id {
        return Err(bad("the assertion issuer is not the configured IdP"));
    }
    let subject = kid(a, NS_SAML, "Subject").ok_or_else(|| bad("the assertion has no Subject"))?;
    let name_id = kid(subject, NS_SAML, "NameID")
        .map(text_of)
        .unwrap_or_default();
    if name_id.is_empty() {
        return Err(bad("the assertion has no NameID"));
    }
    let mut confirmed = false;
    let mut not_on_or_after = i64::MAX;
    for sc in kids(subject, NS_SAML, "SubjectConfirmation") {
        if sc.attribute("Method") != Some(BEARER) {
            continue;
        }
        let Some(d) = kid(sc, NS_SAML, "SubjectConfirmationData") else {
            continue;
        };
        if d.attribute("Recipient") != Some(e.acs_url.as_str()) {
            continue;
        }
        if let Some(irt) = &e.in_response_to
            && d.attribute("InResponseTo") != Some(irt.as_str())
        {
            continue;
        }
        let Some(na) = d.attribute("NotOnOrAfter") else {
            continue;
        };
        if within(e.now, d.attribute("NotBefore"), Some(na)).is_ok() {
            not_on_or_after = not_on_or_after.min(parse_instant(na).unwrap_or(i64::MAX));
            confirmed = true;
        }
    }
    if !confirmed {
        return Err(bad("no valid bearer subject confirmation for this ACS URL"));
    }
    let cond =
        kid(a, NS_SAML, "Conditions").ok_or_else(|| bad("the assertion has no Conditions"))?;
    within(
        e.now,
        cond.attribute("NotBefore"),
        cond.attribute("NotOnOrAfter"),
    )?;
    if let Some(na) = cond.attribute("NotOnOrAfter").and_then(parse_instant) {
        not_on_or_after = not_on_or_after.min(na);
    }
    let restrictions = kids(cond, NS_SAML, "AudienceRestriction");
    if restrictions.is_empty() {
        return Err(bad("the assertion has no AudienceRestriction"));
    }
    for r in restrictions {
        if !kids(r, NS_SAML, "Audience")
            .iter()
            .any(|au| text_of(*au) == e.sp_entity_id)
        {
            return Err(bad("the assertion is not for this SP (audience)"));
        }
    }
    let authn = kid(a, NS_SAML, "AuthnStatement");
    let session_index = authn
        .and_then(|s| s.attribute("SessionIndex"))
        .unwrap_or("")
        .to_string();
    let session_not_on_or_after = authn
        .and_then(|s| s.attribute("SessionNotOnOrAfter"))
        .and_then(parse_instant);
    let mut attributes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for st in kids(a, NS_SAML, "AttributeStatement") {
        for at in kids(st, NS_SAML, "Attribute") {
            let Some(name) = at.attribute("Name") else {
                continue;
            };
            let vals = kids(at, NS_SAML, "AttributeValue").into_iter().map(text_of);
            attributes.entry(name.to_string()).or_default().extend(vals);
        }
    }
    Ok(Assertion {
        id: id.to_string(),
        name_id,
        session_index,
        attributes,
        not_on_or_after,
        session_not_on_or_after,
        in_response_to: resp.attribute("InResponseTo").unwrap_or("").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(b64_decode(&b64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(b64_decode("Zm9v!").is_none());
    }

    #[test]
    fn instants() {
        assert_eq!(parse_instant("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        assert_eq!(
            parse_instant("2024-01-01T01:00:00+01:00"),
            Some(1_704_067_200)
        );
        assert_eq!(
            parse_instant("2024-01-01T00:00:00.123Z"),
            Some(1_704_067_200)
        );
        assert_eq!(format_instant(1_704_067_200), "2024-01-01T00:00:00Z");
        assert_eq!(format_instant(0), "1970-01-01T00:00:00Z");
        assert!(parse_instant("garbage").is_none());
    }

    #[test]
    fn stored_deflate_has_valid_framing() {
        let d = deflate_stored(b"abc");
        assert_eq!(d, vec![1, 3, 0, 0xfc, 0xff, b'a', b'b', b'c']);
        let big = deflate_stored(&vec![7u8; 70_000]);
        assert_eq!(big[0], 0);
        assert_eq!(big.len(), 70_000 + 10);
    }

    #[test]
    fn dtd_is_refused() {
        let xml = "<!DOCTYPE x [<!ENTITY a \"b\">]><x>&a;</x>";
        assert!(parse_xml(xml).is_err());
    }

    #[test]
    fn exclusive_c14n_renders_only_used_namespaces() {
        let xml = r#"<a xmlns:x="urn:x" xmlns:y="urn:y" xmlns="urn:d"><b y:att="1" z="2">t &amp; &lt;<c xmlns:y="urn:y"/></b></a>"#;
        let doc = parse_xml(xml).unwrap();
        let b = doc.root_element().first_element_child().unwrap();
        let out = exc_c14n(&doc, b, None, &[]).unwrap();
        assert_eq!(
            out,
            r#"<b xmlns="urn:d" xmlns:y="urn:y" z="2" y:att="1">t &amp; &lt;<c></c></b>"#
        );
    }
}

#[cfg(test)]
mod fixture_tests {
    //! Vectors made by independent tools: `xmllint --exc-c14n` (libxml2) for canonicalization
    //! and `openssl dgst -sign` for the signature over a libxml2-canonicalized `SignedInfo`.

    use super::*;

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIC6DCCAdCgAwIBAgIUIzYHz4Yi+xs3ABoHtFNwH3slsg8wDQYJKoZIhvcNAQEL\nBQAwEzERMA8GA1UEAwwIdGVzdC1pZHAwIBcNMjYxMDE4MTAzODEzWhgPMjEyNjA5\nMjQxMDM4MTNaMBMxETAPBgNVBAMMCHRlc3QtaWRwMIIBIjANBgkqhkiG9w0BAQEF\nAAOCAQ8AMIIBCgKCAQEAtIHabimcnRfq4s4kUjuRgxsoiGdk0PdxwNRRvMXqOGPP\nkfWG/Ftja6c+9/8F7YzY85vjlUekYfxj9Xnormhs0LEUoXEfP3wY4HKWqdHwjWKv\nQmvjioKceyEm/5uXz6+a2zy5E0VbjeXMOntWHBaGSAp+9ROd3WlAvhxc8Borthp1\nYo2am6442T0gA4XSnM3e4QF+kdU6wChInorqrOOzAUtRrrJdS6o9JJpsVVDJCZ8g\nvc/b1diUQpHJrcJ7OK+Rmp969LxxjdVvm2f33ukOpq2f0kTLsROK7Ea8oaZP6nLN\nUm3fs6Aay2Aqz//8CGErRxz6NOt3RerkdxAxOvVBuwIDAQABozIwMDAdBgNVHQ4E\nFgQUHNqVuzaZRAHw2vWIFcZMVq9jKV4wDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG\n9w0BAQsFAAOCAQEAmUlfHKVqYhqgChUVOpjpWDCZpwzGgaIWWyWMjXrHvWWQdhlz\nzHXGkcL80IPNLUJu/+T+ZJiX3GV+l9whexhxG1TtVycqYk949c7BxDCKVs3OlnQI\n6aslll9uiOiidT+77FnWYet0nOsQCcIX/ufklCnucwSQr9uyFowDnI9qL+uj8u7v\nlRC9gRVBTaxnBdSIE1QtW7+bk9McdCGa7d9RJMcT3AhMiry9CFtGczWT6BVuNMHa\nHZW32VY9EfZNcBgKW1Kgsf77jIf8C4IH6rurzrN8FJcpzrbMRvOJBTg2/Qx3eNzH\nxy46+07lnSZDzfdmcm6Flj+9wUOHKIBLCxt4Og==\n-----END CERTIFICATE-----\n";
    const RESPONSE: &str = r##"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" ID="_resp1" InResponseTo="_req1" IssueInstant="2026-11-01T00:00:00Z" Version="2.0" Destination="https://hub.example.com/saml/orgs/acme/acs"><saml:Issuer xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">https://idp.example.com/metadata</saml:Issuer><samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></samlp:Status><saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="_a75adf55-01d7-40cc-929f-dbd8372ebdfc" IssueInstant="2026-11-01T00:00:00Z" Version="2.0"><saml:Issuer>https://idp.example.com/metadata</saml:Issuer><ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:SignedInfo><ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"></ds:CanonicalizationMethod><ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256"></ds:SignatureMethod><ds:Reference URI="#_a75adf55-01d7-40cc-929f-dbd8372ebdfc"><ds:Transforms><ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"></ds:Transform><ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"><ec:InclusiveNamespaces xmlns:ec="http://www.w3.org/2001/10/xml-exc-c14n#" PrefixList="xs"></ec:InclusiveNamespaces></ds:Transform></ds:Transforms><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"></ds:DigestMethod><ds:DigestValue>4s4mDNG9Zxe9cCeOpMdRlqUMc2PQTwDYWV/zQOfSwIc=</ds:DigestValue></ds:Reference></ds:SignedInfo><ds:SignatureValue>XtQwknm8uW5E6S1T/rp+rS0ClKp4bVR+sTZc5erTk+R1Kn4TMechno9WuZkMQKTNArMlqhX/mV+h6AjNo1HmroNt/Xd3hSlfe8M+6aoyqvZ5t66YToRNm//his1owLylymAIC7dy0j3wUh9BQKZPiIBtrQD1QbNlWvauUCT6TP+mstvBY9/3Y8YWRyxCTl3BMevBCHrGhKZW1ewFLE+ch15ApuS72swvR3YV79qusbaplOgumdr4/Ffv0p7Sb/ab+GU5ZHGTXyagqSRwVgQDTr91lMbpt9Rdg7/yr6x+q7G2KTeUJ6WVyUP7lLTevuPFb8IRrqrTUc8uQso55bdwdQ==</ds:SignatureValue></ds:Signature><saml:Subject><saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">alice@example.com</saml:NameID><saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml:SubjectConfirmationData InResponseTo="_req1" NotOnOrAfter="2099-01-01T00:00:00Z" Recipient="https://hub.example.com/saml/orgs/acme/acs"/></saml:SubjectConfirmation></saml:Subject><saml:Conditions NotBefore="2026-10-31T23:59:00Z" NotOnOrAfter="2099-01-01T00:00:00Z"><saml:AudienceRestriction><saml:Audience>https://hub.example.com/saml/orgs/acme</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AuthnStatement AuthnInstant="2026-11-01T00:00:00Z" SessionIndex="_s1"><saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement><saml:AttributeStatement><saml:Attribute Name="groups"><saml:AttributeValue>core</saml:AttributeValue><saml:AttributeValue>admins</saml:AttributeValue></saml:Attribute></saml:AttributeStatement></saml:Assertion></samlp:Response>"##;

    fn expect(cert: &Certificate) -> Expect {
        Expect {
            idp_entity_id: "https://idp.example.com/metadata".into(),
            sp_entity_id: "https://hub.example.com/saml/orgs/acme".into(),
            acs_url: "https://hub.example.com/saml/orgs/acme/acs".into(),
            certs: vec![cert.clone()],
            now: parse_instant("2026-11-01T00:05:00Z").unwrap(),
            in_response_to: Some("_req1".into()),
        }
    }

    fn check(xml: &str, e: &Expect) -> Result<Assertion> {
        validate_response(&b64_encode(xml.as_bytes()), e)
    }

    #[test]
    fn libxml2_vectors_for_exclusive_c14n() {
        let cases = [
            (
                r#"<a xmlns="urn:d" xmlns:x="urn:x" xmlns:y="urn:y"><x:b y:q="1" z="a&amp;b&lt;c&quot;d" x:p="2">text &gt; &amp; <![CDATA[<raw>]]> tail<!-- dropped --><c/><d xmlns=""><e/></d></x:b></a>"#,
                r#"<a xmlns="urn:d"><x:b xmlns:x="urn:x" xmlns:y="urn:y" z="a&amp;b&lt;c&quot;d" x:p="2" y:q="1">text &gt; &amp; &lt;raw&gt; tail<c></c><d xmlns=""><e></e></d></x:b></a>"#,
            ),
            (
                "<r xmlns:p=\"urn:p\" xml:lang=\"en\"><p:s a=\"1\"  b = '2'>\n\t<p:t>x</p:t>\n</p:s></r>",
                "<r xml:lang=\"en\"><p:s xmlns:p=\"urn:p\" a=\"1\" b=\"2\">\n\t<p:t>x</p:t>\n</p:s></r>",
            ),
            (
                r#"<root><child xmlns="urn:c" xmlns:k="urn:k" k:attr="v">v</child><other xmlns:unused="urn:u"/></root>"#,
                r#"<root><child xmlns="urn:c" xmlns:k="urn:k" k:attr="v">v</child><other></other></root>"#,
            ),
        ];
        for (input, want) in cases {
            let doc = parse_xml(input).unwrap();
            assert_eq!(exc_c14n(&doc, doc.root_element(), None, &[]).unwrap(), want);
        }
    }

    #[test]
    fn an_openssl_signed_response_validates() {
        let cert = parse_certificate(CERT).unwrap();
        assert_eq!(cert.alg, KeyAlg::Rsa);
        assert!(cert.not_after > cert.not_before);
        let a = check(RESPONSE, &expect(&cert)).unwrap();
        assert_eq!(a.name_id, "alice@example.com");
        assert_eq!(a.session_index, "_s1");
        assert_eq!(a.attributes["groups"], vec!["core", "admins"]);
        assert_eq!(a.in_response_to, "_req1");
    }

    #[test]
    fn tampering_is_detected() {
        let cert = parse_certificate(CERT).unwrap();
        let e = expect(&cert);
        let tampered = RESPONSE.replace("alice@example.com", "mallory@example.com");
        assert!(
            check(&tampered, &e)
                .unwrap_err()
                .message
                .contains("digest mismatch")
        );
        // A flipped signature byte fails verification.
        let sig = "XtQwknm8uW5E6S1T";
        let bad_sig = RESPONSE.replace(sig, "XtQwknm8uW5E6S1U");
        assert!(
            check(&bad_sig, &e)
                .unwrap_err()
                .message
                .contains("does not verify")
        );
        // The signed attribute list cannot be extended.
        let extra = RESPONSE.replace(
            "<saml:AttributeValue>admins",
            "<saml:AttributeValue>root</saml:AttributeValue><saml:AttributeValue>admins",
        );
        assert!(check(&extra, &e).is_err());
    }

    #[test]
    fn semantic_checks_refuse_misdirected_or_stale_assertions() {
        let cert = parse_certificate(CERT).unwrap();
        let base = expect(&cert);
        let mut e = base.clone();
        e.sp_entity_id = "https://hub.example.com/saml/orgs/other".into();
        assert!(
            check(RESPONSE, &e)
                .unwrap_err()
                .message
                .contains("audience")
        );
        let mut e = base.clone();
        e.acs_url = "https://evil.example.com/acs".into();
        assert!(check(RESPONSE, &e).is_err());
        let mut e = base.clone();
        e.idp_entity_id = "https://other-idp.example.com".into();
        assert!(check(RESPONSE, &e).is_err());
        let mut e = base.clone();
        e.in_response_to = Some("_other".into());
        assert!(check(RESPONSE, &e).is_err());
        let mut e = base.clone();
        e.now = parse_instant("2099-06-01T00:00:00Z").unwrap();
        assert!(
            check(RESPONSE, &e)
                .unwrap_err()
                .message
                .contains("bearer subject confirmation")
        );
        let mut e = base.clone();
        e.now = parse_instant("2026-10-30T00:00:00Z").unwrap();
        assert!(
            check(RESPONSE, &e)
                .unwrap_err()
                .message
                .contains("not valid yet")
        );
        // IdP-initiated (no InResponseTo expectation) still validates the rest.
        let mut e = base;
        e.in_response_to = None;
        assert!(check(RESPONSE, &e).is_ok());
    }

    #[test]
    fn signature_wrapping_and_unsigned_assertions_are_refused() {
        let cert = parse_certificate(CERT).unwrap();
        let e = expect(&cert);
        // Move the signed assertion into Extensions and put an unsigned one where it was.
        let start = RESPONSE.find("<saml:Assertion").unwrap();
        let end = RESPONSE.find("</saml:Assertion>").unwrap() + "</saml:Assertion>".len();
        let signed = &RESPONSE[start..end];
        let evil = signed
            .replace("alice@example.com", "mallory@example.com")
            .replace("_a75adf55-01d7-40cc-929f-dbd8372ebdfc", "_evil");
        let evil_unsigned = {
            let s = evil.find("<ds:Signature").unwrap();
            let t = evil.find("</ds:Signature>").unwrap() + "</ds:Signature>".len();
            format!("{}{}", &evil[..s], &evil[t..])
        };
        let wrapped = format!(
            "{}<samlp:Extensions>{signed}</samlp:Extensions>{evil_unsigned}{}",
            &RESPONSE[..start],
            &RESPONSE[end..]
        );
        assert!(check(&wrapped, &e).is_err());
        // No signature at all.
        let s = RESPONSE.find("<ds:Signature").unwrap();
        let t = RESPONSE.find("</ds:Signature>").unwrap() + "</ds:Signature>".len();
        let unsigned = format!("{}{}", &RESPONSE[..s], &RESPONSE[t..]);
        assert!(
            check(&unsigned, &e)
                .unwrap_err()
                .message
                .contains("must be signed")
        );
        // A different certificate does not verify the signature.
        let mut other = cert.clone();
        let n = other.key.len();
        other.key[n - 8] ^= 0x55;
        let mut e2 = e.clone();
        e2.certs = vec![other];
        assert!(check(RESPONSE, &e2).is_err());
        // Doctypes and garbage are refused before anything else.
        assert!(check("<!DOCTYPE r [<!ENTITY x \"y\">]><samlp:Response/>", &e).is_err());
        assert!(validate_response("not base64 !!", &e).is_err());
    }

    #[test]
    fn metadata_round_trip() {
        let md = format!(
            "<md:EntityDescriptor xmlns:md=\"{NS_MD}\" xmlns:ds=\"{NS_DS}\" entityID=\"https://idp.example.com/metadata\">\
<md:IDPSSODescriptor protocolSupportEnumeration=\"{NS_SAMLP}\">\
<md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>\
<md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" Location=\"https://idp.example.com/post\"/>\
<md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" Location=\"https://idp.example.com/sso\"/>\
</md:IDPSSODescriptor></md:EntityDescriptor>",
            CERT.lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>()
        );
        let parsed = parse_idp_metadata(&md).unwrap();
        assert_eq!(parsed.entity_id, "https://idp.example.com/metadata");
        assert_eq!(parsed.sso_url, "https://idp.example.com/sso");
        assert_eq!(parsed.certificates.len(), 1);
        assert!(parse_certificate(&parsed.certificates[0]).is_ok());
        assert!(parse_idp_metadata("<x/>").is_err());
        let sp = sp_metadata("https://hub/saml/orgs/a", "https://hub/saml/orgs/a/acs");
        let doc = parse_xml(&sp).unwrap();
        assert!(is_el(doc.root_element(), NS_MD, "EntityDescriptor"));
        let url = redirect_url(
            "https://idp.example.com/sso",
            &authn_request("_x", 0, "https://idp.example.com/sso", "sp", "acs"),
            "_x",
        );
        assert!(
            url.starts_with("https://idp.example.com/sso?SAMLRequest=")
                && url.ends_with("&RelayState=_x")
        );
    }
}
