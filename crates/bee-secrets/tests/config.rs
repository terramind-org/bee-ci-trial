//! Allow-lists, inline allow, size and binary skipping, custom TOML rules.

use bee_secrets::Scanner;

const AWS: &str = "id = \"AKIAIOSFODNN7EXAMPLE\"\n";
const NPM: &[u8] = b"npm_aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH3jL5";

#[test]
fn per_path_allow_list() {
    let s = Scanner::from_toml("[[allow]]\npaths = [\"tests/fixtures/**\", \"*.md\"]\n").unwrap();
    assert!(s.scan("tests/fixtures/a/b.txt", AWS.as_bytes()).is_empty());
    assert!(s.scan("docs/guide.md", AWS.as_bytes()).is_empty());
    assert_eq!(s.scan("src/lib.rs", AWS.as_bytes()).len(), 1);
}

#[test]
fn per_rule_allow_list_only_silences_that_rule() {
    let s = Scanner::from_toml("[[allow]]\nrules = [\"aws-access-key-id\"]\n").unwrap();
    assert!(s.scan("a.rs", AWS.as_bytes()).is_empty());
    assert_eq!(s.scan("a.rs", NPM).len(), 1);
}

#[test]
fn path_and_rule_allow_entries_combine_with_and() {
    let s = Scanner::from_toml(
        "[[allow]]\npaths = [\"fixtures/**\"]\nrules = [\"aws-access-key-id\"]\n",
    )
    .unwrap();
    assert!(s.scan("fixtures/a.rs", AWS.as_bytes()).is_empty());
    assert_eq!(s.scan("src/a.rs", AWS.as_bytes()).len(), 1);
    assert_eq!(s.scan("fixtures/a.rs", NPM).len(), 1);
}

#[test]
fn inline_allow_applies_to_its_own_line_only() {
    let s = Scanner::builtin();
    let text = "a = \"AKIAIOSFODNN7EXAMPLE\" // bee:allow-secret\nb = \"AKIAIOSFODNN7EXAMPLE\"\n";
    let f = s.scan("a.rs", text.as_bytes());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].line, 2);
    let above = "// bee:allow-secret\nb = \"AKIAIOSFODNN7EXAMPLE\"\n";
    assert_eq!(s.scan("a.rs", above.as_bytes()).len(), 1);
}

#[test]
fn inline_allow_covers_multiline_blocks_on_the_first_line() {
    let t = "-----BEGIN RSA PRIVATE KEY----- # bee:allow-secret\nMIIEvQIBADANBgkqhkiG9w0B\n-----END RSA PRIVATE KEY-----\n";
    assert!(Scanner::builtin().scan("k", t.as_bytes()).is_empty());
}

#[test]
fn binary_and_oversized_files_are_skipped() {
    let s = Scanner::builtin();
    let mut bin = vec![0u8];
    bin.extend_from_slice(AWS.as_bytes());
    assert!(!s.is_scannable(&bin));
    assert!(s.scan("a.bin", &bin).is_empty());
    // A NUL after the sniff window does not make a file binary.
    let mut late = AWS.as_bytes().to_vec();
    late.resize(9000, b' ');
    late.push(0);
    assert!(s.is_scannable(&late));
    assert_eq!(s.scan("a.txt", &late).len(), 1);

    let mut big = AWS.as_bytes().to_vec();
    big.resize(bee_secrets::DEFAULT_MAX_FILE_SIZE + 1, b' ');
    assert!(s.scan("big.txt", &big).is_empty());
    big.truncate(bee_secrets::DEFAULT_MAX_FILE_SIZE);
    assert_eq!(s.scan("big.txt", &big).len(), 1);
}

#[test]
fn size_cap_is_configurable() {
    let s = Scanner::from_toml("max_file_size = 10\n").unwrap();
    assert!(s.scan("a", AWS.as_bytes()).is_empty());
    let s = Scanner::from_toml("max_file_size = 4096\n").unwrap();
    assert_eq!(s.scan("a", AWS.as_bytes()).len(), 1);
}

const CUSTOM: &str = r#"
[[rule]]
id = "acme-token"
provider = "acme"
description = "Acme API token"
regex = '\b(acme_[A-Za-z0-9]{32})\b'
keywords = ["acme_"]
entropy = 3.0
confidence = "medium"
example = "t = acme_aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH"
near_miss = "t = acme_short"
"#;

#[test]
fn custom_rules_behave_like_builtins() {
    let s = Scanner::from_toml(CUSTOM).unwrap();
    let rule = s.rules().iter().find(|r| r.id() == "acme-token").unwrap();
    rule.check_examples().unwrap();
    let f = s.scan("a", rule.example().unwrap().as_bytes());
    assert_eq!(
        (f[0].rule_id.as_str(), f[0].provider.as_str()),
        ("acme-token", "acme")
    );
    assert_eq!(f[0].confidence, bee_secrets::Confidence::Medium);
    assert!(s.scan("a", rule.near_miss().unwrap().as_bytes()).is_empty());
    // Built-ins stay on by default and can be turned off.
    assert_eq!(s.scan("a", AWS.as_bytes()).len(), 1);
    let only = Scanner::from_toml(&format!("builtin = false\n{CUSTOM}")).unwrap();
    assert!(only.scan("a", AWS.as_bytes()).is_empty());
}

#[test]
fn bad_config_is_rejected_with_invalid_argument() {
    use bee_core::Reason;
    for toml in [
        "nonsense = 1",
        "[[rule]]\nid = \"Bad_ID\"\nprovider = \"x\"\nregex = \"a\"\nkeywords = [\"a\"]",
        "[[rule]]\nid = \"x\"\nprovider = \"x\"\nregex = \"(\"\nkeywords = [\"a\"]",
        "[[rule]]\nid = \"x\"\nprovider = \"x\"\nregex = \"a\"\nkeywords = []",
        "[[rule]]\nid = \"x\"\nprovider = \"x\"\nregex = \"a\"\nkeywords = [\"a\"]\nentropy = 9.0",
        "[[rule]]\nid = \"aws-access-key-id\"\nprovider = \"x\"\nregex = \"a\"\nkeywords = [\"a\"]",
        "[[allow]]\n",
        "[[allow]]\nrules = [\"no-such-rule\"]",
        "builtin = false",
    ] {
        let err = Scanner::from_toml(toml).unwrap_err();
        assert_eq!(err.reason, Reason::InvalidArgument, "{toml}");
    }
}

#[test]
fn scanning_is_deterministic() {
    let s = Scanner::builtin();
    let text = format!("{AWS}{AWS}{}\n", String::from_utf8_lossy(NPM));
    assert_eq!(s.scan("a", text.as_bytes()), s.scan("a", text.as_bytes()));
}
