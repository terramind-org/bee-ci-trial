//! Per-method fixtures: what to call, with which request, after which setup, and what the
//! outcome must be.
//!
//! Fixtures live in `crates/bee-conformance/fixtures/`, one JSON file per service
//! (`<noun>.json`), plus `scenario.json`, the shared seed every fixture builds on. A file is
//!
//! ```json
//! {
//!   "service": "bee.v1.IssueService",
//!   "fixtures": [
//!     {
//!       "method": "CloseIssue",
//!       "setup": [{"call": "IssueService/CreateIssue", "save": "tmp",
//!                  "request": {"repo": {"fullName": "{{repo}}"}, "title": "to close"}}],
//!       "request": {"repo": {"fullName": "{{repo}}"}, "number": "{{tmp.number}}"},
//!       "expect": "ok"
//!     }
//!   ]
//! }
//! ```
//!
//! - `request` is the proto3-JSON request (for the client-streaming `UploadObjects`, an array
//!   of messages). Strings are templates, see [`crate::parity::template`].
//! - `setup` steps run through the API before the call, identically for every form, so
//!   mutating methods act on fresh state of their own. A step is an RPC (`call`), the
//!   built-in `commit` (upload files, create a changeset, optionally land it), or the built-in
//!   `seal` (seal a secret value for a scope's public key), or the built-in `upload` (send
//!   bytes to a signed URL a response handed out).
//! - `token` (on a fixture or a step) calls as another principal, e.g. an agent or a runner
//!   created in setup; every form then uses that token.
//! - `expect` is `ok` or the SPEC §12.2 reason every form must report. A fixture that expects
//!   an error says why in `note`.
//! - `volatile` lists JSON pointers (with `*` for every array element) blanked in the
//!   normalised outcome before forms are compared, for values that legitimately differ
//!   between two identical servers (a worker's progress, a measured duration).
//! - `maxItems` bounds what is compared of a server stream (open event streams need it).
//! - `save` keeps the call's response as a variable for later fixtures of the same file.

use serde::Deserialize;
use serde_json::Value;

/// One file of fixtures for one service.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFile {
    /// Fully qualified service name, e.g. `bee.v1.IssueService`.
    pub service: String,
    /// Optional prose about the service's fixtures.
    #[serde(default)]
    pub note: String,
    /// The fixtures, run in this order.
    pub fixtures: Vec<Fixture>,
}

/// The fixture of one method.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Fixture {
    /// Method name, e.g. `CreateIssue`.
    pub method: String,
    /// Steps run through the API on each form's server before the call.
    #[serde(default)]
    pub setup: Vec<Step>,
    /// The request (a template).
    #[serde(default = "empty_object")]
    pub request: Value,
    /// `ok` or a SPEC §12.2 reason.
    #[serde(default = "ok")]
    pub expect: String,
    /// JSON pointers blanked before comparison.
    #[serde(default)]
    pub volatile: Vec<String>,
    /// Compare at most this many stream items.
    #[serde(default)]
    pub max_items: Option<u64>,
    /// Calls as this bearer token (a template, e.g. `{{agent.token}}`) instead of the dev
    /// user's, in every form.
    #[serde(default)]
    pub token: Option<String>,
    /// Saves each form's response under this name for the fixtures after it in the file (each
    /// form's server keeps its own value).
    #[serde(default)]
    pub save: Option<String>,
    /// Why the fixture looks the way it does (required when `expect` is not `ok`).
    #[serde(default)]
    pub note: String,
}

/// One setup or scenario step.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Step {
    /// An RPC, `Service/Method` (short service name, e.g. `IssueService/CreateIssue`).
    #[serde(default)]
    pub call: Option<String>,
    /// The request of `call` (a template).
    #[serde(default = "empty_object")]
    pub request: Value,
    /// The built-in commit step.
    #[serde(default)]
    pub commit: Option<Commit>,
    /// The built-in seal step: seals text for a secret scope's public key.
    #[serde(default)]
    pub seal: Option<Seal>,
    /// The built-in upload step: an HTTP request to a URL the server handed out.
    #[serde(default)]
    pub upload: Option<Upload>,
    /// Calls as this bearer token (a template) instead of the dev user's.
    #[serde(default)]
    pub token: Option<String>,
    /// Stops collecting a streaming setup call after this many items.
    #[serde(default)]
    pub max_items: Option<u64>,
    /// Saves the response (or the commit's result) under this variable name.
    #[serde(default)]
    pub save: Option<String>,
    /// `ok` (default) or the reason the step is expected to fail with.
    #[serde(default = "ok")]
    pub expect: String,
}

/// Uploads files, creates a changeset on a parent, and optionally lands it on a ref or
/// creates a ref at it. Saves `{"id": <text id>, "objectId": {"raw": ...}, "changeId": {"raw": ...},
/// "changeset": ..., "blobs": {<path>: {"id", "objectId", "data" (base64)}}}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Commit {
    /// `org/name` (a template).
    pub repo: String,
    /// The parent: a ref name resolved through `ResolveRef`, or a text object id.
    pub parent: String,
    /// Path to UTF-8 content.
    pub files: std::collections::BTreeMap<String, String>,
    /// Paths to delete.
    #[serde(default)]
    pub delete: Vec<String>,
    /// Commit message.
    pub message: String,
    /// The change id's distinguishing number (change ids must be unique per repo).
    pub change: u64,
    /// Land the changeset on this ref.
    #[serde(default)]
    pub land: Option<String>,
    /// Create this ref at the changeset.
    #[serde(default)]
    pub create_ref: Option<String>,
}

/// Seals `plaintext` for an X25519 `publicKey` (base64, a template such as `{{k.key}}`) the
/// way a client prepares `SecretService.PutSecret` (libsodium sealed box). Saves
/// `{"sealed": <base64>}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Seal {
    /// The scope's public key, base64.
    pub public_key: String,
    /// The secret value.
    pub plaintext: String,
}

/// Sends bytes to a URL a response handed out (a signed upload URL for package files, workflow
/// artifacts and caches), the way a client completes such an upload. All fields are
/// templates. Saves `{"status": <HTTP status>, "body": <text>}`; any status other than 2xx
/// fails the step.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Upload {
    /// The URL.
    pub url: String,
    /// HTTP method, default `PUT`.
    #[serde(default)]
    pub method: Option<String>,
    /// The body as UTF-8 text.
    #[serde(default)]
    pub text: Option<String>,
    /// The body as base64 (instead of `text`).
    #[serde(default)]
    pub base64: Option<String>,
    /// Extra headers, name to value.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Send the dev user's (or the step's) token as `Authorization: Bearer`.
    #[serde(default)]
    pub bearer: bool,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn ok() -> String {
    "ok".to_owned()
}

impl Fixture {
    /// True when every form must succeed.
    pub fn expects_ok(&self) -> bool {
        self.expect == "ok"
    }
}

impl FixtureFile {
    /// Parses a fixture file and checks its own consistency (methods unique, errors noted).
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: FixtureFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut seen = std::collections::BTreeSet::new();
        for f in &file.fixtures {
            if !seen.insert(f.method.clone()) {
                return Err(format!("{}: two fixtures", f.method));
            }
            if !f.expects_ok() && f.note.trim().is_empty() {
                return Err(format!(
                    "{}: expects {} without a note saying why",
                    f.method, f.expect
                ));
            }
            for s in &f.setup {
                s.check().map_err(|e| format!("{}: {e}", f.method))?;
            }
        }
        Ok(file)
    }
}

impl Step {
    /// Exactly one of `call`, `commit`, `seal` and `upload`.
    pub fn check(&self) -> Result<(), String> {
        let kinds = [
            self.call.is_some(),
            self.commit.is_some(),
            self.seal.is_some(),
            self.upload.is_some(),
        ];
        if kinds.iter().filter(|k| **k).count() != 1 {
            return Err("a step has exactly one of `call`, `commit`, `seal` and `upload`".into());
        }
        match &self.call {
            Some(call) if !call.contains('/') => {
                Err(format!("step call {call:?} is not Service/Method"))
            }
            _ => Ok(()),
        }
    }
}

/// The shared scenario: steps run once, through the API, on the seed server.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Optional prose.
    #[serde(default)]
    pub note: String,
    /// Steps in order; saved names become global variables.
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Parses `scenario.json`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let s: Scenario = serde_json::from_str(text).map_err(|e| e.to_string())?;
        for step in &s.steps {
            step.check()?;
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checks() {
        let f = FixtureFile::parse(
            r#"{"service": "bee.v1.IssueService", "fixtures": [
                {"method": "ListIssues", "request": {"repo": {"fullName": "{{repo}}"}}},
                {"method": "GetIssue", "expect": "NOT_FOUND", "note": "no such issue"}
            ]}"#,
        )
        .expect("parses");
        assert_eq!(f.fixtures.len(), 2);
        assert!(f.fixtures[0].expects_ok());
        assert!(
            FixtureFile::parse(
                r#"{"service": "s", "fixtures": [{"method": "A", "expect": "NOT_FOUND"}]}"#
            )
            .is_err()
        );
        assert!(
            FixtureFile::parse(
                r#"{"service": "s", "fixtures": [{"method": "A"}, {"method": "A"}]}"#
            )
            .is_err()
        );
        assert!(
            FixtureFile::parse(r#"{"service": "s", "fixtures": [{"method": "A", "bogus": 1}]}"#)
                .is_err()
        );
        assert!(
            FixtureFile::parse(
                r#"{"service": "s", "fixtures": [{"method": "A", "setup": [{"call": "x"}]}]}"#
            )
            .is_err()
        );
    }
}
