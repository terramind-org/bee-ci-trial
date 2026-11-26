//! Canonical form of the effective policy and the policy epoch.
//!
//! The canonical form is a sequence of tokens. Every token is written as its UTF-8 byte length
//! in decimal, a `:`, and the bytes, so the encoding is injective without escaping. The stream
//! starts with the token `swarm-policy-canon-v1` and then encodes the effective policy section
//! by section in a fixed order, each section introduced by its name and closed by `end`:
//!
//! `codeowners`, `groups`, `roles`, `members`, `access`, `refs`, `land`, `owners`, `push`,
//! `paths`, `rulesets`, `codeowners-file`.
//!
//! Inside a section, keyed entries appear in ascending key order (byte order of the normalized
//! key; ref patterns order exact names before prefixes, then by text) and sets in ascending
//! order. Paths are normalized prefixes rendered with one leading `/`; globs in their
//! normalized text; absent optional values are the token `-`; booleans `0`/`1`; numbers in
//! decimal. Keys added after v1 (`code_scanning`, `require_agent_review`, ruleset
//! `repo_properties`) are written only when set, behind tokens starting with `:`. Because comments, whitespace, key order, list order, duplicate list entries and
//! path spelling never reach the model, they never change the epoch.
//!
//! The epoch is the first 8 bytes of BLAKE3 over the canonical form, read as a big-endian
//! `u64` and shown in decimal.

use std::collections::BTreeSet;

use crate::model::{ApproverSet, PushRules, RuleBody};
use crate::owners::OwnerRef;
use crate::path::{Glob, display_prefix};
use crate::policy::Policy;
use crate::subject::Subject;

struct W(Vec<u8>);

impl W {
    fn t(&mut self, s: &str) {
        self.0.extend_from_slice(s.len().to_string().as_bytes());
        self.0.push(b':');
        self.0.extend_from_slice(s.as_bytes());
    }

    fn n(&mut self, v: u64) {
        self.t(&v.to_string());
    }

    fn b(&mut self, v: bool) {
        self.t(if v { "1" } else { "0" });
    }

    fn opt_n(&mut self, v: Option<u64>) {
        match v {
            Some(v) => self.n(v),
            None => self.t("-"),
        }
    }

    fn list<I: IntoIterator<Item = String>>(&mut self, items: I) {
        self.t("[");
        for i in items {
            self.t(&i);
        }
        self.t("]");
    }

    fn subjects(&mut self, s: &BTreeSet<Subject>) {
        self.list(s.iter().map(ToString::to_string));
    }

    fn opt_subjects(&mut self, s: &Option<BTreeSet<Subject>>) {
        match s {
            Some(s) => self.subjects(s),
            None => self.t("-"),
        }
    }

    fn owners(&mut self, s: &BTreeSet<OwnerRef>) {
        self.list(s.iter().map(ToString::to_string));
    }

    fn globs(&mut self, s: &BTreeSet<Glob>) {
        self.list(s.iter().map(|g| {
            let mut t = g.as_str().to_string();
            if g.is_dir_only() {
                t.push('/');
            }
            t
        }));
    }

    fn push_rules(&mut self, p: &PushRules) {
        self.globs(&p.deny_paths);
        self.list(p.deny_extensions.iter().cloned());
        self.opt_n(p.max_file_size);
        self.opt_n(p.max_path_length);
    }

    fn body(&mut self, b: &RuleBody) {
        self.opt_subjects(&b.allow);
        self.list(b.required_checks.iter().cloned());
        self.t("approvals");
        for a in &b.approvals {
            self.list(a.classes.iter().map(|c| c.as_str().to_string()));
            self.list(a.paths.iter().map(|p| display_prefix(p)));
            self.n(u64::from(a.count));
            match &a.from {
                ApproverSet::Any => self.t("any"),
                ApproverSet::Owners => self.t("owners"),
                ApproverSet::Subjects(s) => {
                    self.t("subjects");
                    self.subjects(s);
                }
            }
            self.b(a.include_agents);
        }
        self.t("end");
        self.list(
            b.agents_may_land_without_human
                .iter()
                .map(|c| c.as_str().to_string()),
        );
        self.b(b.require_signed);
        self.b(b.require_linear);
        self.b(b.dismiss_stale_approvals);
        self.b(b.require_last_push_approval);
        self.push_rules(&b.push);
        // Later additions are written only when set, so policies that do not use them keep
        // their epoch; the leading `:` cannot start a ref pattern or a ruleset name.
        if !b.code_scanning.is_empty() {
            self.t(":code_scanning");
            for c in &b.code_scanning {
                self.t(&c.tool);
                self.t(c.alerts.as_str());
                self.t(c.security_alerts.as_str());
            }
            self.t("end");
        }
        if b.require_agent_review {
            self.t(":agent_review");
        }
    }
}

pub(crate) fn canonical_bytes(p: &Policy) -> Vec<u8> {
    let d = &p.doc;
    let mut w = W(Vec::new());
    w.t("swarm-policy-canon-v1");

    w.t("codeowners");
    match &d.codeowners {
        Some(c) => w.t(&display_prefix(c)),
        None => w.t("-"),
    }
    w.t("end");

    w.t("groups");
    for (name, g) in &d.groups {
        w.t(name);
        w.subjects(&g.members);
    }
    w.t("end");

    w.t("roles");
    for (name, r) in &d.roles {
        w.t(name);
        match &r.base {
            Some(b) => w.t(b),
            None => w.t("-"),
        }
        w.list(r.actions.iter().map(|a| a.as_str().to_string()));
    }
    w.t("end");

    w.t("members");
    for (role, m) in &d.members {
        w.t(role);
        w.subjects(&m.subjects);
    }
    w.t("end");

    w.t("access");
    for (prefix, r) in &d.access {
        w.t(&display_prefix(prefix));
        w.opt_subjects(&r.read);
        w.opt_subjects(&r.write);
    }
    w.t("end");

    w.t("refs");
    for (pattern, r) in &d.refs {
        w.t(&pattern.to_string());
        w.opt_subjects(&r.create);
        w.opt_subjects(&r.update);
        w.opt_subjects(&r.delete);
        w.opt_subjects(&r.force);
    }
    w.t("end");

    w.t("land");
    for (pattern, r) in &d.land {
        w.t(&pattern.to_string());
        w.body(&r.body);
    }
    w.t("end");

    w.t("owners");
    for (prefix, r) in &d.owners {
        w.t(&display_prefix(prefix));
        w.owners(&r.owners);
    }
    w.t("end");

    w.t("push");
    w.push_rules(&d.push);
    w.t("end");

    w.t("paths");
    w.globs(&d.paths.sensitive);
    w.globs(&d.paths.generated);
    w.globs(&d.paths.exclusive_lease);
    w.t("end");

    w.t("rulesets");
    for (name, r) in &d.rulesets {
        w.t(name);
        w.t(r.mode.as_str());
        w.list(r.refs.iter().map(ToString::to_string));
        w.subjects(&r.bypass);
        w.body(&r.body);
        if !r.repo_properties.is_empty() {
            w.t(":repo_properties");
            for (k, v) in &r.repo_properties {
                w.t(k);
                w.list(v.iter().cloned());
            }
            w.t("end");
        }
    }
    w.t("end");

    w.t("codeowners-file");
    if let Some(co) = &p.codeowners {
        for e in co.entries() {
            w.t(&e.pattern);
            w.owners(&e.owners);
        }
    }
    w.t("end");
    w.0
}

pub(crate) fn epoch_of(p: &Policy) -> u64 {
    first_u64(blake3::hash(&canonical_bytes(p)).as_bytes())
}

fn first_u64(digest: &[u8; 32]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(b)
}

/// The repo epoch carried in the `bee-policy-epoch` trailer (SPEC §9.5): the first 8 bytes,
/// big-endian, of BLAKE3 over `swarm-epoch-v1`, a zero byte, the policy epoch (8 bytes
/// big-endian) and the repo's revocation counter (8 bytes big-endian).
pub fn repo_epoch(policy_epoch: u64, revocation_counter: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"swarm-epoch-v1\0");
    h.update(&policy_epoch.to_be_bytes());
    h.update(&revocation_counter.to_be_bytes());
    first_u64(h.finalize().as_bytes())
}
