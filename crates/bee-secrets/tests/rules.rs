//! Every built-in rule has a positive and a negative test, plus provider-specific cases.

use bee_secrets::{Confidence, Scanner, builtin_rules, shannon_entropy};

fn ids(scanner: &Scanner, path: &str, text: &str) -> Vec<String> {
    scanner
        .scan(path, text.as_bytes())
        .into_iter()
        .map(|f| f.rule_id)
        .collect()
}

#[test]
fn rule_ids_are_unique_and_documented() {
    let rules = builtin_rules();
    let mut seen = std::collections::BTreeSet::new();
    for r in &rules {
        assert!(seen.insert(r.id().to_owned()), "duplicate {}", r.id());
        assert!(
            !r.description().is_empty(),
            "{} lacks a description",
            r.id()
        );
        assert!(!r.provider().is_empty());
        assert!(!r.keywords().is_empty());
        assert!(
            r.example().is_some() && r.near_miss().is_some(),
            "{}",
            r.id()
        );
    }
    assert!(rules.len() >= 25);
}

#[test]
fn every_builtin_example_matches_its_own_rule() {
    let scanner = Scanner::builtin();
    for rule in scanner.rules() {
        rule.check_examples().unwrap();
        let found = ids(&scanner, "src/x.txt", rule.example().unwrap());
        assert!(
            found.iter().any(|id| id == rule.id()),
            "{} example found {found:?}",
            rule.id()
        );
    }
}

#[test]
fn every_builtin_near_miss_does_not_match_its_own_rule() {
    let scanner = Scanner::builtin();
    for rule in scanner.rules() {
        let found = ids(&scanner, "src/x.txt", rule.near_miss().unwrap());
        assert!(
            !found.iter().any(|id| id == rule.id()),
            "{} near miss matched",
            rule.id()
        );
    }
}

#[test]
fn findings_never_contain_the_secret() {
    let scanner = Scanner::builtin();
    for rule in scanner.rules() {
        let text = rule.example().unwrap();
        for f in scanner.scan("a.txt", text.as_bytes()) {
            let secret = &text[f.start..f.end];
            assert!(f.redacted_preview.ends_with("****"));
            assert!(f.redacted_preview.len() <= 8);
            assert!(!f.redacted_preview.contains(&secret[secret.len() / 2..]));
            assert!(f.end > f.start && f.line >= 1);
        }
    }
}

#[test]
fn aws_documented_example_keys_are_found_with_offsets_and_lines() {
    let text = "fn main() {}\nlet id = \"AKIAIOSFODNN7EXAMPLE\";\n";
    let f = Scanner::builtin().scan("src/main.rs", text.as_bytes());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].rule_id, "aws-access-key-id");
    assert_eq!(f[0].provider, "aws");
    assert_eq!(&text[f[0].start..f[0].end], "AKIAIOSFODNN7EXAMPLE");
    assert_eq!(f[0].line, 2);
    assert_eq!(f[0].redacted_preview, "AKIA****");
    assert_eq!(f[0].confidence, Confidence::High);
}

#[test]
fn aws_secret_key_needs_aws_context_and_forty_characters() {
    let s = Scanner::builtin();
    let key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    assert_eq!(key.len(), 40);
    let rule = "aws-secret-access-key".to_owned();
    assert!(ids(&s, "a", &format!("AWS_SECRET_ACCESS_KEY={key}\n")).contains(&rule));
    assert!(!ids(&s, "a", &format!("checksum = \"{key}\"\n")).contains(&rule));
    let long = format!("aws_secret_access_key = \"{key}X\"\n");
    assert!(!ids(&s, "a", &long).contains(&rule));
}

#[test]
fn github_prefixes_all_match_and_wrong_lengths_do_not() {
    let s = Scanner::builtin();
    let body = "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH3jL5"; // 36 chars
    assert_eq!(body.len(), 36);
    for prefix in ["ghp", "gho", "ghu", "ghs", "ghr"] {
        let t = format!("x = {prefix}_{body}");
        assert_eq!(ids(&s, "a", &t), vec!["github-token"], "{prefix}");
        let short = format!("x = {prefix}_{}", &body[..35]);
        assert!(ids(&s, "a", &short).is_empty(), "{prefix} short");
    }
    assert!(ids(&s, "a", &format!("ghx_{body}")).is_empty());
    let pat = format!(
        "github_pat_11ABCDEFG0{}",
        "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH3jL5".repeat(2)
    );
    assert_eq!(pat.len() - "github_pat_".len(), 82);
    assert_eq!(ids(&s, "a", &pat), vec!["github-fine-grained-pat"]);
    assert!(ids(&s, "a", &format!("{pat}Z")).is_empty());
}

#[test]
fn stripe_live_is_high_and_test_is_medium() {
    let s = Scanner::builtin();
    let live = s.scan(
        "a",
        concat!("k = sk_", "live_4eC39HqLyjWDarjtT1zdp7dc").as_bytes(),
    );
    let test = s.scan(
        "a",
        concat!("k = sk_", "test_4eC39HqLyjWDarjtT1zdp7dc").as_bytes(),
    );
    assert_eq!(
        (live[0].rule_id.as_str(), live[0].confidence),
        ("stripe-live-key", Confidence::High)
    );
    assert_eq!(
        (test[0].rule_id.as_str(), test[0].confidence),
        ("stripe-test-key", Confidence::Medium)
    );
    assert!(s.scan("a", b"pk_live_4eC39HqLyjWDarjtT1zdp7dc").is_empty());
}

#[test]
fn openai_and_anthropic_keys_are_distinguished() {
    let s = Scanner::builtin();
    let ant = format!(
        "sk-ant-api03-{}",
        "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH3jL5".repeat(3)
    );
    assert_eq!(ids(&s, "a", &ant), vec!["anthropic-api-key"]);
    let legacy = "sk-aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH3jL5nR7tV9xB2cE4";
    assert_eq!(legacy.len() - 3, 48);
    assert_eq!(ids(&s, "a", legacy), vec!["openai-api-key"]);
    assert!(
        ids(
            &s,
            "a",
            "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .is_empty()
    );
}

#[test]
fn pem_headers_without_bodies_and_public_keys_do_not_match() {
    let s = Scanner::builtin();
    assert!(ids(&s, "a", "-----BEGIN RSA PRIVATE KEY-----\n").is_empty());
    assert!(
        ids(
            &s,
            "a",
            "-----BEGIN CERTIFICATE-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END CERTIFICATE-----"
        )
        .is_empty()
    );
    for kind in ["RSA ", "EC ", "OPENSSH ", "DSA ", "ENCRYPTED ", ""] {
        let t = format!(
            "-----BEGIN {kind}PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END {kind}PRIVATE KEY-----"
        );
        assert_eq!(ids(&s, "k.pem", &t), vec!["private-key-pem"], "{kind:?}");
    }
}

#[test]
fn gcp_service_account_json_reports_one_wide_finding() {
    let t = "{\"type\": \"service_account\", \"private_key\": \"-----BEGIN PRIVATE KEY-----\\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\\n-----END PRIVATE KEY-----\\n\"}";
    assert_eq!(
        ids(&Scanner::builtin(), "sa.json", t),
        vec!["gcp-service-account"]
    );
}

#[test]
fn azure_connection_string_hides_its_embedded_account_key() {
    let rule = builtin_rules()
        .into_iter()
        .find(|r| r.id() == "azure-storage-connection-string")
        .unwrap();
    assert_eq!(
        ids(&Scanner::builtin(), "a", rule.example().unwrap()),
        vec!["azure-storage-connection-string"]
    );
}

#[test]
fn jwt_requires_high_entropy_and_is_medium() {
    let s = Scanner::builtin();
    let f = s.scan(
        "a",
        b"t=eyJhbGciOiJIUzI1NiIsInR5cCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk5yJV_adQssw5c",
    );
    assert_eq!(f[0].confidence, Confidence::Medium);
    let flat = format!("eyJ{0}.eyJ{0}.{0}", "a".repeat(30));
    assert!(s.scan("a", flat.as_bytes()).is_empty());
}

#[test]
fn generic_assignments_are_low_confidence_and_entropy_gated() {
    let s = Scanner::builtin();
    let f = s.scan("a", b"api_key: \"Zq8Lw3Vn7Rt2Xb5H\"\n");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].confidence, Confidence::Low);
    assert!(s.scan("a", b"password = \"password123\"\n").is_empty());
    assert!(s.scan("a", b"token = \"${TOKEN_FROM_ENV}\"\n").is_empty());
    assert!(s.scan("a", b"let token = next_token();\n").is_empty());
    let env = s.scan("a", b"SECRET_KEY=Zq8Lw3Vn7Rt2Xb5H9k\n");
    assert_eq!(env[0].rule_id, "generic-secret-assignment");
}

#[test]
fn provider_context_rules_need_their_context() {
    let s = Scanner::builtin();
    let sid = concat!("SK", "0123456789abcdef0123456789abcdef");
    assert!(!ids(&s, "a", &format!("id = {sid}")).contains(&"twilio-api-key-sid".into()));
    assert!(ids(&s, "a", &format!("twilio_key = {sid}")).contains(&"twilio-api-key-sid".into()));
}

#[test]
fn entropy_edge_cases() {
    assert_eq!(shannon_entropy(b""), 0.0);
    assert_eq!(shannon_entropy(b"aaaa"), 0.0);
    assert!((shannon_entropy(b"abab") - 1.0).abs() < 1e-12);
    let all: Vec<u8> = (0..=255).collect();
    assert!((shannon_entropy(&all) - 8.0).abs() < 1e-12);
    // A well-formed key shape with no entropy is rejected; the same shape with entropy is not.
    let s = Scanner::builtin();
    assert!(s.scan("a", b"AKIAAAAAAAAAAAAAAAAA").is_empty());
    assert_eq!(s.scan("a", b"AKIAIOSFODNN7EXAMPLE").len(), 1);
}

#[test]
fn overlapping_keywords_still_trigger_rules() {
    // "secret" and "token" overlap here; the overlapping prefilter must report both.
    let t = b"secrettoken = \"Zq8Lw3Vn7Rt2Xb5H\"";
    assert_eq!(Scanner::builtin().scan("a", t).len(), 1);
}

#[test]
fn multiline_findings_report_the_start_line() {
    let t = "a\nb\n-----BEGIN EC PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0B\n-----END EC PRIVATE KEY-----\n";
    let f = Scanner::builtin().scan("k", t.as_bytes());
    assert_eq!(f[0].line, 3);
}
