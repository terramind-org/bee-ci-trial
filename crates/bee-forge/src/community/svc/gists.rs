//! `GistService`: user-owned gists. Reads need a token that allows `read`; every change needs
//! `commit`. A secret gist is reachable by anyone who holds its id.

use bee_policy::Action;
use bee_proto::v1::gist_service_server::GistService;
use bee_proto::v1::{
    Comment, CreateGistCommentRequest, CreateGistRequest, DeleteGistRequest, DeleteGistResponse,
    ForkGistRequest, GetGistFileRawUrlRequest, GetGistFileRawUrlResponse, GetGistRequest,
    GetGistRevisionRequest, Gist, GistListFilter, IsGistStarredRequest, IsGistStarredResponse,
    ListGistCommentsRequest, ListGistCommentsResponse, ListGistForksRequest, ListGistForksResponse,
    ListGistRevisionsRequest, ListGistRevisionsResponse, ListGistsRequest, ListGistsResponse,
    ObjectId, StarGistRequest, StarGistResponse, UnstarGistRequest, UnstarGistResponse,
    UpdateGistRequest,
};
use tonic::{Request, Response, Status};

use crate::community::gists::{DEFAULT_INLINE, FileEdit, ListKind, NewGist};
use crate::error::ForgeError;
use crate::ids::{ts, ts_ms};
use crate::service::{ForgeServices, next, page, reply};

fn rev(o: &Option<ObjectId>) -> Option<ObjectId> {
    o.clone().filter(|o| !o.raw.is_empty())
}

fn inline(n: u32) -> usize {
    if n == 0 { DEFAULT_INLINE } else { n as usize }
}

#[tonic::async_trait]
impl GistService for ForgeServices {
    async fn create_gist(&self, req: Request<CreateGistRequest>) -> Result<Response<Gist>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let mut files: Vec<(String, String)> = r.files.into_iter().collect();
            files.sort();
            self.community()
                .gists
                .create(
                    &cx,
                    NewGist {
                        files,
                        description: r.description,
                        public: r.public,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_gist(&self, req: Request<GetGistRequest>) -> Result<Response<Gist>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            self.community()
                .gists
                .get(&cx, &r.gist_id, None, inline(r.max_inline_bytes))
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_gists(
        &self,
        req: Request<ListGistsRequest>,
    ) -> Result<Response<ListGistsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let since = r.since.as_ref().map(ts_ms).unwrap_or(0);
            let (kind, user) = match GistListFilter::try_from(r.filter).unwrap_or_default() {
                GistListFilter::Unspecified | GistListFilter::Mine => {
                    (ListKind::Mine, String::new())
                }
                GistListFilter::Public => (ListKind::Public, String::new()),
                GistListFilter::Starred => (ListKind::Starred, String::new()),
                GistListFilter::User => {
                    if r.user.is_empty() {
                        return Err(ForgeError::missing("user"));
                    }
                    let (id, _, _) = self.community().users.resolve_login(&cx, &r.user).await?;
                    (ListKind::User, id)
                }
            };
            let (gists, nt) = self
                .community()
                .gists
                .list(&cx, kind, &user, since, &token, limit)
                .await?;
            Ok(ListGistsResponse {
                gists,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_gist(&self, req: Request<UpdateGistRequest>) -> Result<Response<Gist>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let edits: Vec<FileEdit> = r
                .file_changes
                .into_iter()
                .map(|c| FileEdit {
                    filename: c.filename,
                    content: c.content,
                    new_filename: c.new_filename,
                    delete: c.delete,
                })
                .collect();
            self.community()
                .gists
                .update(
                    &cx,
                    &r.gist_id,
                    rev(&r.expected_head).as_ref(),
                    r.description.as_deref(),
                    &edits,
                    r.comments_locked,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_gist(
        &self,
        req: Request<DeleteGistRequest>,
    ) -> Result<Response<DeleteGistResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().gists.delete(&cx, &r.gist_id).await?;
            Ok(DeleteGistResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_gist_revisions(
        &self,
        req: Request<ListGistRevisionsRequest>,
    ) -> Result<Response<ListGistRevisionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let (revisions, nt) = self
                .community()
                .gists
                .revisions(&cx, &r.gist_id, &token, limit)
                .await?;
            Ok(ListGistRevisionsResponse {
                revisions,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_gist_revision(
        &self,
        req: Request<GetGistRevisionRequest>,
    ) -> Result<Response<Gist>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let at = rev(&r.changeset).ok_or_else(|| ForgeError::missing("changeset"))?;
            self.community()
                .gists
                .get(&cx, &r.gist_id, Some(&at), inline(r.max_inline_bytes))
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_gist_file_raw_url(
        &self,
        req: Request<GetGistFileRawUrlRequest>,
    ) -> Result<Response<GetGistFileRawUrlResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (url, expires, size) = self
                .community()
                .gists
                .raw_url(&cx, &r.gist_id, &r.filename, rev(&r.changeset).as_ref())
                .await?;
            Ok(GetGistFileRawUrlResponse {
                url,
                expires_at: Some(ts(expires)),
                size,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn fork_gist(&self, req: Request<ForkGistRequest>) -> Result<Response<Gist>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community().gists.fork(&cx, &r.gist_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_gist_forks(
        &self,
        req: Request<ListGistForksRequest>,
    ) -> Result<Response<ListGistForksResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let (forks, nt) = self
                .community()
                .gists
                .forks(&cx, &r.gist_id, &token, limit)
                .await?;
            Ok(ListGistForksResponse {
                forks,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn star_gist(
        &self,
        req: Request<StarGistRequest>,
    ) -> Result<Response<StarGistResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .gists
                .set_starred(&cx, &r.gist_id, true)
                .await?;
            Ok(StarGistResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn unstar_gist(
        &self,
        req: Request<UnstarGistRequest>,
    ) -> Result<Response<UnstarGistResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .gists
                .set_starred(&cx, &r.gist_id, false)
                .await?;
            Ok(UnstarGistResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn is_gist_starred(
        &self,
        req: Request<IsGistStarredRequest>,
    ) -> Result<Response<IsGistStarredResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            Ok(IsGistStarredResponse {
                starred: self.community().gists.is_starred(&cx, &r.gist_id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_gist_comment(
        &self,
        req: Request<CreateGistCommentRequest>,
    ) -> Result<Response<Comment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            self.community()
                .gists
                .create_comment(&cx, &r.gist_id, &r.body)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_gist_comments(
        &self,
        req: Request<ListGistCommentsRequest>,
    ) -> Result<Response<ListGistCommentsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let (token, limit) = page(&r.page);
            let (comments, nt) = self
                .community()
                .gists
                .list_comments(&cx, &r.gist_id, &token, limit)
                .await?;
            Ok(ListGistCommentsResponse {
                comments,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}
