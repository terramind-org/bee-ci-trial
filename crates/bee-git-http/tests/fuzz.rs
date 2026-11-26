//! Fuzzing of the gateway's protocol parsers (PRODUCT §4.25): pkt-lines, push requests
//! (commands, capabilities, shallow lines and a pack), partial-clone filters, ref names, SSH
//! exec commands and keys, and HTTP credentials never panic on arbitrary or half-valid input.

use bee_git::ObjectFormat;
use bee_git_http::pktline::{self, Reader};
use bee_git_http::walk::Filter;
use bee_git_http::{credential, receive, refs, ssh};
use proptest::prelude::*;

fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..max)
}

/// Reads every packet of `buf` until the end or an error.
fn drain(buf: &[u8]) {
    let mut r = Reader::new(buf);
    for _ in 0..10_000 {
        match r.next_pkt() {
            Ok(Some(p)) => {
                let _ = p.line();
            }
            Ok(None) | Err(_) => break,
        }
    }
    let _ = r.rest();
}

/// A push request body: pkt-line commands (some well-formed), a flush, then pack bytes.
fn push_body() -> impl Strategy<Value = Vec<u8>> {
    let oid = prop_oneof![
        Just("0".repeat(40)),
        Just("a".repeat(40)),
        "[0-9a-f]{40}",
        "[0-9a-z]{0,44}",
    ];
    let name = prop_oneof![
        Just("refs/heads/main".to_string()),
        Just("refs/tags/v1".to_string()),
        "refs/[a-z/._-]{0,30}",
        any::<String>(),
    ];
    let caps = prop_oneof![
        Just(String::new()),
        Just("\0report-status side-band-64k atomic push-options object-format=sha1".to_string()),
        "\0[ -~]{0,40}",
    ];
    let command = (oid.clone(), oid, name, caps).prop_map(|(a, b, n, c)| format!("{a} {b} {n}{c}"));
    let line = prop_oneof![
        command,
        "shallow [0-9a-f]{40}",
        "push-option [ -~]{0,20}",
        any::<String>(),
    ];
    (
        proptest::collection::vec(line, 0..6),
        any::<bool>(),
        bytes(256),
    )
        .prop_map(|(lines, pack_header, tail)| {
            let mut out = Vec::new();
            for l in lines {
                pktline::text(&mut out, &format!("{l}\n"));
            }
            pktline::flush(&mut out);
            if pack_header {
                out.extend_from_slice(b"PACK\0\0\0\x02\0\0\0\x01");
            }
            out.extend(tail);
            out
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn pkt_line_reader_never_panics(buf in prop_oneof![
        bytes(512),
        proptest::collection::vec(prop_oneof![
            Just(b"0000".to_vec()),
            Just(b"0001".to_vec()),
            Just(b"0002".to_vec()),
            Just(b"0004".to_vec()),
            Just(b"fff0".to_vec()),
            "[0-9a-f]{4}[ -~]{0,12}".prop_map(String::into_bytes),
        ], 0..16).prop_map(|v| v.concat()),
    ]) {
        drain(&buf);
    }

    #[test]
    fn push_requests_never_panic(body in prop_oneof![push_body(), bytes(512)]) {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            if let Ok(req) = receive::parse(format, &body) {
                let _ = req.has("atomic");
            }
        }
    }

    #[test]
    fn filters_ref_names_and_exec_commands_never_panic(text in any::<String>()) {
        let _ = Filter::parse(&text);
        let _ = refs::bee_name(&text, "me");
        let _ = ssh::parse_exec(&text);
        let _ = ssh::parse_public_key(&text);
    }

    #[test]
    fn http_credentials_never_panic(value in prop_oneof![
        any::<String>(),
        "(Basic|Bearer|basic) [A-Za-z0-9+/=:_-]{0,40}",
    ]) {
        let mut headers = http::HeaderMap::new();
        if let Ok(v) = http::HeaderValue::from_str(&value) {
            headers.insert(http::header::AUTHORIZATION, v);
        }
        let _ = credential(&headers);
    }
}
