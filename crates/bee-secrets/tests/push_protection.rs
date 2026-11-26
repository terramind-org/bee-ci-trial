//! Push-protection decisions and the AS-SEC-02 scenario.

use bee_core::{PayloadValue, Reason};
use bee_secrets::{Bypass, BypassReason, Scanner, evaluate_push_protection};

const RUST_FILE: &str = "const KEY: &str = \"AKIAIOSFODNN7EXAMPLE\";\nfn main() {}\n";

#[test]
fn as_sec_02_credential_is_blocked() {
    let findings = Scanner::builtin().scan("src/main.rs", RUST_FILE.as_bytes());
    assert_eq!(findings.len(), 1);

    // Without a bypass the push is refused with SECRET_DETECTED naming the rule, not the secret.
    let blocked = evaluate_push_protection(findings.clone(), None);
    assert!(blocked.blocked);
    assert!(blocked.bypass.is_none());
    let err = blocked.to_error().expect("blocked decisions have an error");
    assert_eq!(err.reason, Reason::SecretDetected);
    assert_eq!(err.reason.as_str(), "SECRET_DETECTED");
    assert!(err.message.contains("aws-access-key-id"));
    assert!(!err.message.contains("IOSFODNN7EXAMPLE"));
    assert!(!format!("{err:?}").contains("IOSFODNN7EXAMPLE"));
    assert_eq!(
        err.payload.get("rules"),
        Some(&PayloadValue::List(vec![PayloadValue::from(
            "aws-access-key-id"
        )]))
    );

    // With a recorded bypass the push proceeds and the bypass is returned for the audit log.
    let bypass = Bypass {
        principal: "agent:release-bot".into(),
        reason: BypassReason::UsedInTests,
    };
    let allowed = evaluate_push_protection(findings.clone(), Some(bypass.clone()));
    assert!(!allowed.blocked);
    assert_eq!(allowed.bypass, Some(bypass));
    assert_eq!(allowed.findings, findings);
    assert!(allowed.to_error().is_none());
}

#[test]
fn blank_principal_is_not_a_bypass() {
    let findings = Scanner::builtin().scan("a.rs", RUST_FILE.as_bytes());
    let d = evaluate_push_protection(
        findings,
        Some(Bypass {
            principal: "  ".into(),
            reason: BypassReason::FalsePositive,
        }),
    );
    assert!(d.blocked);
}

#[test]
fn clean_pushes_pass_and_unused_bypasses_are_not_recorded() {
    let d = evaluate_push_protection(
        Vec::new(),
        Some(Bypass {
            principal: "p".into(),
            reason: BypassReason::WillFixLater,
        }),
    );
    assert!(!d.blocked);
    assert!(d.bypass.is_none());
    assert!(d.findings.is_empty());
}

#[test]
fn low_confidence_findings_are_reported_but_do_not_block() {
    let findings = Scanner::builtin().scan("a.py", b"password = \"q8Zr2vLx9Tm4Kp7W\"\n");
    assert_eq!(findings.len(), 1);
    let d = evaluate_push_protection(findings, None);
    assert!(!d.blocked);
    assert_eq!(d.findings.len(), 1);
    assert!(d.to_error().is_none());
}

#[test]
fn bypass_reasons_are_enumerated_with_stable_strings() {
    let names: Vec<&str> = BypassReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(names, ["false_positive", "used_in_tests", "will_fix_later"]);
    for r in BypassReason::ALL {
        assert_eq!(BypassReason::parse(r.as_str()), Some(r));
    }
    assert_eq!(BypassReason::parse("because"), None);
}

#[test]
fn inline_allow_prevents_the_block_without_a_bypass() {
    let text = "const KEY: &str = \"AKIAIOSFODNN7EXAMPLE\"; // bee:allow-secret\n";
    let d = evaluate_push_protection(
        Scanner::builtin().scan("src/main.rs", text.as_bytes()),
        None,
    );
    assert!(!d.blocked);
}
