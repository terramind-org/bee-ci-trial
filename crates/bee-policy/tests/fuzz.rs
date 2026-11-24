//! Fuzzing of the policy parsers (PRODUCT §4.25): `.beehive/policy.toml`, CODEOWNERS, path
//! globs and ref patterns never panic on arbitrary text or on random assemblies of the
//! format's own fragments, and a parse failure is always a typed `PolicyError`.

use bee_policy::{Action, Codeowners, Glob, Origin, Policy, RefPattern, parse_doc};
use proptest::prelude::*;

/// Pieces of real policy files, assembled in random orders.
const FRAGMENTS: &[&str] = &[
    "version = 1\n",
    "version = 2\n",
    "[members]\n",
    "admin = [\"alice\"]\n",
    "write = [\"@humans\", \"@agents\"]\n",
    "read = [\"@everyone\"]\n",
    "[groups]\nteam = [\"@team\", \"bob\"]\n",
    "[[access]]\n",
    "path = \"/\"\n",
    "path = \"/secret/**\"\n",
    "path = \"\"\n",
    "read = [\"@humans\"]\n",
    "write = []\n",
    "[[land]]\n",
    "ref = \"main\"\n",
    "ref = \"release/*\"\n",
    "required_approvals = 2\n",
    "required_checks = [\"ci/test\"]\n",
    "agents_may_land_without_human = [\"docs\", \"logic\"]\n",
    "require_signed = true\n",
    "[[rulesets]]\nname = \"r\"\nenforcement = \"active\"\n",
    "[[rulesets.rules]]\ntype = \"pull_request\"\n",
    "[roles.custom]\nactions = [\"read\", \"commit\"]\n",
    "[owners]\n\"/src\" = [\"@team\"]\n",
    "[[push_rules]]\nmax_file_bytes = 10\n",
    "= = =\n",
    "[[\n",
    "\"unterminated\n",
    "x = { a = 1 }\n",
    "ref = 5\n",
    "\u{0}\u{7f}\n",
];

const CODEOWNERS_FRAGMENTS: &[&str] = &[
    "* @org/team\n",
    "/src/ @alice\n",
    "*.rs @bob @org/rust\n",
    "# comment\n",
    "docs/** user@example.com\n",
    "!negated @x\n",
    "[Section]\n",
    "path\\ with\\ spaces @y\n",
    "\n",
    "/a/b/c\n",
    "@ @ @\n",
];

fn assembled(pieces: &'static [&'static str]) -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::sample::select(pieces), 0..24).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn policy_files_never_panic(text in prop_oneof![any::<String>(), assembled(FRAGMENTS)]) {
        for origin in [Origin::Repo, Origin::Org] {
            let _ = parse_doc(&text, origin);
        }
        if let Ok(p) = Policy::parse(&text) {
            // A policy that parsed answers questions without panicking either.
            let _ = p.doc();
        }
        let _ = Policy::from_sources(Some(&text), Some(&text));
    }

    #[test]
    fn codeowners_never_panic(text in prop_oneof![any::<String>(), assembled(CODEOWNERS_FRAGMENTS)]) {
        let _ = Codeowners::parse(&text);
    }

    #[test]
    fn globs_patterns_and_names_never_panic(text in any::<String>()) {
        let _ = Glob::parse(&text);
        let _ = Glob::parse_dir_only(&text);
        let _ = RefPattern::parse(&text);
        let _ = Action::parse(&text);
    }
}
