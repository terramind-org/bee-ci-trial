//! The stored post. Field numbers follow the proto rule: never renumbered or retyped.

use bee_proto::v1::{Post, PostState};

/// A post row.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PostRec {
    /// The post as stored: counts, viewer, version and media URLs are filled on read.
    #[prost(message, optional, tag = "1")]
    pub post: Option<Post>,
    /// Org of the repo (events and audit).
    #[prost(string, tag = "2")]
    pub org_id: String,
}

impl PostRec {
    /// The post (always present in a stored record).
    pub fn post(&self) -> &Post {
        static EMPTY: std::sync::OnceLock<Post> = std::sync::OnceLock::new();
        self.post
            .as_ref()
            .unwrap_or_else(|| EMPTY.get_or_init(Post::default))
    }

    /// The post, mutable.
    pub fn post_mut(&mut self) -> &mut Post {
        self.post.get_or_insert_with(Post::default)
    }

    /// Its state.
    pub fn state(&self) -> PostState {
        PostState::try_from(self.post().state).unwrap_or(PostState::Unspecified)
    }

    /// The author's principal id.
    pub fn author_id(&self) -> &str {
        self.post()
            .author
            .as_ref()
            .map(|a| a.id.as_str())
            .unwrap_or("")
    }

    /// Whether a person wrote it (agent and service posts need a human's approval).
    pub fn by_human(&self) -> bool {
        self.post()
            .author
            .as_ref()
            .is_some_and(|a| a.kind == bee_proto::v1::PrincipalKind::Human as i32)
    }

    /// Whether the text and media may still change.
    pub fn editable(&self) -> bool {
        matches!(self.state(), PostState::Draft | PostState::PendingApproval)
    }
}
