//! Tokens end to end: minting, offline attenuation, verification, revocation, rotation,
//! budgets and quotas.

use bee_auth::{
    AgentInfo, Attenuation, AuthReason, Budget, BudgetLedger, DEFAULT_TOKEN_LIFETIME_SECS,
    InMemoryLedger, InMemoryRevocations, KeyRing, MintRequest, Principal, PrincipalRef, Request,
    RevocationList, Scope, SigningKey, Usage, Verifier, VerifyingKeys, attenuate,
};
use bee_policy::{Action, Policy, PrincipalId, PrincipalKind};
use biscuit_auth::{BlockBuilder, UnverifiedBiscuit};

const NOW: u64 = 1_800_000_000;

fn ring() -> KeyRing {
    KeyRing::new(SigningKey::from_secret_bytes(1, &[7u8; 32]).unwrap())
}

fn human() -> Principal {
    Principal::human("H1", "Ada")
}

fn agent(id: &str, owner: &str) -> Principal {
    Principal::agent(
        id,
        "helper",
        AgentInfo {
            owner: PrincipalId::new(owner),
            runtime: "claude-code".into(),
            purpose: "docs".into(),
        },
    )
}

fn mint_human(ring: &KeyRing, scope: Scope) -> String {
    ring.mint(
        &MintRequest {
            principal: human(),
            on_behalf_of: vec![],
            scope,
        },
        NOW,
    )
    .unwrap()
    .text
}

fn full_scope() -> Scope {
    Scope::new().repos(["repo1"]).unwrap()
}

const POLICY: &str = r#"
version = 1
[members]
write = ["@humans", "@agents"]
[[access]]
path = "/"
read = ["@everyone"]
write = ["@humans", "@agents"]
"#;

/// AS-AUTH-01: mint a token for a sub-agent limited to `docs/` with a budget of five
/// changesets; its sixth commit and any write outside `docs/` are refused with the right
/// reasons.
#[test]
fn as_auth_01_subagent_limited_to_docs_with_budget_five() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let ledger = InMemoryLedger::new();
    let policy = Policy::parse(POLICY).unwrap();

    // The human's agent gets a repo-wide token from the server.
    let a1 = agent("A1", "H1");
    let agent_token = ring
        .mint(
            &MintRequest {
                principal: a1.clone(),
                on_behalf_of: vec![human().to_ref()],
                scope: full_scope(),
            },
            NOW,
        )
        .unwrap();

    // The agent mints a narrower token for its sub-agent, offline.
    let sub = attenuate(
        &agent_token.text,
        &Attenuation {
            scope: Scope::new()
                .write_paths(["docs/"])
                .unwrap()
                .budget(Budget {
                    max_changesets: Some(5),
                    ..Budget::default()
                })
                .session("sub-session-1"),
            act_as: Some(PrincipalRef::new("A2", PrincipalKind::Agent)),
        },
    )
    .unwrap();

    let vt = verifier.verify(&sub.text, NOW + 60).unwrap();
    assert_eq!(vt.principal.id, "A2");
    assert_eq!(
        vt.on_behalf_of
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["A1", "H1"]
    );
    assert_eq!(vt.delegation_root().id, "H1");
    assert_eq!(vt.session(), Some("sub-session-1"));

    let chain = vt.policy_chain(|_| (Vec::new(), Vec::new()));
    let commit = |path: &str| -> Result<(), bee_auth::AuthError> {
        let req = Request::new("repo1", Action::Commit).paths([path]);
        vt.authorize(&req, NOW + 60)?;
        assert!(
            policy
                .check_access_chain(&chain, Action::Commit, path)
                .allowed
        );
        ledger.charge(&vt.budget_charges(), Usage::changeset())
    };

    for i in 0..5 {
        commit(&format!("docs/page{i}.md")).unwrap();
    }
    let sixth = commit("docs/page5.md").unwrap_err();
    assert_eq!(sixth.reason, AuthReason::BudgetExhausted);
    assert_eq!(sixth.reason.as_str(), "BUDGET_EXHAUSTED");

    // Outside docs/: refused by the token even though repo policy would allow it.
    let outside = Request::new("repo1", Action::Commit).paths(["src/main.rs"]);
    let err = vt.authorize(&outside, NOW + 60).unwrap_err();
    assert_eq!(err.reason, AuthReason::PolicyDenied);
    assert_eq!(err.reason.as_str(), "POLICY_DENIED");
    assert!(
        policy
            .check_access_chain(&chain, Action::Commit, "src/main.rs")
            .allowed
    );
    // `docs2/` is not under `docs/`.
    let near = Request::new("repo1", Action::Commit).paths(["docs2/a.md"]);
    assert_eq!(
        vt.authorize(&near, NOW + 60).unwrap_err().reason,
        AuthReason::PolicyDenied
    );
    // The Datalog half refuses it on its own too.
    assert!(vt.check_datalog(&outside, NOW + 60).is_err());

    // The parent agent's own token is unaffected by the sub-agent's budget.
    let parent = verifier.verify(&agent_token.text, NOW + 60).unwrap();
    assert!(parent.budget_charges().is_empty());
    assert!(parent.authorize(&outside, NOW + 60).is_ok());
}

#[test]
fn revocation_cascades_to_derived_tokens_only() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let root = ring
        .mint(
            &MintRequest {
                principal: human(),
                on_behalf_of: vec![],
                scope: full_scope(),
            },
            NOW,
        )
        .unwrap();
    let narrow = |t: &str, p: &str| {
        attenuate(
            t,
            &Attenuation {
                scope: Scope::new().write_paths([p]).unwrap(),
                act_as: None,
            },
        )
        .unwrap()
    };
    let child = narrow(&root.text, "docs");
    let grandchild = narrow(&child.text, "docs/api");
    let sibling = narrow(&root.text, "src");
    assert_ne!(child.id, root.id);

    assert!(revocations.revoke(&child.id));
    assert!(!revocations.revoke(&child.id), "second revoke is a no-op");
    assert_eq!(revocations.counter(), 1);
    assert_eq!(
        verifier.verify(&child.text, NOW).unwrap_err().reason,
        AuthReason::TokenRevoked
    );
    assert_eq!(
        verifier.verify(&grandchild.text, NOW).unwrap_err().reason,
        AuthReason::TokenRevoked
    );
    assert!(verifier.verify(&root.text, NOW).is_ok());
    assert!(verifier.verify(&sibling.text, NOW).is_ok());

    revocations.revoke(&root.id);
    assert_eq!(
        verifier.verify(&sibling.text, NOW).unwrap_err().reason,
        AuthReason::TokenRevoked
    );
    assert_eq!(
        verifier.verify(&root.text, NOW).unwrap_err().reason,
        AuthReason::TokenRevoked
    );
    assert_eq!(revocations.counter(), 2);
    // The verified token lists the ids whose revocation would kill it.
    let fresh = narrow(&mint_human(&ring, full_scope()), "docs");
    let vt = verifier.verify(&fresh.text, NOW).unwrap();
    assert_eq!(vt.block_ids().len(), 2);
    assert_eq!(vt.token_id, fresh.id);
}

#[test]
fn key_rotation_first_signs_all_verify() {
    let mut ring = ring();
    let old = ring
        .mint(
            &MintRequest {
                principal: human(),
                on_behalf_of: vec![],
                scope: full_scope(),
            },
            NOW,
        )
        .unwrap();
    ring.rotate(SigningKey::from_secret_bytes(2, &[9u8; 32]).unwrap())
        .unwrap();
    assert!(
        ring.rotate(SigningKey::from_secret_bytes(2, &[3u8; 32]).unwrap())
            .is_err(),
        "ids are unique"
    );
    let new = ring
        .mint(
            &MintRequest {
                principal: human(),
                on_behalf_of: vec![],
                scope: full_scope(),
            },
            NOW,
        )
        .unwrap();
    let revocations = InMemoryRevocations::new();

    let keys = ring.verifying_keys();
    let verifier = Verifier::new(&keys, &revocations);
    assert_eq!(verifier.verify(&old.text, NOW).unwrap().key_id, 1);
    assert_eq!(verifier.verify(&new.text, NOW).unwrap().key_id, 2);

    // A verifier that only knows key 2 rejects old tokens.
    let mut only_new = VerifyingKeys::new();
    only_new.insert(2, ring.signer().public_key());
    let v2 = Verifier::new(&only_new, &revocations);
    assert_eq!(
        v2.verify(&old.text, NOW).unwrap_err().reason,
        AuthReason::Unauthenticated
    );
    assert!(v2.verify(&new.text, NOW).is_ok());

    assert!(ring.retire(2).is_err(), "cannot retire the signing key");
    ring.retire(1).unwrap();
    let keys = ring.verifying_keys();
    let verifier = Verifier::new(&keys, &revocations);
    assert_eq!(
        verifier.verify(&old.text, NOW).unwrap_err().reason,
        AuthReason::Unauthenticated
    );
    assert!(verifier.verify(&new.text, NOW).is_ok());
}

#[test]
fn default_lifetime_is_twelve_hours_and_attenuation_cannot_extend_it() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let t = mint_human(&ring, full_scope());
    assert_eq!(DEFAULT_TOKEN_LIFETIME_SECS, 43_200);
    let vt = verifier.verify(&t, NOW).unwrap();
    assert_eq!(vt.scope.expires_at, Some(NOW + DEFAULT_TOKEN_LIFETIME_SECS));
    assert!(
        verifier
            .verify(&t, NOW + DEFAULT_TOKEN_LIFETIME_SECS - 1)
            .is_ok()
    );
    assert_eq!(
        verifier
            .verify(&t, NOW + DEFAULT_TOKEN_LIFETIME_SECS)
            .unwrap_err()
            .reason,
        AuthReason::TokenExpired
    );
    let longer = attenuate(
        &t,
        &Attenuation {
            scope: Scope::new().expires_at(NOW + 999_999),
            act_as: None,
        },
    )
    .unwrap();
    assert_eq!(
        verifier
            .verify(&longer.text, NOW + DEFAULT_TOKEN_LIFETIME_SECS)
            .unwrap_err()
            .reason,
        AuthReason::TokenExpired
    );
    let shorter = attenuate(
        &t,
        &Attenuation {
            scope: Scope::new().expires_at(NOW + 10),
            act_as: None,
        },
    )
    .unwrap();
    assert_eq!(
        verifier.verify(&shorter.text, NOW + 10).unwrap_err().reason,
        AuthReason::TokenExpired
    );
    // An already-verified token refuses requests once expired.
    let vt = verifier.verify(&shorter.text, NOW).unwrap();
    let req = Request::new("repo1", Action::Read);
    assert_eq!(
        vt.authorize(&req, NOW + 10).unwrap_err().reason,
        AuthReason::TokenExpired
    );
    // Minting with an expiry in the past is refused.
    let past = ring.mint(
        &MintRequest {
            principal: human(),
            on_behalf_of: vec![],
            scope: Scope::new().expires_at(NOW),
        },
        NOW,
    );
    assert_eq!(past.unwrap_err().reason, AuthReason::InvalidArgument);
}

#[test]
fn forged_and_foreign_tokens_are_unauthenticated() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let t = mint_human(&ring, full_scope());
    let mut bytes = t.clone().into_bytes();
    let mid = bytes.len() / 2;
    bytes[mid] = if bytes[mid] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(bytes).unwrap();
    assert_eq!(
        verifier.verify(&tampered, NOW).unwrap_err().reason,
        AuthReason::Unauthenticated
    );
    assert_eq!(
        verifier.verify("not a token", NOW).unwrap_err().reason,
        AuthReason::Unauthenticated
    );
    let other = KeyRing::new(SigningKey::from_secret_bytes(1, &[8u8; 32]).unwrap());
    let foreign = mint_human(&other, full_scope());
    assert_eq!(
        verifier.verify(&foreign, NOW).unwrap_err().reason,
        AuthReason::Unauthenticated
    );
}

#[test]
fn blocks_without_a_bee_scope_are_rejected_and_datalog_is_enforced() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let t = mint_human(&ring, full_scope());
    // A raw Biscuit block with no context: the effective scope could not include it.
    let raw = UnverifiedBiscuit::from_base64(&t).unwrap();
    let bare = raw
        .append(
            BlockBuilder::new()
                .code("reject if write_path($p)")
                .unwrap(),
        )
        .unwrap();
    let err = verifier
        .verify(&bare.to_base64().unwrap(), NOW)
        .unwrap_err();
    assert_eq!(err.reason, AuthReason::Unauthenticated);
    // A block whose context claims no restriction but whose Datalog forbids writes: the Datalog
    // still applies, so the holder cannot hide a restriction.
    let ctx = r#"{"v":1,"kind":"attenuation","scope":{}}"#.to_string();
    let sneaky = raw
        .append(
            BlockBuilder::new()
                .context(ctx)
                .code("reject if write_path($p)")
                .unwrap(),
        )
        .unwrap();
    let vt = verifier.verify(&sneaky.to_base64().unwrap(), NOW).unwrap();
    let write = Request::new("repo1", Action::Commit).paths(["a"]);
    assert_eq!(
        vt.authorize(&write, NOW).unwrap_err().reason,
        AuthReason::PolicyDenied
    );
    assert!(
        vt.authorize(&Request::new("repo1", Action::Read).paths(["a"]), NOW)
            .is_ok()
    );
    // A later block cannot change the principal.
    let ctx = r#"{"v":1,"kind":"attenuation","principal":{"id":"root","kind":"human"},"scope":{}}"#;
    let hijack = raw
        .append(BlockBuilder::new().context(ctx.to_string()))
        .unwrap();
    assert_eq!(
        verifier
            .verify(&hijack.to_base64().unwrap(), NOW)
            .unwrap_err()
            .reason,
        AuthReason::Unauthenticated
    );
}

#[test]
fn delegation_rules() {
    let ring = ring();
    // An agent token must be minted on behalf of the agent's owner.
    let a1 = agent("A1", "H1");
    let r = ring.mint(
        &MintRequest {
            principal: a1.clone(),
            on_behalf_of: vec![],
            scope: Scope::new(),
        },
        NOW,
    );
    assert_eq!(r.unwrap_err().reason, AuthReason::InvalidArgument);
    let wrong = PrincipalRef::new("H2", PrincipalKind::Human);
    let r = ring.mint(
        &MintRequest {
            principal: a1.clone(),
            on_behalf_of: vec![wrong],
            scope: Scope::new(),
        },
        NOW,
    );
    assert!(r.is_err());
    // Only agents can be named in act_as.
    let t = mint_human(&ring, full_scope());
    let r = attenuate(
        &t,
        &Attenuation {
            scope: Scope::new(),
            act_as: Some(PrincipalRef::new("H9", PrincipalKind::Human)),
        },
    );
    assert_eq!(r.unwrap_err().reason, AuthReason::InvalidArgument);
}

#[test]
fn ref_repo_and_action_limits() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let t = mint_human(&ring, Scope::new().repos(["org/*"]).unwrap());
    let d = attenuate(
        &t,
        &Attenuation {
            scope: Scope::new()
                .refs(["ws/H1/*"])
                .unwrap()
                .actions([Action::Read, Action::Commit])
                .repos(["org/a", "other/x"])
                .unwrap(),
            act_as: None,
        },
    )
    .unwrap();
    let vt = verifier.verify(&d.text, NOW).unwrap();
    let ok = Request::new("org/a", Action::Commit)
        .paths(["x"])
        .on_ref("ws/H1/w");
    assert!(vt.authorize(&ok, NOW).is_ok());
    for bad in [
        Request::new("org/a", Action::Commit).on_ref("main"),
        Request::new("org/a", Action::Land).on_ref("ws/H1/w"),
        Request::new("other/x", Action::Read),
        Request::new("org/b", Action::Read),
    ] {
        assert_eq!(
            vt.authorize(&bad, NOW).unwrap_err().reason,
            AuthReason::PolicyDenied,
            "{bad:?}"
        );
        assert!(
            vt.check_datalog(&bad, NOW).is_err(),
            "datalog agrees on {bad:?}"
        );
    }
    assert!(vt.allows_action(Action::Commit));
    assert!(!vt.allows_action(Action::Land));
}

#[test]
fn quotas_are_shared_per_delegation_root() {
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let ledger = InMemoryLedger::new();
    let per_principal = Budget {
        max_changesets: Some(100),
        ..Budget::default()
    };
    let per_root = Budget {
        max_changesets: Some(3),
        ..Budget::default()
    };
    let tokens: Vec<_> = ["A1", "A2"]
        .iter()
        .map(|id| {
            let t = ring
                .mint(
                    &MintRequest {
                        principal: agent(id, "H1"),
                        on_behalf_of: vec![human().to_ref()],
                        scope: full_scope(),
                    },
                    NOW,
                )
                .unwrap();
            verifier.verify(&t.text, NOW).unwrap()
        })
        .collect();
    let charge = |i: usize| {
        ledger.charge(
            &tokens[i].quota_charges(per_principal, per_root),
            Usage::changeset(),
        )
    };
    charge(0).unwrap();
    charge(1).unwrap();
    charge(0).unwrap();
    // Fourth changeset under H1, whichever agent makes it.
    assert_eq!(charge(1).unwrap_err().reason, AuthReason::BudgetExhausted);
    assert_eq!(tokens[0].delegation_root().id, "H1");
}

/// SPEC §9.2: `authorize_repo` names a repo by full name, or by id only when the token lists
/// that exact id. Patterns are never matched against an id, across attenuation too.
#[test]
fn repo_ids_are_accepted_only_as_exact_entries() {
    const ID: &str = "01JAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER: &str = "01JBBBBBBBBBBBBBBBBBBBBBBB";
    let ring = ring();
    let keys = ring.verifying_keys();
    let revocations = InMemoryRevocations::new();
    let verifier = Verifier::new(&keys, &revocations);
    let check = |text: &str, ref_name: Option<&str>| {
        let vt = verifier.verify(text, NOW).unwrap();
        vt.authorize_repo(
            "org/app",
            ID,
            |key| {
                let r = Request::new(key, Action::Commit);
                match ref_name {
                    Some(n) => r.on_ref(n),
                    None => r,
                }
            },
            NOW,
        )
        .map_err(|e| (e.reason, e.message))
    };
    let mint = |repos: &[&str]| mint_human(&ring, Scope::new().repos(repos).unwrap());

    assert!(check(&mint(&["org/app"]), None).is_ok());
    assert!(check(&mint(&[ID]), None).is_ok());
    assert!(check(&mint(&["org/*"]), None).is_ok());
    assert!(check(&mint_human(&ring, Scope::new()), None).is_ok());
    for repos in [&[OTHER][..], &["01J*"], &["01*"], &["org/other"]] {
        let (reason, message) = check(&mint(repos), None).unwrap_err();
        assert_eq!(reason, AuthReason::PolicyDenied, "{repos:?}");
        // The refusal names the repo by full name.
        assert!(message.contains("org/app"), "{repos:?}: {message}");
    }
    // An id next to a name pattern is still listed exactly.
    let s = Scope::new().repos([ID, "org/*"]).unwrap();
    assert!(s.lists_repo_id(ID));
    assert!(!s.lists_repo_id(OTHER));
    assert!(check(&mint(&[ID, "ops/*"]), None).is_ok());
    assert!(!Scope::new().repos(["01*"]).unwrap().lists_repo_id(ID));
    assert!(!Scope::new().lists_repo_id(ID), "unrestricted lists no id");
    assert!(!Scope::new().repos([ID]).unwrap().lists_repo_id(""));

    // Attenuation: an unrestricted parent narrowed to the id keeps the id; other dimensions
    // still apply to the id form.
    let root = mint_human(&ring, Scope::new());
    let narrowed = |scope: Scope| {
        attenuate(
            &root,
            &Attenuation {
                scope,
                act_as: None,
            },
        )
        .unwrap()
        .text
    };
    let child = narrowed(Scope::new().repos([ID]).unwrap());
    assert_eq!(
        verifier.verify(&child, NOW).unwrap().scope.repos,
        Scope::new().repos([ID]).unwrap().repos,
        "the effective scope reports the id as given"
    );
    assert!(check(&child, None).is_ok());
    let child = narrowed(Scope::new().repos([ID]).unwrap().refs(["main"]).unwrap());
    assert!(check(&child, Some("main")).is_ok());
    assert_eq!(
        check(&child, Some("dev")).unwrap_err().0,
        AuthReason::PolicyDenied
    );
    // A parent naming another repo's id cannot be widened to this one by a child.
    let other = mint(&[OTHER]);
    let child = attenuate(
        &other,
        &Attenuation {
            scope: Scope::new().repos([ID]).unwrap(),
            act_as: None,
        },
    )
    .unwrap();
    assert_eq!(
        check(&child.text, None).unwrap_err().0,
        AuthReason::PolicyDenied
    );
}
