//! Row records of the changes surface.
//!
//! Bodies are protobuf. Where a `bee.v1` message carries everything a row needs it is
//! stored as is (reviews, threads, merge-queue entries, land conflicts, changeset comments);
//! the records below wrap a `bee.v1` message with the state the API does not expose. They
//! follow the proto's additive-only rule: fields are never renumbered or retyped.

use crate::error::{ForgeError, Result};
use bee_proto::v1::{ChangeRequest, Comment, ObjectId, Principal, ReactionGroup, ReviewComment};

/// A change request row.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CrRecord {
    /// The change request as served (version, comment count and reactions are filled on
    /// read).
    #[prost(message, optional, tag = "1")]
    pub cr: Option<ChangeRequest>,
    /// Org of the repo (events and audit).
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Paths the current revision changes, with their diff class and new size.
    #[prost(message, repeated, tag = "3")]
    pub paths: Vec<PathClass>,
    /// Per-revision facts the API does not carry (who pushed it).
    #[prost(message, repeated, tag = "4")]
    pub revs: Vec<RevMeta>,
    /// Issues linked through closing keywords in the body or the stack's messages.
    #[prost(uint64, repeated, tag = "5")]
    pub keyword_issues: Vec<u64>,
    /// Reactions on the CR itself.
    #[prost(message, repeated, tag = "6")]
    pub reactions: Vec<ReactionRec>,
    /// The current stack contains merge changesets.
    #[prost(bool, tag = "7")]
    pub has_merges: bool,
    /// The current stack, oldest first.
    #[prost(message, repeated, tag = "8")]
    pub stack: Vec<ObjectId>,
    /// Manually linked issues close on land.
    #[prost(bool, tag = "9")]
    pub close_on_land: bool,
    /// Issues linked by hand (`LinkChangeIssues`, `OpenChange.linked_issues`).
    #[prost(uint64, repeated, tag = "10")]
    pub manual_issues: Vec<u64>,
    /// Conversation and review comments on the CR.
    #[prost(uint32, tag = "11")]
    pub comment_count: u32,
    /// The target tip just before the CR landed (what a revert restores).
    #[prost(message, optional, tag = "12")]
    pub pre_land_tip: Option<ObjectId>,
    /// Land method of the merge-queue entry (`bee.v1.LandMethod`).
    #[prost(int32, tag = "13")]
    pub queue_method: i32,
    /// The delegation chain of whoever enabled auto-merge or enqueued the CR (actor first),
    /// so the land is evaluated for the same chain.
    #[prost(string, repeated, tag = "14")]
    pub land_chain: Vec<String>,
    /// The target tip the merge-queue entry's combined result (`speculative_head`) was built
    /// on; a result built on an older tip is rebuilt before it may land.
    #[prost(message, optional, tag = "15")]
    pub queue_base: Option<ObjectId>,
    /// Store-clock timestamp for the durable intent to land this change request.
    #[prost(uint64, tag = "16")]
    pub landing_started_at_ms: u64,
    /// Unique id for the current durable landing intent.
    #[prost(string, tag = "17")]
    pub landing_intent_id: String,
    /// The change request head authorized by the current landing intent.
    #[prost(message, optional, tag = "18")]
    pub landing_head: Option<ObjectId>,
    /// Verified run that opened the change. Its evidence events need no extra lookup.
    #[prost(string, tag = "19")]
    pub(crate) safety_run_id: String,
    /// Whether the current queue entry came from auto-merge. Absent for legacy entries.
    #[prost(bool, optional, tag = "20")]
    pub(crate) queued_by_auto_merge: Option<bool>,
}

impl CrRecord {
    /// The change request (always present in a stored record).
    pub fn cr(&self) -> &ChangeRequest {
        static EMPTY: std::sync::OnceLock<ChangeRequest> = std::sync::OnceLock::new();
        self.cr
            .as_ref()
            .unwrap_or_else(|| EMPTY.get_or_init(ChangeRequest::default))
    }

    /// The change request, mutable.
    pub fn cr_mut(&mut self) -> &mut ChangeRequest {
        self.cr.get_or_insert_with(ChangeRequest::default)
    }

    /// Rejects a fresh landing intent, or clears one abandoned for more than two minutes.
    pub(crate) fn clear_expired_landing_intent(&mut self, now_ms: u64) -> Result<bool> {
        if self.landing_intent_id.is_empty() {
            return Ok(false);
        }
        if now_ms.saturating_sub(self.landing_started_at_ms) <= 120_000 {
            return Err(ForgeError::invalid_state(format!(
                "change request #{} is landing",
                self.cr().number
            )));
        }
        self.clear_landing_intent();
        Ok(true)
    }

    pub(crate) fn clear_landing_intent(&mut self) {
        self.landing_started_at_ms = 0;
        self.landing_intent_id.clear();
        self.landing_head = None;
    }

    /// Issues closed when the CR lands: keyword links, plus manual links when
    /// `close_on_land` is set; sorted and unique.
    pub fn closing_issues(&self) -> Vec<u64> {
        let mut v = self.keyword_issues.clone();
        if self.close_on_land {
            v.extend(self.manual_issues.iter().copied());
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Every linked issue, sorted and unique.
    pub fn linked_issues(&self) -> Vec<u64> {
        let mut v = self.keyword_issues.clone();
        v.extend(self.manual_issues.iter().copied());
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// A changed path of the current revision.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PathClass {
    /// Repository path.
    #[prost(string, tag = "1")]
    pub path: String,
    /// Diff class (SPEC §11.3 wire name).
    #[prost(string, tag = "2")]
    pub class: String,
    /// New size in bytes (meaningless when `deleted`).
    #[prost(uint64, tag = "3")]
    pub size: u64,
    /// The path is absent at the head.
    #[prost(bool, tag = "4")]
    pub deleted: bool,
}

/// Facts about one revision.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RevMeta {
    /// Revision number.
    #[prost(uint32, tag = "1")]
    pub n: u32,
    /// Who pushed it (the last-push approval rule).
    #[prost(message, optional, tag = "2")]
    pub pusher: Option<Principal>,
}

/// One principal's reaction.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReactionRec {
    /// Who reacted.
    #[prost(string, tag = "1")]
    pub principal_id: String,
    /// `bee.v1.ReactionContent`.
    #[prost(int32, tag = "2")]
    pub content: i32,
}

/// A comment row: a conversation comment or a review comment.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CommentRecord {
    /// Set for conversation comments.
    #[prost(message, optional, tag = "1")]
    pub conversation: Option<Comment>,
    /// Set for line and file comments (members of a thread).
    #[prost(message, optional, tag = "2")]
    pub review: Option<ReviewComment>,
    /// Reactions.
    #[prost(message, repeated, tag = "3")]
    pub reactions: Vec<ReactionRec>,
    /// The review the comment was submitted with, if any.
    #[prost(string, tag = "4")]
    pub review_id: String,
    /// Earlier bodies of a conversation comment, oldest first (`CommentService`).
    #[prost(message, repeated, tag = "5")]
    pub edits: Vec<bee_proto::v1::CommentEdit>,
}

/// A file a reviewer marked viewed.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ViewedRecord {
    /// Path.
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    /// Revision it was viewed on.
    #[prost(uint32, tag = "2")]
    pub revision: u32,
    /// The file's blob at that revision (absent when the file was deleted).
    #[prost(message, optional, tag = "3")]
    pub blob: Option<ObjectId>,
}

/// Groups reactions for display; `viewer` marks the caller's own.
pub fn reaction_groups(recs: &[ReactionRec], viewer: &str) -> Vec<ReactionGroup> {
    let mut by: std::collections::BTreeMap<i32, (u32, bool)> = std::collections::BTreeMap::new();
    for r in recs {
        let e = by.entry(r.content).or_default();
        e.0 += 1;
        e.1 |= r.principal_id == viewer;
    }
    by.into_iter()
        .map(|(content, (count, viewer_reacted))| ReactionGroup {
            content,
            count,
            viewer_reacted,
        })
        .collect()
}

/// Adds or removes one reaction; returns whether anything changed.
pub fn toggle_reaction(recs: &mut Vec<ReactionRec>, who: &str, content: i32, add: bool) -> bool {
    let pos = recs
        .iter()
        .position(|r| r.principal_id == who && r.content == content);
    match (pos, add) {
        (None, true) => {
            recs.push(ReactionRec {
                principal_id: who.to_string(),
                content,
            });
            true
        }
        (Some(i), false) => {
            recs.remove(i);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactions_group_and_toggle() {
        let mut v = Vec::new();
        assert!(toggle_reaction(&mut v, "a", 1, true));
        assert!(!toggle_reaction(&mut v, "a", 1, true));
        assert!(toggle_reaction(&mut v, "b", 1, true));
        assert!(toggle_reaction(&mut v, "b", 6, true));
        let g = reaction_groups(&v, "a");
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].count, g[0].viewer_reacted), (2, true));
        assert_eq!((g[1].count, g[1].viewer_reacted), (1, false));
        assert!(toggle_reaction(&mut v, "a", 1, false));
        assert_eq!(reaction_groups(&v, "a")[0].count, 1);
    }

    #[test]
    fn closing_issues_follow_close_on_land() {
        let mut r = CrRecord {
            keyword_issues: vec![3, 1],
            manual_issues: vec![2, 3],
            ..CrRecord::default()
        };
        assert_eq!(r.closing_issues(), vec![1, 3]);
        assert_eq!(r.linked_issues(), vec![1, 2, 3]);
        r.close_on_land = true;
        assert_eq!(r.closing_issues(), vec![1, 2, 3]);
    }
}
