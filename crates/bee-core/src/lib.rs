//! Shared domain types: ids, errors, paths, ref names, revisions, modes and limits.
//!
//! The contract lives in SPEC.md §3 (identifiers), §4 (paths, modes, limits), §5 (refs and
//! revisions) and §12.2 (errors). Nothing here reads the clock or a random source.

pub mod change_id;
pub mod error;
pub mod hex;
pub mod id;
pub mod limits;
pub mod mode;
pub mod path;
pub mod refname;
pub mod revision;
pub mod ulid;

pub use change_id::{CHANGE_ID_LEN, ChangeId};
pub use error::{BeeError, GrpcCode, PayloadValue, Reason, Result, Subcode};
pub use id::{
    DIGEST_LEN, HashAlgo, MIN_PREFIX_HEX, OBJECT_ID_LEN, ObjectId, ObjectIdPrefix, ObjectKind,
};
pub use limits::Limits;
pub use mode::Mode;
pub use path::{RepoPath, validate_name};
pub use refname::{RefName, RefNamespace};
pub use revision::{Revision, RevisionBase, RevisionSuffix};
pub use ulid::Ulid;
