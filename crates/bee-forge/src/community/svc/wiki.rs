//! `WikiService`.

use bee_policy::Action;
use bee_proto::v1::wiki_service_server::WikiService;
use bee_proto::v1::{
    CreateWikiPageRequest, DeleteWikiPageRequest, DeleteWikiPageResponse, DisableWikiRequest,
    EnableWikiRequest, GetWikiFooterRequest, GetWikiPageRequest, GetWikiRequest,
    GetWikiSidebarRequest, ListWikiPageRevisionsRequest, ListWikiPageRevisionsResponse,
    ListWikiPagesRequest, ListWikiPagesResponse, Repo, RepoRole, SearchWikiPagesRequest,
    SearchWikiPagesResponse, SetWikiFooterRequest, SetWikiSidebarRequest, UpdateWikiPageRequest,
    WikiEditAccess, WikiInfo, WikiPage, WikiPageFormat,
};
use tonic::{Request, Response, Status};

use super::writable;
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::Result;
use crate::service::{ForgeServices, next, page, reply};

impl ForgeServices {
    /// Authorizes a wiki call: reads need `read`; edits need `commit`, or `read` when the
    /// wiki is open to everyone who can read the repo.
    async fn wiki_cx(&self, caller: &Caller, repo: &Repo, edit: Option<&str>) -> Result<Cx> {
        let Some(audit) = edit else {
            return self.repo_cx(caller, repo, Action::Read, None, None).await;
        };
        let (_, access) = self.community().wiki.settings(&caller.cx, repo).await?;
        let action = if access == WikiEditAccess::Readers {
            Action::Read
        } else {
            Action::Commit
        };
        let cx = self
            .repo_cx(caller, repo, action, None, Some(audit))
            .await?;
        writable(repo)?;
        Ok(cx)
    }
}

fn at(r: &Option<bee_proto::v1::Revision>) -> String {
    r.as_ref().map(|x| x.expr.clone()).unwrap_or_default()
}

#[tonic::async_trait]
impl WikiService for ForgeServices {
    async fn enable_wiki(
        &self,
        req: Request<EnableWikiRequest>,
    ) -> Result<Response<WikiInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    Some(RepoRole::Maintain),
                    Some("wiki.enable"),
                )
                .await?;
            writable(&repo)?;
            let access = WikiEditAccess::try_from(r.edit_access).unwrap_or_default();
            self.community().wiki.enable(&cx, &repo, access).await
        }
        .await;
        reply(&caller, out)
    }

    async fn disable_wiki(
        &self,
        req: Request<DisableWikiRequest>,
    ) -> Result<Response<WikiInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    Some(RepoRole::Maintain),
                    Some("wiki.disable"),
                )
                .await?;
            writable(&repo)?;
            self.community().wiki.disable(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_wiki(&self, req: Request<GetWikiRequest>) -> Result<Response<WikiInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            self.community().wiki.info(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_wiki_pages(
        &self,
        req: Request<ListWikiPagesRequest>,
    ) -> Result<Response<ListWikiPagesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            let (token, limit) = page(&r.page);
            let (pages, nt) = self
                .community()
                .wiki
                .list_pages(&cx, &repo, &at(&r.at), &token, limit)
                .await?;
            Ok(ListWikiPagesResponse {
                pages,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_wiki_page(
        &self,
        req: Request<GetWikiPageRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            self.community()
                .wiki
                .get_page(&cx, &repo, &r.slug, &at(&r.at))
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_wiki_page(
        &self,
        req: Request<CreateWikiPageRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .wiki_cx(&caller, &repo, Some("wiki.page_created"))
                .await?;
            let format = WikiPageFormat::try_from(r.format).unwrap_or_default();
            self.community()
                .wiki
                .create_page(&cx, &repo, &r.title, &r.content, format, &r.message)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_wiki_page(
        &self,
        req: Request<UpdateWikiPageRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .wiki_cx(&caller, &repo, Some("wiki.page_edited"))
                .await?;
            let format = r.format.and_then(|f| WikiPageFormat::try_from(f).ok());
            self.community()
                .wiki
                .update_page(
                    &cx,
                    &repo,
                    &r.slug,
                    r.expected_changeset.as_ref(),
                    r.title.as_deref(),
                    r.content.as_deref(),
                    format,
                    &r.message,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_wiki_page(
        &self,
        req: Request<DeleteWikiPageRequest>,
    ) -> Result<Response<DeleteWikiPageResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .wiki_cx(&caller, &repo, Some("wiki.page_deleted"))
                .await?;
            let changeset = self
                .community()
                .wiki
                .delete_page(&cx, &repo, &r.slug, &r.message)
                .await?;
            Ok(DeleteWikiPageResponse {
                changeset: Some(changeset),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_wiki_page_revisions(
        &self,
        req: Request<ListWikiPageRevisionsRequest>,
    ) -> Result<Response<ListWikiPageRevisionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            let (token, limit) = page(&r.page);
            let (revisions, nt) = self
                .community()
                .wiki
                .revisions(&cx, &repo, &r.slug, &token, limit)
                .await?;
            Ok(ListWikiPageRevisionsResponse {
                revisions,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_wiki_sidebar(
        &self,
        req: Request<GetWikiSidebarRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            self.community().wiki.sidebar(&cx, &repo, &at(&r.at)).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_wiki_sidebar(
        &self,
        req: Request<SetWikiSidebarRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .wiki_cx(&caller, &repo, Some("wiki.page_edited"))
                .await?;
            self.community()
                .wiki
                .set_sidebar(&cx, &repo, &r.content, &r.message)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_wiki_footer(
        &self,
        req: Request<GetWikiFooterRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            self.community().wiki.footer(&cx, &repo, &at(&r.at)).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_wiki_footer(
        &self,
        req: Request<SetWikiFooterRequest>,
    ) -> Result<Response<WikiPage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .wiki_cx(&caller, &repo, Some("wiki.page_edited"))
                .await?;
            self.community()
                .wiki
                .set_footer(&cx, &repo, &r.content, &r.message)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn search_wiki_pages(
        &self,
        req: Request<SearchWikiPagesRequest>,
    ) -> Result<Response<SearchWikiPagesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.wiki_cx(&caller, &repo, None).await?;
            let (token, limit) = page(&r.page);
            let (hits, nt) = self
                .community()
                .wiki
                .search(&cx, &repo, &r.query, &token, limit)
                .await?;
            Ok(SearchWikiPagesResponse {
                hits,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}
