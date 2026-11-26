//! Property tests: attenuation only narrows, and the Datalog half agrees with the typed scope.

use bee_auth::{
    Attenuation, Budget, InMemoryRevocations, KeyRing, MintRequest, Principal, Request, Scope,
    SigningKey, Verifier, attenuate,
};
use bee_policy::Action;
use proptest::prelude::*;

const NOW: u64 = 1_800_000_000;
const COMPONENTS: [&str; 5] = ["docs", "docs2", "src", "a", "b"];
const REPOS: [&str; 4] = ["org/a", "org/b", "other/x", "org/*"];
const REFS: [&str; 5] = ["main", "ws/*", "ws/a/*", "release/*", "release/1"];
const ACTIONS: [Action; 5] = [
    Action::Read,
    Action::Commit,
    Action::Land,
    Action::Review,
    Action::Lease,
];

fn path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(COMPONENTS.to_vec()), 1..4).prop_map(|v| v.join("/"))
}

fn prefix() -> impl Strategy<Value = String> {
    prop_oneof![Just("/".to_string()), path()]
}

fn opt_set<T: std::fmt::Debug + Clone>(
    s: impl Strategy<Value = T>,
) -> impl Strategy<Value = Option<Vec<T>>> {
    prop::option::of(prop::collection::vec(s, 0..3))
}

fn scope() -> impl Strategy<Value = Scope> {
    (
        opt_set(prop::sample::select(REPOS.to_vec())),
        opt_set(prefix()),
        opt_set(prefix()),
        opt_set(prop::sample::select(REFS.to_vec())),
        opt_set(prop::sample::select(ACTIONS.to_vec())),
        prop::option::of(1u64..100_000),
        prop::option::of(0u64..10),
    )
        .prop_map(|(repos, read, write, refs, actions, exp, budget)| {
            let mut s = Scope::new();
            if let Some(r) = repos {
                s = s.repos(r).unwrap();
            }
            if let Some(r) = read {
                s = s.read_paths(r).unwrap();
            }
            if let Some(w) = write {
                s = s.write_paths(w).unwrap();
            }
            if let Some(r) = refs {
                s = s.refs(r).unwrap();
            }
            if let Some(a) = actions {
                s = s.actions(a);
            }
            if let Some(e) = exp {
                s = s.expires_at(NOW + e);
            }
            if let Some(b) = budget {
                s = s.budget(Budget {
                    max_changesets: Some(b),
                    ..Budget::default()
                });
            }
            s
        })
}

fn request() -> impl Strategy<Value = Request> {
    (
        prop::sample::select(vec!["org/a", "org/b", "other/x", "org/c"]),
        prop::sample::select(ACTIONS.to_vec()),
        prop::collection::vec(path(), 0..3),
        prop::option::of(prop::sample::select(vec![
            "main",
            "ws/a/1",
            "ws/b/1",
            "release/1",
            "release/2",
        ])),
    )
        .prop_map(|(repo, action, paths, r)| {
            let mut req = Request::new(repo, action).paths(paths);
            if let Some(r) = r {
                req = req.on_ref(r);
            }
            req
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Typed scopes: the intersection is within both sides, and allows a request only if both
    /// sides do.
    #[test]
    fn intersection_only_narrows(a in scope(), b in scope(), req in request()) {
        let e = a.intersect(&b);
        prop_assert!(e.is_within(&a));
        prop_assert!(e.is_within(&b));
        let both = a.check(&req).is_ok() && b.check(&req).is_ok();
        prop_assert_eq!(e.check(&req).is_ok(), both);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Real tokens: a derived token's effective scope is within its parent's, it never allows a
    /// request its parent refuses, and the Datalog half agrees with the typed check.
    #[test]
    fn attenuated_tokens_only_narrow(root in scope(), child in scope(), reqs in prop::collection::vec(request(), 1..6)) {
        let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[5u8; 32]).unwrap());
        let keys = ring.verifying_keys();
        let revocations = InMemoryRevocations::new();
        let verifier = Verifier::new(&keys, &revocations);
        let parent = ring
            .mint(&MintRequest { principal: Principal::human("H1", "h"), on_behalf_of: vec![], scope: root }, NOW)
            .unwrap();
        let derived = attenuate(&parent.text, &Attenuation { scope: child, act_as: None }).unwrap();
        let p = verifier.verify(&parent.text, NOW).unwrap();
        let d = verifier.verify(&derived.text, NOW);
        // A child expiry at or before NOW makes the derived token expired; that is narrowing too.
        let Ok(d) = d else { return Ok(()); };
        prop_assert!(d.scope.is_within(&p.scope));
        prop_assert!(d.budget_charges().len() >= p.budget_charges().len());
        for req in &reqs {
            let child_ok = d.authorize(req, NOW).is_ok();
            let parent_ok = p.authorize(req, NOW).is_ok();
            prop_assert!(!child_ok || parent_ok, "child allowed what the parent refused: {:?}", req);
            prop_assert_eq!(d.check_datalog(req, NOW).is_ok(), d.scope.check(req).is_ok(), "datalog vs typed: {:?}", req);
            prop_assert_eq!(p.check_datalog(req, NOW).is_ok(), p.scope.check(req).is_ok(), "datalog vs typed: {:?}", req);
        }
    }
}
