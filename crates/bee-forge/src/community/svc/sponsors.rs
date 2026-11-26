//! `SponsorService`. Reads need a token that allows `read`; changes need `commit`. A user
//! manages their own sponsor data; an org's is managed by its owners and admins.

use bee_policy::Action;
use bee_proto::v1::sponsor_service_server::SponsorService;
use bee_proto::v1::{
    CancelSponsorshipRequest, CreateSponsorTierRequest, CreateSponsorshipRequest,
    CreateSponsorshipResponse, DeleteSponsorTierRequest, DeleteSponsorTierResponse,
    GetSponsorFiscalHostRequest, GetSponsorFundingFileRequest, GetSponsorPayoutSummaryRequest,
    GetSponsorProfileRequest, GetSponsorshipRequest, ListSponsorContentAccessRequest,
    ListSponsorContentAccessResponse, ListSponsorTiersRequest, ListSponsorTiersResponse,
    ListSponsorshipsRequest, ListSponsorshipsResponse, RepoRole, SponsorFiscalHost,
    SponsorFundingFile, SponsorPayoutSummary, SponsorProfile, SponsorTier, Sponsorship,
    SponsorshipRole, UpdateSponsorProfileRequest, UpdateSponsorTierRequest,
};
use tonic::{Request, Response, Status};

use crate::auth::{Caller, OrgPerm};
use crate::community::common::ref_repo_id;
use crate::community::ports::VcsRepo;
use crate::community::sponsors::{NewSponsorship, NewTier, Owner, TierPatch};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::service::{ForgeServices, next, page, reply};

impl ForgeServices {
    /// The sponsorable a login names: a user, else an org; empty means the caller.
    async fn sowner(&self, caller: &Caller, login: &str) -> Result<Owner> {
        let cx = &caller.cx;
        if login.is_empty() {
            return self.user_owner(cx, &cx.actor_id).await;
        }
        match self.community().users.resolve_login(cx, login).await {
            Ok((pid, _, _)) => self.user_owner(cx, &pid).await,
            Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {
                let o = self.fg().orgs.get_org(cx, login).await?;
                Ok(Owner::org(&o.id, &o.name))
            }
            Err(e) => Err(e),
        }
    }

    async fn user_owner(&self, cx: &Cx, pid: &str) -> Result<Owner> {
        let p = self.community().users.profile(cx, pid, "").await?;
        Ok(Owner::user(&p.principal.unwrap_or_default(), &p.login))
    }

    async fn owner_of_key(&self, cx: &Cx, key: &str) -> Result<Owner> {
        match key.split_once(':') {
            Some(("user", id)) => self.user_owner(cx, id).await,
            Some(("org", id)) => {
                let o = self.fg().orgs.org_any(cx, id).await?;
                Ok(Owner::org(&o.id, &o.name))
            }
            _ => Err(ForgeError::not_found(format!("sponsorable {key}"))),
        }
    }

    /// Whether the caller manages an owner's sponsor data (themselves, or an org they own or
    /// administer).
    async fn manages(&self, caller: &Caller, o: &Owner) -> bool {
        match o.key.split_once(':') {
            Some(("user", id)) => id == caller.cx.actor_id,
            Some(("org", id)) => self
                .fg()
                .authz
                .org(caller, id, OrgPerm::Manage)
                .await
                .is_ok(),
            _ => false,
        }
    }

    async fn manage_cx(&self, caller: &Caller, o: &Owner, audit: &str) -> Result<Cx> {
        let cx = self.user_cx(caller, Action::Commit)?;
        match o.key.split_once(':') {
            Some(("user", id)) if id == caller.cx.actor_id => Ok(cx),
            Some(("org", id)) => {
                let auth = self.fg().authz.org(caller, id, OrgPerm::Manage).await;
                self.guarded(caller, id, "", audit, auth).await
            }
            _ => Err(ForgeError::permission_denied(format!(
                "only {} can change this",
                o.login
            ))),
        }
    }

    /// Principals whose private sponsorships the caller may see: themselves and the orgs
    /// they own or administer.
    async fn parties(&self, caller: &Caller) -> Vec<String> {
        let mut v = vec![caller.cx.actor_id.clone()];
        if let Ok((orgs, _)) = self
            .fg()
            .orgs
            .orgs_of(&caller.cx, &caller.cx.actor_id, "", 100)
            .await
        {
            for o in orgs {
                if self
                    .fg()
                    .authz
                    .org(caller, &o.id, OrgPerm::Manage)
                    .await
                    .is_ok()
                {
                    v.push(o.id);
                }
            }
        }
        v
    }

    /// Resolves sponsor-only repo references to ids the caller administers (and, for an org
    /// owner, that belong to the org).
    async fn tier_repos(
        &self,
        caller: &Caller,
        owner: &Owner,
        refs: &[bee_proto::v1::RepoRef],
    ) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for r in refs {
            let repo = self.repo(caller, &Some(r.clone())).await?;
            self.fg()
                .authz
                .repo(
                    caller,
                    &repo,
                    Action::ManageSettings,
                    &[],
                    None,
                    Some(RepoRole::Admin),
                )
                .await?;
            if let Some(("org", id)) = owner.key.split_once(':')
                && repo.org_id != id
            {
                return Err(ForgeError::invalid(
                    "foreign_repo",
                    format!("{} does not belong to {}", repo.full_name, owner.login),
                ));
            }
            ids.push(repo.id);
        }
        Ok(ids)
    }
}

#[tonic::async_trait]
impl SponsorService for ForgeServices {
    async fn get_sponsor_profile(
        &self,
        req: Request<GetSponsorProfileRequest>,
    ) -> Result<Response<SponsorProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            let own = self.manages(&caller, &o).await;
            self.community().sponsors.profile(&cx, &o, own).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_sponsor_profile(
        &self,
        req: Request<UpdateSponsorProfileRequest>,
    ) -> Result<Response<SponsorProfile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let o = self.sowner(&caller, &r.login).await?;
            let cx = self
                .manage_cx(&caller, &o, "sponsor.profile_update")
                .await?;
            self.community()
                .sponsors
                .update_profile(
                    &cx,
                    &o,
                    r.enabled,
                    r.introduction.as_deref(),
                    r.goal_cents,
                    r.goal_description.as_deref(),
                    r.currency.as_deref(),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_sponsor_tier(
        &self,
        req: Request<CreateSponsorTierRequest>,
    ) -> Result<Response<SponsorTier>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let o = self.sowner(&caller, &r.login).await?;
            let cx = self.manage_cx(&caller, &o, "sponsor.tier_create").await?;
            let repo_ids = self.tier_repos(&caller, &o, &r.sponsor_only_repos).await?;
            self.community()
                .sponsors
                .create_tier(
                    &cx,
                    &o,
                    NewTier {
                        name: r.name,
                        description: r.description,
                        amount_cents: r.amount_cents,
                        frequency: r.frequency,
                        custom_amount: r.custom_amount,
                        repo_ids,
                        welcome: r.welcome_message,
                        publish: r.publish,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_sponsor_tiers(
        &self,
        req: Request<ListSponsorTiersRequest>,
    ) -> Result<Response<ListSponsorTiersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            let own = self.manages(&caller, &o).await;
            let (token, limit) = page(&r.page);
            let (tiers, nt) = self
                .community()
                .sponsors
                .list_tiers(
                    &cx,
                    &o,
                    r.include_drafts && own,
                    r.include_retired,
                    &token,
                    limit,
                )
                .await?;
            Ok(ListSponsorTiersResponse {
                tiers,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_sponsor_tier(
        &self,
        req: Request<UpdateSponsorTierRequest>,
    ) -> Result<Response<SponsorTier>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self
                .community()
                .sponsors
                .tier_owner_key(&caller.cx, &r.tier_id)
                .await?;
            let o = self.owner_of_key(&caller.cx, &key).await?;
            let cx = self.manage_cx(&caller, &o, "sponsor.tier_update").await?;
            let repo_ids = if r.replace_sponsor_only_repos {
                Some(self.tier_repos(&caller, &o, &r.sponsor_only_repos).await?)
            } else {
                None
            };
            self.community()
                .sponsors
                .update_tier(
                    &cx,
                    &o,
                    &r.tier_id,
                    TierPatch {
                        name: r.name,
                        description: r.description,
                        amount_cents: r.amount_cents,
                        custom_amount: r.custom_amount,
                        repo_ids,
                        welcome: r.welcome_message,
                        state: r.state,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_sponsor_tier(
        &self,
        req: Request<DeleteSponsorTierRequest>,
    ) -> Result<Response<DeleteSponsorTierResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let key = self
                .community()
                .sponsors
                .tier_owner_key(&caller.cx, &r.tier_id)
                .await?;
            let o = self.owner_of_key(&caller.cx, &key).await?;
            let cx = self.manage_cx(&caller, &o, "sponsor.tier_delete").await?;
            self.community()
                .sponsors
                .delete_tier(&cx, &o, &r.tier_id)
                .await?;
            Ok(DeleteSponsorTierResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_sponsorship(
        &self,
        req: Request<CreateSponsorshipRequest>,
    ) -> Result<Response<CreateSponsorshipResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let maintainee = self.sowner(&caller, &r.maintainee_login).await?;
            let sponsor = if r.sponsor_org.is_empty() {
                self.user_owner(&cx, &cx.actor_id).await?
            } else {
                let o = self.fg().orgs.get_org(&cx, &r.sponsor_org).await?;
                let owner = Owner::org(&o.id, &o.name);
                self.manage_cx(&caller, &owner, "sponsor.create").await?;
                owner
            };
            let (sponsorship, checkout_url) = self
                .community()
                .sponsors
                .create_sponsorship(
                    &cx,
                    &maintainee,
                    &sponsor,
                    NewSponsorship {
                        tier_id: r.tier_id,
                        amount_cents: r.amount_cents,
                        frequency: r.frequency,
                        private: r.private,
                        note: r.note,
                    },
                )
                .await?;
            Ok(CreateSponsorshipResponse {
                sponsorship: Some(sponsorship),
                checkout_url,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sponsorship(
        &self,
        req: Request<GetSponsorshipRequest>,
    ) -> Result<Response<Sponsorship>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let parties = self.parties(&caller).await;
            self.community()
                .sponsors
                .get(&cx, &r.sponsorship_id, &parties)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_sponsorship(
        &self,
        req: Request<CancelSponsorshipRequest>,
    ) -> Result<Response<Sponsorship>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Commit)?;
            let parties = self.parties(&caller).await;
            self.community()
                .sponsors
                .cancel(&cx, &r.sponsorship_id, &parties)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_sponsorships(
        &self,
        req: Request<ListSponsorshipsRequest>,
    ) -> Result<Response<ListSponsorshipsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            let role = match SponsorshipRole::try_from(r.role).unwrap_or_default() {
                SponsorshipRole::Unspecified => SponsorshipRole::Sponsor,
                x => x,
            };
            let parties = self.parties(&caller).await;
            let (token, limit) = page(&r.page);
            let (sponsorships, nt) = self
                .community()
                .sponsors
                .list(
                    &cx,
                    role,
                    &o,
                    r.include_ended,
                    r.include_private,
                    &parties,
                    &token,
                    limit,
                )
                .await?;
            Ok(ListSponsorshipsResponse {
                sponsorships,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sponsor_funding_file(
        &self,
        req: Request<GetSponsorFundingFileRequest>,
    ) -> Result<Response<SponsorFundingFile>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.repo_cx(&caller, &repo, Action::Read, None, None)
                .await?;
            let expr = r.at.map(|a| a.expr).unwrap_or_default();
            let at = self
                .community()
                .ports
                .vcs
                .resolve(&VcsRepo::of_repo(&repo), &expr)
                .await?;
            self.community().sponsors.funding_file(&repo, &at).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sponsor_fiscal_host(
        &self,
        req: Request<GetSponsorFiscalHostRequest>,
    ) -> Result<Response<SponsorFiscalHost>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            self.community().sponsors.fiscal_host(&cx, &o).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sponsor_payout_summary(
        &self,
        req: Request<GetSponsorPayoutSummaryRequest>,
    ) -> Result<Response<SponsorPayoutSummary>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            if !self.manages(&caller, &o).await {
                return Err(ForgeError::permission_denied(format!(
                    "only {} can see their payouts",
                    o.login
                )));
            }
            self.community().sponsors.payout_summary(&cx, &o).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_sponsor_content_access(
        &self,
        req: Request<ListSponsorContentAccessRequest>,
    ) -> Result<Response<ListSponsorContentAccessResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.user_cx(&caller, Action::Read)?;
            let o = self.sowner(&caller, &r.login).await?;
            if !self.manages(&caller, &o).await {
                return Err(ForgeError::permission_denied(format!(
                    "only {} can see who has access",
                    o.login
                )));
            }
            let repo_id = ref_repo_id(&r.repo);
            let repo_id = if repo_id.is_empty() && r.repo.is_some() {
                self.repo(&caller, &r.repo).await?.id
            } else {
                repo_id
            };
            let (token, limit) = page(&r.page);
            let (accesses, nt) = self
                .community()
                .sponsors
                .content_access(&cx, &o, &repo_id, &token, limit)
                .await?;
            Ok(ListSponsorContentAccessResponse {
                accesses,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }
}
