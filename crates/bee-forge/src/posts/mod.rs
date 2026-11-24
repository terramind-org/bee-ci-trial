//! Posts on change requests and the Following feed (PRODUCT §4.16; the rules are in
//! `docs/spec/forge-community.md` "Posts and feeds"). Serves `PostService`.
//!
//! A change request carries at most one post: short NFC plain text (at most
//! [`MAX_TEXT_CHARS`] characters) with up to [`MAX_IMAGES`] images or one video. It is a
//! `DRAFT` while the change is open and publishes when the change lands, in the land's own
//! transaction ([`store::on_landed`] from `Changes::record_landed`). A post written by an
//! agent or service publishes only once a human with repo Write approved it; until then a
//! landed one is `PENDING_APPROVAL`. Closing the change without landing discards its post
//! (`DISCARDED`), reopening it makes it a draft again.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | tables, the migration, event kinds, audit prefix, limits |
//! | [`model`] | the stored record |
//! | [`text`] | pure text validation (NFC, length, control characters) |
//! | [`store`] | [`Posts`]: drafts, media, approval, withdrawal, likes, reposts, publishing hooks, upkeep |
//! | [`feed`] | the Following feed's sources and their merge |
//! | [`adopt`] | adoptions: issues and change requests a post inspired ("Inspired by") |
//! | [`service`] | the tonic `PostService` and the post as each caller sees it |
//!
//! # Storage
//!
//! Every table lives on the `changes` surface, so the land, close and reopen transactions of
//! a change request update its post atomically. The changes module lists the migration
//! ([`M1`]); this module owns the `post.*` event kinds and audit actions. Repo follows are
//! the users store's (`crate::community::users`).
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | posts | repo | post | `k1` author, `k2` change request (unique), `n1` published ms (published only) |
//! | post ids | `""` | post | `k1` repo |
//! | media | repo | media | `k1` change request, `k2` post (attached), `n1` created ms (unattached only) |
//! | likes | post | principal | `n1` time |
//! | reposts | reposter | post | `k1` post, `k2` repo, `n1` time |
//! | counters | post | `likes` / `reposts` | `n1` count |
//! | change adoptions | `""` | change request | `k1` post, `n1` created ms |
//! | issue adoptions (work surface) | `""` | issue | `k1` post, `n1` created ms |
//!
//! An adoption is written by the transaction that opens its issue or change request, so it
//! lives on that subject's surface: change adoptions here ([`M2`], listed by the changes
//! module), issue adoptions on the work surface ([`M_WORK`], listed by the work module).

pub mod adopt;
pub mod feed;
pub mod model;
pub mod service;
pub mod store;
pub mod text;

pub use store::{PostPorts, Posts};

use crate::changes::SURFACE;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, TableDef};

/// Most characters (Unicode scalar values) of a post's text.
pub const MAX_TEXT_CHARS: usize = 500;
/// Most images on one post.
pub const MAX_IMAGES: usize = 4;
/// Alt text bounds of an image, in characters.
pub const ALT_TEXT_CHARS: (usize, usize) = (1, 1000);
/// Longest withdrawal reason, in characters.
pub const MAX_REASON_CHARS: usize = 1000;
/// Unattached media is deleted after this long.
pub const UNATTACHED_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Blob key and URL path prefix of post media (`posts/<repo>/<media>`).
pub const MEDIA_PREFIX: &str = "posts";

/// Posts.
pub static POSTS: TableDef = TableDef {
    name: "forge_posts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: true,
    indexes: &[
        Index::ScopeK1,
        Index::ScopeK2,
        Index::ScopeN1,
        Index::K1,
        Index::N1,
    ],
};

/// Post id → repo, so a post is found by its id alone.
pub static POST_IDS: TableDef = TableDef {
    name: "forge_post_ids",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Uploaded media.
pub static MEDIA: TableDef = TableDef {
    name: "forge_post_media",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::N1],
};

/// Likes.
pub static LIKES: TableDef = TableDef {
    name: "forge_post_likes",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};

/// Reposts.
pub static REPOSTS: TableDef = TableDef {
    name: "forge_post_reposts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::K1],
};

/// Like and repost counters.
pub static COUNTERS: TableDef = TableDef {
    name: "forge_post_counters",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Change requests a post inspired: scope `""`, id the change request, `k1` the post,
/// `n1` created (ms).
pub static CHANGE_ADOPTIONS: TableDef = TableDef {
    name: "forge_post_change_adoptions",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Issues a post inspired, on the work surface (the issue's own transaction writes them):
/// scope `""`, id the issue, `k1` the post, `n1` created (ms).
pub static ISSUE_ADOPTIONS: TableDef = TableDef {
    name: "forge_post_issue_adoptions",
    surface: crate::work::SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// The posts tables, in the changes module's migration list (they share its surface).
pub static M1: Migration = Migration {
    module: "changes",
    version: 4,
    name: "create_posts",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&POSTS),
        MigrationOp::CreateTable(&POST_IDS),
        MigrationOp::CreateTable(&MEDIA),
        MigrationOp::CreateTable(&LIKES),
        MigrationOp::CreateTable(&REPOSTS),
        MigrationOp::CreateTable(&COUNTERS),
    ],
};

/// Change adoptions, in the changes module's migration list.
pub static M2: Migration = Migration {
    module: "changes",
    version: 5,
    name: "create_post_change_adoptions",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&CHANGE_ADOPTIONS)],
};

/// Issue adoptions, in the work module's migration list.
pub static M_WORK: Migration = Migration {
    module: "work",
    version: 2,
    name: "create_post_issue_adoptions",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&ISSUE_ADOPTIONS)],
};

/// What the posts module owns: its event kinds and audit actions (its tables are listed by
/// the changes module, whose surface they live on).
pub static MODULE: ForgeModule = ForgeModule {
    name: "posts",
    surfaces: &[],
    migrations: &[],
    event_kinds: &[
        EventKind::new(
            "post.drafted",
            "The draft post of a change request was written or edited",
        ),
        EventKind::new(
            "post.approval_requested",
            "A landed change's agent-written post waits for a person's approval",
        ),
        EventKind::new("post.published", "A post was published"),
        EventKind::new("post.withdrawn", "A post was withdrawn"),
        EventKind::new("post.liked", "A post was liked"),
        EventKind::new("post.reposted", "A post was reposted"),
        EventKind::new(
            "post.adopted",
            "An issue or change request names a post as its inspiration",
        ),
    ],
    audit_prefixes: &["post."],
};
