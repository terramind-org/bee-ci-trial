//! GitHub's numeric ids and GraphQL node ids for bee objects.
//!
//! Bee ids are ULIDs or principal ids (strings); GitHub clients expect integers that fit a
//! JavaScript number. Every object gets `id = BLAKE3("swarm-compat id v1\0" ‖ kind ‖ "\0" ‖
//! bee id)` truncated to 53 bits (never 0): a pure function of the object, so it is stable
//! across restarts and replicas. Paths that take a numeric id resolve it through the
//! [`IdIndex`], filled whenever an object is rendered, and fall back to scanning the parent
//! collection.
//!
//! Node ids are `<prefix>_<base64url(kind ":" key)>`: decodable without state, so `node(id:)`
//! and mutation inputs (`pullRequestId`, `repositoryId`, `subjectId`) work on any replica.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Largest index size before the oldest half is dropped.
const INDEX_CAP: usize = 200_000;

/// The numeric id of `(kind, bee id)`.
pub fn num(kind: &str, id: &str) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"swarm-compat id v1\0");
    h.update(kind.as_bytes());
    h.update(b"\0");
    h.update(id.as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 8];
    b.copy_from_slice(&d.as_bytes()[..8]);
    let n = u64::from_be_bytes(b) & ((1u64 << 53) - 1);
    if n == 0 { 1 } else { n }
}

/// What a node id names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRef {
    /// GraphQL type name, e.g. `PullRequest`.
    pub kind: String,
    /// The key: a bee id, or `repo_id:number` for numbered objects.
    pub key: String,
}

fn prefix(kind: &str) -> &'static str {
    match kind {
        "Repository" => "R",
        "PullRequest" => "PR",
        "Issue" => "I",
        "User" => "U",
        "Bot" => "BOT",
        "Organization" => "O",
        "Label" => "LA",
        "Milestone" => "MI",
        "IssueComment" => "IC",
        "PullRequestReview" => "PRR",
        "PullRequestReviewComment" => "PRRC",
        "PullRequestReviewThread" => "PRRT",
        "Release" => "RE",
        "ReleaseAsset" => "RA",
        "CheckRun" => "CR",
        "CheckSuite" => "CS",
        "StatusContext" => "SC",
        "Commit" => "C",
        "Ref" => "REF",
        "Team" => "T",
        "Workflow" => "W",
        "WorkflowRun" => "WFR",
        "Gist" => "G",
        "Deployment" => "DE",
        "Tree" => "TR",
        "Blob" => "B",
        _ => "N",
    }
}

/// The node id of `(kind, key)`.
pub fn node(kind: &str, key: &str) -> String {
    format!(
        "{}_{}",
        prefix(kind),
        URL_SAFE_NO_PAD.encode(format!("{kind}:{key}"))
    )
}

/// Decodes a node id made by [`node`].
pub fn parse_node(id: &str) -> Option<NodeRef> {
    let (_, enc) = id.split_once('_')?;
    let raw = URL_SAFE_NO_PAD.decode(enc).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (kind, key) = text.split_once(':')?;
    Some(NodeRef {
        kind: kind.to_string(),
        key: key.to_string(),
    })
}

/// What a numeric id was rendered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Indexed {
    /// The bee id.
    pub id: String,
    /// Extra context (the repo id, an issue number), kind-specific.
    pub context: String,
}

/// Numeric id → bee id, remembered as objects are rendered.
#[derive(Default)]
pub struct IdIndex {
    map: Mutex<HashMap<(String, u64), Indexed>>,
}

impl IdIndex {
    /// Remembers `(kind, id)` with `context` and returns its numeric id.
    pub fn remember(&self, kind: &str, id: &str, context: &str) -> u64 {
        let n = num(kind, id);
        if let Ok(mut m) = self.map.lock() {
            if m.len() >= INDEX_CAP {
                let drop: Vec<(String, u64)> = m.keys().take(INDEX_CAP / 2).cloned().collect();
                for k in drop {
                    m.remove(&k);
                }
            }
            m.insert(
                (kind.to_string(), n),
                Indexed {
                    id: id.to_string(),
                    context: context.to_string(),
                },
            );
        }
        n
    }

    /// What `(kind, n)` was rendered from, if remembered.
    pub fn get(&self, kind: &str, n: u64) -> Option<Indexed> {
        self.map
            .lock()
            .ok()
            .and_then(|m| m.get(&(kind.to_string(), n)).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ids_are_stable_and_js_safe() {
        let a = num("repo", "01J00000000000000000000000");
        assert_eq!(a, num("repo", "01J00000000000000000000000"));
        assert_ne!(a, num("issue", "01J00000000000000000000000"));
        assert!(a < (1u64 << 53) && a > 0);
    }

    #[test]
    fn node_ids_round_trip() {
        let id = node("PullRequest", "01J0:12");
        assert!(id.starts_with("PR_"));
        assert_eq!(
            parse_node(&id),
            Some(NodeRef {
                kind: "PullRequest".into(),
                key: "01J0:12".into()
            })
        );
        assert_eq!(parse_node("garbage"), None);
    }

    #[test]
    fn index_remembers() {
        let ix = IdIndex::default();
        let n = ix.remember("hook", "w1", "r1");
        assert_eq!(ix.get("hook", n).unwrap().id, "w1");
        assert!(ix.get("hook", n + 1).is_none());
    }
}
