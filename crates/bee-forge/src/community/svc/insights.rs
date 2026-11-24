//! `InsightsService`. Repo insights need `read`; traffic needs `commit` (push access, like
//! GitHub); org dependency insights need org membership.

use bee_policy::Action;
use bee_proto::v1::insights_service_server::InsightsService;
use bee_proto::v1::{
    CitationInfo, CommunityFileKind, CommunityProfile, GetCitationRequest, GetCodeFrequencyRequest,
    GetCodeFrequencyResponse, GetCommitActivityRequest, GetCommitActivityResponse,
    GetCommunityProfileRequest, GetLanguageStatsRequest, GetLanguageStatsResponse,
    GetLicenseInfoRequest, GetNetworkGraphRequest, GetNetworkGraphResponse, GetPulseRequest,
    GetTrafficClonesRequest, GetTrafficClonesResponse, GetTrafficViewsRequest,
    GetTrafficViewsResponse, LicenseInfo, ListContributorsRequest, ListContributorsResponse,
    ListInsightsAgentStatsRequest, ListInsightsAgentStatsResponse,
    ListInsightsOrgDependenciesRequest, ListInsightsOrgDependenciesResponse,
    ListTrafficPopularPathsRequest, ListTrafficPopularPathsResponse, ListTrafficReferrersRequest,
    ListTrafficReferrersResponse, ObjectId, PulseSummary, Repo, Revision,
};
use tonic::{Request, Response, Status};

use crate::auth::OrgPerm;
use crate::community::common::repo_ref_id;
use crate::community::insights::{ContributorQuery, Insights};
use crate::community::ports::{DependencyQuery, VcsRepo, read_file};
use crate::community::repofiles;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::service::{ForgeServices, next, page, reply};

fn expr(r: &Option<Revision>) -> String {
    r.as_ref().map(|x| x.expr.clone()).unwrap_or_default()
}

impl ForgeServices {
    async fn ins_read(
        &self,
        caller: &crate::auth::Caller,
        r: &Option<bee_proto::v1::RepoRef>,
    ) -> Result<(Repo, Cx)> {
        let repo = self.repo(caller, r).await?;
        let cx = self
            .repo_cx(caller, &repo, Action::Read, None, None)
            .await?;
        Ok((repo, cx))
    }

    async fn ins_tree(
        &self,
        repo: &Repo,
        at: &Option<Revision>,
    ) -> Result<(ObjectId, Vec<crate::community::ports::FileEntry>)> {
        let vcs = &self.community().ports.vcs;
        let vr = VcsRepo::of_repo(repo);
        let changeset = vcs.resolve(&vr, &expr(at)).await?;
        let tree = vcs.tree(&vr, &changeset).await?;
        Ok((changeset, tree))
    }

    async fn ins_text(&self, repo: &Repo, at: &ObjectId, path: &str) -> Option<String> {
        let vcs = &self.community().ports.vcs;
        read_file(vcs.as_ref(), &VcsRepo::of_repo(repo), at, path)
            .await
            .ok()
            .flatten()
            .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
    }
}

#[tonic::async_trait]
impl InsightsService for ForgeServices {
    async fn get_language_stats(
        &self,
        req: Request<GetLanguageStatsRequest>,
    ) -> Result<Response<GetLanguageStatsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            let (changeset, tree) = self.ins_tree(&repo, &r.at).await?;
            let tree_id = self
                .community()
                .ports
                .vcs
                .tree_id(&VcsRepo::of_repo(&repo), &changeset)
                .await?;
            let (languages, total_bytes) = repofiles::language_stats(
                &tree,
                r.include_vendored,
                r.include_generated,
                r.include_documentation,
            );
            Ok(GetLanguageStatsResponse {
                languages,
                total_bytes,
                changeset: Some(changeset),
                tree: Some(tree_id),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_license_info(
        &self,
        req: Request<GetLicenseInfoRequest>,
    ) -> Result<Response<LicenseInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            let (changeset, tree) = self.ins_tree(&repo, &r.at).await?;
            let mut files = Vec::new();
            for f in repofiles::license_files(&tree) {
                if let Some(t) = self.ins_text(&repo, &changeset, &f.path).await {
                    files.push((f, t));
                }
            }
            let mut manifest = None;
            if files.is_empty() {
                for p in ["Cargo.toml", "package.json"] {
                    if let Some(t) = self.ins_text(&repo, &changeset, p).await
                        && let Some(l) = repofiles::manifest_license(p, &t)
                    {
                        manifest = Some((p, l));
                        break;
                    }
                }
            }
            Ok(repofiles::license_info(&files, manifest, &changeset))
        }
        .await;
        reply(&caller, out)
    }

    async fn get_community_profile(
        &self,
        req: Request<GetCommunityProfileRequest>,
    ) -> Result<Response<CommunityProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.ins_read(&caller, &r.repo).await?;
            let (changeset, tree) = self.ins_tree(&repo, &r.at).await?;
            let coc_path = repofiles::community_files(&tree, None)
                .into_iter()
                .find(|f| f.kind == CommunityFileKind::CodeOfConduct as i32 && f.present)
                .and_then(|f| {
                    f.paths
                        .first()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                });
            let coc = match coc_path {
                Some(p) => self.ins_text(&repo, &changeset, &p).await,
                None => None,
            };
            let files = repofiles::community_files(&tree, coc.as_deref());
            let settings = self.fg().orgs.repo_settings(&cx, &repo.id).await?;
            let mut p = repofiles::community_profile(
                files,
                !repo.description.trim().is_empty(),
                settings.has_discussions,
                &changeset,
                self.fg().db.now_ms(),
            );
            p.repo = repo_ref_id(&repo.id);
            Ok(p)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_citation(
        &self,
        req: Request<GetCitationRequest>,
    ) -> Result<Response<CitationInfo>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            let (changeset, tree) = self.ins_tree(&repo, &r.at).await?;
            let file = tree
                .iter()
                .find(|f| f.path == "CITATION.cff")
                .ok_or_else(|| ForgeError::not_found("CITATION.cff"))?;
            let text = self
                .ins_text(&repo, &changeset, &file.path)
                .await
                .ok_or_else(|| ForgeError::not_found("CITATION.cff"))?;
            repofiles::citation(&file.path, &file.blob, &text)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_pulse(
        &self,
        req: Request<GetPulseRequest>,
    ) -> Result<Response<PulseSummary>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.ins_read(&caller, &r.repo).await?;
            let w = Insights::check_window(r.window)?;
            self.community().insights.pulse(&cx, &repo, w).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_contributors(
        &self,
        req: Request<ListContributorsRequest>,
    ) -> Result<Response<ListContributorsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.ins_read(&caller, &r.repo).await?;
            let (token, limit) = page(&r.page);
            let q = ContributorQuery {
                ref_name: expr(&r.at),
                kind: r.kind,
                since_ms: r.since.as_ref().map(ts_ms).unwrap_or(0),
                until_ms: r.until.as_ref().map(ts_ms).unwrap_or(0),
                include_weeks: r.include_weeks,
            };
            let c = self
                .community()
                .insights
                .contributors(&cx, &repo, &q, &token, limit)
                .await?;
            Ok(ListContributorsResponse {
                contributors: c.page,
                human_count: c.humans,
                agent_count: c.agents,
                placeholder_count: c.placeholders,
                computing: false,
                page: next(c.next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_commit_activity(
        &self,
        req: Request<GetCommitActivityRequest>,
    ) -> Result<Response<GetCommitActivityResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            Ok(GetCommitActivityResponse {
                weeks: self.community().insights.commit_activity(&repo).await?,
                computing: false,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_code_frequency(
        &self,
        req: Request<GetCodeFrequencyRequest>,
    ) -> Result<Response<GetCodeFrequencyResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            Ok(GetCodeFrequencyResponse {
                weeks: self.community().insights.code_frequency(&repo).await?,
                computing: false,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_traffic_views(
        &self,
        req: Request<GetTrafficViewsRequest>,
    ) -> Result<Response<GetTrafficViewsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, None)
                .await?;
            let interval = bee_proto::v1::TrafficInterval::try_from(r.interval).unwrap_or_default();
            let (count, uniques, points) = self
                .community()
                .insights
                .traffic(&cx, &repo.id, false, interval)
                .await?;
            Ok(GetTrafficViewsResponse {
                count,
                uniques,
                points,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_traffic_clones(
        &self,
        req: Request<GetTrafficClonesRequest>,
    ) -> Result<Response<GetTrafficClonesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, None)
                .await?;
            let interval = bee_proto::v1::TrafficInterval::try_from(r.interval).unwrap_or_default();
            let (count, uniques, points) = self
                .community()
                .insights
                .traffic(&cx, &repo.id, true, interval)
                .await?;
            Ok(GetTrafficClonesResponse {
                count,
                uniques,
                points,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_traffic_referrers(
        &self,
        req: Request<ListTrafficReferrersRequest>,
    ) -> Result<Response<ListTrafficReferrersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, None)
                .await?;
            Ok(ListTrafficReferrersResponse {
                referrers: self.community().insights.referrers(&cx, &repo.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_traffic_popular_paths(
        &self,
        req: Request<ListTrafficPopularPathsRequest>,
    ) -> Result<Response<ListTrafficPopularPathsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .repo_cx(&caller, &repo, Action::Commit, None, None)
                .await?;
            Ok(ListTrafficPopularPathsResponse {
                paths: self
                    .community()
                    .insights
                    .popular_paths(&cx, &repo.id)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_network_graph(
        &self,
        req: Request<GetNetworkGraphRequest>,
    ) -> Result<Response<GetNetworkGraphResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            let since = r.since.as_ref().map(ts_ms).unwrap_or(0);
            let until = r.until.as_ref().map(ts_ms).unwrap_or(0);
            let (nodes, edges, truncated) = self
                .community()
                .insights
                .network(
                    &repo,
                    r.include_forks,
                    &r.refs,
                    since,
                    until,
                    r.max_nodes as usize,
                )
                .await?;
            Ok(GetNetworkGraphResponse {
                nodes,
                edges,
                truncated,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_insights_org_dependencies(
        &self,
        req: Request<ListInsightsOrgDependenciesRequest>,
    ) -> Result<Response<ListInsightsOrgDependenciesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            self.fg()
                .authz
                .org(&caller, &org_id, OrgPerm::ViewMembers)
                .await?;
            let q = DependencyQuery {
                ecosystem: r.ecosystem,
                query: r.query,
                license: r.license,
                vulnerable_only: r.vulnerable_only,
            };
            let (token, limit) = page(&r.page);
            let (dependencies, total_count, nt) = self
                .community()
                .insights
                .org_dependencies(&org_id, &q, &token, limit)
                .await?;
            Ok(ListInsightsOrgDependenciesResponse {
                dependencies,
                total_count,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_insights_agent_stats(
        &self,
        req: Request<ListInsightsAgentStatsRequest>,
    ) -> Result<Response<ListInsightsAgentStatsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, _) = self.ins_read(&caller, &r.repo).await?;
            let w = Insights::check_window(r.window)?;
            let (token, limit) = page(&r.page);
            let (agents, total, nt) = self
                .community()
                .insights
                .agent_stats(&repo, w, &token, limit)
                .await?;
            Ok(ListInsightsAgentStatsResponse {
                agents,
                total_cost: Some(total),
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}
