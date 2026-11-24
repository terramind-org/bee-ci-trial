//! Event reactions of Pages, registered on the event log by the forge:
//!
//! - `ref.updated` on a site's source ref rebuilds and deploys the site (REF sites);
//! - `cr.opened`, `cr.revised` and `cr.reopened` build the preview of that change request from
//!   its newest revision (REF sites with previews on, change requests aimed at the source ref);
//! - `cr.closed` and `cr.landed` remove the preview.
//!
//! Builds run on their own tasks so the event relay is never held up by a deployment.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{PagesBuildType, event};

use super::Pages;
use super::deploy::DeployOptions;
use super::site::build_type;
use crate::cx::Cx;
use crate::events::EventListener;
use bee_proto::v1::Event;

/// Keeps sites and previews in step with refs and change requests.
pub struct PagesListener {
    pages: Arc<Pages>,
}

impl PagesListener {
    /// A listener over `pages`.
    pub fn new(pages: Arc<Pages>) -> PagesListener {
        PagesListener { pages }
    }
}

fn matches_ref(name: &str, source: &str) -> bool {
    name == source || name.strip_prefix("refs/heads/") == Some(source)
}

#[async_trait]
impl EventListener for PagesListener {
    async fn on_event(&self, ev: &Event) {
        let kind = ev.kind.as_str();
        if !matches!(
            kind,
            "ref.updated" | "cr.opened" | "cr.revised" | "cr.reopened" | "cr.closed" | "cr.landed"
        ) || ev.repo_id.is_empty()
        {
            return;
        }
        let pages = self.pages.clone();
        let ev = ev.clone();
        tokio::spawn(async move {
            let cx = Cx::system().with_timeout(std::time::Duration::from_secs(120));
            let Ok(repo) = pages.orgs.repo_any(&cx, &ev.repo_id).await else {
                return;
            };
            if repo.deleted_at.is_some() {
                return;
            }
            let Ok(site) = pages.site(&cx, &repo).await else {
                return;
            };
            match (ev.kind.as_str(), &ev.body) {
                ("ref.updated", Some(event::Body::RefUpdated(r))) => {
                    if build_type(site.build_type) == PagesBuildType::Ref
                        && matches_ref(&r.name, &site.source_ref)
                    {
                        let _ = pages
                            .request_build(&cx, &repo, &DeployOptions::default())
                            .await;
                    }
                }
                ("cr.opened" | "cr.revised" | "cr.reopened", Some(event::Body::Change(cr))) => {
                    if !cr.target_ref.is_empty() && !matches_ref(&cr.target_ref, &site.source_ref) {
                        return;
                    }
                    if let Some(head) = cr.revisions.last().and_then(|r| r.head.clone()) {
                        let _ = pages
                            .deploy_change_preview(&cx, &repo, cr.number, &head)
                            .await;
                    }
                }
                ("cr.closed" | "cr.landed", Some(event::Body::Change(cr))) => {
                    let _ = pages.remove_change_preview(&cx, &repo, cr.number).await;
                }
                _ => {}
            }
        });
    }
}
