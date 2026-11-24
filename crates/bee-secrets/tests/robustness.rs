//! Property tests and the throughput budget.

use bee_secrets::Scanner;
use proptest::prelude::*;

fn scanner() -> &'static Scanner {
    use std::sync::OnceLock;
    static S: OnceLock<Scanner> = OnceLock::new();
    S.get_or_init(Scanner::builtin)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn scanner_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        for f in scanner().scan("a/b.bin", &bytes) {
            prop_assert!(f.start < f.end && f.end <= bytes.len());
            prop_assert!(f.line >= 1);
        }
    }

    #[test]
    fn scanner_never_panics_on_keyword_salad(parts in proptest::collection::vec(
        prop::sample::select(vec![
            "AKIA", "ghp_", "sk-ant-", "-----BEGIN RSA PRIVATE KEY-----", "-----END RSA PRIVATE KEY-----",
            "password=", "\"", "'", "\n", "eyJ", ".", "aws", "secret", "token", " ", "twilio", "SK", "xoxb-",
            "AccountKey=", "key-", "bee:allow-secret", "\u{1f600}", "0123456789abcdef", "==", "/",
        ]),
        0..200,
    )) {
        let text: String = parts.concat();
        let bytes = text.as_bytes();
        for f in scanner().scan("x.txt", bytes) {
            prop_assert!(f.end <= bytes.len());
        }
    }

    #[test]
    fn scanning_is_deterministic_for_any_input(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        prop_assert_eq!(scanner().scan("p", &bytes), scanner().scan("p", &bytes));
    }
}

/// Source-like text with ordinary uses of `token`, `secret` and `password`.
fn typical_source(target: usize) -> Vec<u8> {
    let chunk = "\
use std::collections::BTreeMap;

/// Parses the next token from the input and advances the cursor.
pub fn next_token(input: &str, pos: &mut usize) -> Option<Token> {
    let bytes = input.as_bytes();
    while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let secret_len = bytes.len().saturating_sub(*pos);
    if secret_len == 0 {
        return None; // nothing left, password policy untouched
    }
    let mut map: BTreeMap<String, u64> = BTreeMap::new();
    map.insert(\"key-count\".to_owned(), secret_len as u64);
    Some(Token { kind: TokenKind::Ident, len: secret_len })
}
";
    let mut v = Vec::with_capacity(target + chunk.len());
    while v.len() < target {
        v.extend_from_slice(chunk.as_bytes());
    }
    v
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing budget is for release builds")]
fn scanning_ten_megabytes_of_source_takes_under_200ms() {
    let s = scanner();
    // Files are scanned one at a time under the 1 MiB cap, as the server does.
    let data = typical_source(10 * 1024 * 1024);
    let files: Vec<&[u8]> = data.chunks(512 * 1024).collect();
    let start = std::time::Instant::now();
    let mut found = 0;
    for (i, f) in files.iter().enumerate() {
        found += s.scan(&format!("src/f{i}.rs"), f).len();
    }
    let elapsed = start.elapsed();
    eprintln!("scanned {} bytes in {elapsed:?}", data.len());
    assert_eq!(found, 0);
    assert!(elapsed.as_millis() < 200, "took {elapsed:?}");
}

#[test]
fn clean_source_has_no_findings() {
    assert!(
        scanner()
            .scan("src/lib.rs", &typical_source(100_000))
            .is_empty()
    );
}
