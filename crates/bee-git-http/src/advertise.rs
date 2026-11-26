//! The refs a principal sees over git, with git ids, peeled annotated tags and `HEAD`.

use std::collections::BTreeMap;

use bee_core::{ObjectId, RefName};
use bee_git::{GitOid, GitTag, ObjectFormat};
use bee_store::DerivedRow;

use crate::backend::RepoSession;
use crate::error::GitResult;
use crate::gitmap::{GitView, detect_format};
use crate::refs::git_name;
use crate::walk::AnnotatedTag;

pub use bee_import::{GIT_REF_TAG_TYPE, GIT_REF_TAG_VERSION, ref_tag_key};

/// The rows recording that tag ref `ref_name`, pointing at `changeset`, carries the annotated
/// tag `tag` (whose bytes go in the importer's `git_tag` record).
pub fn ref_tag_rows(
    ref_name: &RefName,
    changeset: &ObjectId,
    tag: &GitOid,
    bytes: &[u8],
) -> GitResult<Vec<DerivedRow>> {
    Ok(vec![
        DerivedRow {
            key: ref_tag_key(ref_name, changeset)?,
            type_name: GIT_REF_TAG_TYPE.into(),
            version: GIT_REF_TAG_VERSION,
            value: tag.as_bytes().to_vec(),
        },
        DerivedRow {
            key: tag.to_foreign(),
            type_name: bee_import::GIT_TAG_TYPE.into(),
            version: bee_import::GIT_TAG_VERSION,
            value: bytes.to_vec(),
        },
    ])
}

/// One advertised ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvertisedRef {
    /// The git name (`refs/heads/main`).
    pub name: String,
    /// The bee ref.
    pub bee: RefName,
    /// The ref version.
    pub version: u64,
    /// The changeset it points at.
    pub target: ObjectId,
    /// The advertised id: the annotated tag's id for a tag ref that has one, else the commit.
    pub oid: GitOid,
    /// The commit id, when `oid` is an annotated tag.
    pub peeled: Option<GitOid>,
}

/// Everything a ref advertisement needs.
#[derive(Clone, Debug)]
pub struct RefList {
    /// The repo's object format.
    pub format: ObjectFormat,
    /// Visible refs, sorted by git name.
    pub refs: Vec<AdvertisedRef>,
    /// The branch `HEAD` names (`refs/heads/main` unless `main` is absent and another branch
    /// exists).
    pub head: String,
    /// Annotated tags that can be served.
    pub tags: Vec<AnnotatedTag>,
}

impl RefList {
    /// The ref `HEAD` resolves to, if it exists.
    pub fn head_ref(&self) -> Option<&AdvertisedRef> {
        self.refs.iter().find(|r| r.name == self.head)
    }

    /// A ref by git name.
    pub fn get(&self, name: &str) -> Option<&AdvertisedRef> {
        self.refs.iter().find(|r| r.name == name)
    }
}

/// The object format of a session's repo.
pub async fn repo_format(session: &dyn RepoSession) -> GitResult<ObjectFormat> {
    let h = session.history();
    let rows = h.meta().list_refs(h.repo(), "").await?;
    let tips: Vec<ObjectId> = rows.iter().filter_map(|r| r.target).collect();
    detect_format(h, &tips).await
}

/// Lists the refs `session`'s principal may see, with their git ids.
pub async fn list(session: &dyn RepoSession, view: &mut GitView<'_>) -> GitResult<RefList> {
    let h = session.history();
    let me = session.principal();
    let rows = h.meta().list_refs(h.repo(), "").await?;
    let mut refs: BTreeMap<String, AdvertisedRef> = BTreeMap::new();
    let mut tags = Vec::new();
    for r in rows {
        let Some(target) = r.target else { continue };
        if !session.ref_visible(&r.name) {
            continue;
        }
        let commit = view.commit_oid(&target).await?;
        let name = git_name(&r.name, me);
        let mut oid = commit;
        let mut peeled = None;
        if r.name.as_str().starts_with("tags/") {
            let key = ref_tag_key(&r.name, &target)?;
            if let Some(v) = h
                .derived(&key, GIT_REF_TAG_TYPE, GIT_REF_TAG_VERSION)
                .await?
                && let Ok(tag_oid) = GitOid::from_bytes(&v)
                && let Some(bytes) =
                    bee_import::lookup_git_tag(h.meta(), h.repo(), &tag_oid).await?
                && GitTag::parse(&bytes, view.format()).is_ok_and(|t| t.object() == commit)
            {
                oid = tag_oid;
                peeled = Some(commit);
                tags.push(AnnotatedTag {
                    oid: tag_oid,
                    bytes,
                    peeled: target,
                });
            }
        }
        refs.insert(
            name.clone(),
            AdvertisedRef {
                name,
                bee: r.name,
                version: r.version,
                target,
                oid,
                peeled,
            },
        );
    }
    view.flush().await?;
    let head = if refs.contains_key("refs/heads/main") {
        "refs/heads/main".to_string()
    } else {
        refs.keys()
            .find(|n| n.starts_with("refs/heads/"))
            .cloned()
            .unwrap_or_else(|| "refs/heads/main".to_string())
    };
    Ok(RefList {
        format: view.format(),
        refs: refs.into_values().collect(),
        head,
        tags,
    })
}
