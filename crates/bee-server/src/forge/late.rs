//! Late-bound handles: ports that the forge needs when it opens, backed by modules that can
//! only be built once the forge exists (the changes module, the work module).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bee_forge::changes::Changes;
use bee_forge::community::backing::ChangesSource;
use bee_forge::community::ports::{ChangeSource, LandedChange};
use bee_forge::work::{ChangeInfo, ChangeSource as WorkChangeSource};
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::{ChangeState, ObjectId, Repo};

/// A value set once after construction; clones share it.
pub struct Late<T>(Arc<OnceLock<Arc<T>>>);

impl<T> Clone for Late<T> {
    fn clone(&self) -> Self {
        Late(self.0.clone())
    }
}

impl<T> Default for Late<T> {
    fn default() -> Self {
        Late(Arc::new(OnceLock::new()))
    }
}

impl<T> Late<T> {
    /// An unset handle.
    pub fn new() -> Late<T> {
        Late::default()
    }

    /// Sets the value (only the first call takes effect).
    pub fn set(&self, v: Arc<T>) {
        let _ = self.0.set(v);
    }

    /// The value, once set.
    pub fn get(&self) -> Option<Arc<T>> {
        self.0.get().cloned()
    }
}

fn not_ready() -> ForgeError {
    ForgeError::unavailable("the change-request module is starting")
        .with_subcode("port_unavailable")
}

/// Release notes' landed change requests, from the changes module once it is installed.
pub struct LateChangeSource(pub Late<Changes>);

#[async_trait]
impl ChangeSource for LateChangeSource {
    async fn landed_between(
        &self,
        repo: &Repo,
        since: Option<&ObjectId>,
        until: &ObjectId,
    ) -> bee_forge::Result<Vec<LandedChange>> {
        let changes = self.0.get().ok_or_else(not_ready)?;
        ChangesSource(changes)
            .landed_between(repo, since, until)
            .await
    }
}

/// Change requests for project items, from the changes module.
pub struct LateWorkChanges(pub Late<Changes>);

#[async_trait]
impl WorkChangeSource for LateWorkChanges {
    async fn change(&self, repo: &Repo, id: &str) -> bee_forge::Result<Option<ChangeInfo>> {
        let Some(changes) = self.0.get() else {
            return Ok(None);
        };
        let cx = Cx::system();
        let cr = match id.parse::<u64>() {
            Ok(n) => changes.get_by_number(&cx, repo, n).await,
            Err(_) => changes.get(&cx, repo, id).await,
        };
        let cr = match cr {
            Ok(cr) => cr,
            Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let state = match ChangeState::try_from(cr.state) {
            Ok(ChangeState::Landed) => "landed",
            Ok(ChangeState::Closed) => "closed",
            _ if cr.draft => "draft",
            _ => "open",
        };
        Ok(Some(ChangeInfo {
            id: cr.id,
            number: cr.number,
            title: cr.title,
            state: state.into(),
            assignees: cr.assignees,
        }))
    }
}
