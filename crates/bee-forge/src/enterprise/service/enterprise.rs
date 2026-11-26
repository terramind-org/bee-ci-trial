//! `EnterpriseService`.

use bee_proto::v1::enterprise_service_server::EnterpriseService;
use bee_proto::v1::{
    AddEnterpriseOrgRequest, AddEnterpriseOwnerRequest, AddVerifiedDomainRequest,
    CheckVerifiedDomainRequest, CreateIpAllowListEntryRequest, CreateScimTokenRequest,
    CreateScimTokenResponse, DeleteIpAllowListEntryRequest, DeleteIpAllowListEntryResponse,
    DeleteVerifiedDomainRequest, DeleteVerifiedDomainResponse, Enterprise, EnterpriseOrg,
    EnterpriseOwner, EnterprisePolicies, EnterpriseScope, EnterpriseTwoFactorRequirement,
    GetEnterprisePoliciesRequest, GetEnterpriseRequest, GetIpAllowListSettingsRequest,
    GetSamlSsoConfigRequest, GetScimConfigRequest, GetTwoFactorRequirementRequest,
    GetVerifiedDomainSettingsRequest, IpAllowListEntry, IpAllowListSettings,
    ListEnterpriseOrgsRequest, ListEnterpriseOrgsResponse, ListEnterpriseOwnersRequest,
    ListEnterpriseOwnersResponse, ListIpAllowListEntriesRequest, ListIpAllowListEntriesResponse,
    ListSamlIdentitiesRequest, ListSamlIdentitiesResponse, ListScimIdentitiesRequest,
    ListScimIdentitiesResponse, ListTwoFactorNoncompliantMembersRequest,
    ListTwoFactorNoncompliantMembersResponse, ListVerifiedDomainsRequest,
    ListVerifiedDomainsResponse, RemoveEnterpriseOrgRequest, RemoveEnterpriseOrgResponse,
    RemoveEnterpriseOwnerRequest, RemoveEnterpriseOwnerResponse, RevokeSamlSessionRequest,
    RevokeSamlSessionResponse, SamlSsoConfig, ScimConfig, TestSamlSsoConfigRequest,
    TestSamlSsoConfigResponse, UpdateEnterprisePoliciesRequest, UpdateEnterpriseRequest,
    UpdateIpAllowListEntryRequest, UpdateIpAllowListSettingsRequest, UpdateSamlSsoConfigRequest,
    UpdateScimConfigRequest, UpdateTwoFactorRequirementRequest, UpdateTwoFactorRequirementResponse,
    UpdateVerifiedDomainSettingsRequest, VerifiedDomain, VerifiedDomainSettings, enterprise_scope,
};
use tonic::{Request, Response, Status};

use super::EnterpriseApi;
use crate::auth::OrgPerm;
use crate::enterprise::accounts::EnterprisePatch;
use crate::enterprise::common::{ScopeNeed, page_args, page_response};
use crate::enterprise::network::IpSettingsPatch;
use crate::enterprise::sso::SsoPatch;
use crate::error::ForgeError;
use crate::service::reply;

fn enterprise_scope_of(slug: &str) -> Option<EnterpriseScope> {
    Some(EnterpriseScope {
        scope: Some(enterprise_scope::Scope::Enterprise(slug.to_string())),
    })
}

#[tonic::async_trait]
impl EnterpriseService for EnterpriseApi {
    async fn get_enterprise(
        &self,
        req: Request<GetEnterpriseRequest>,
    ) -> Result<Response<Enterprise>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::View,
                    "enterprise.get",
                )
                .await?;
            self.ent.accounts.get(&cx, &rs.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_enterprise(
        &self,
        req: Request<UpdateEnterpriseRequest>,
    ) -> Result<Response<Enterprise>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.update",
                )
                .await?;
            self.ent
                .accounts
                .update(
                    &cx,
                    &rs.id,
                    EnterprisePatch {
                        name: r.name,
                        description: r.description,
                        website_url: r.website_url,
                        managed_users: r.managed_users,
                        managed_users_shortcode: r.managed_users_shortcode,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_enterprise_orgs(
        &self,
        req: Request<ListEnterpriseOrgsRequest>,
    ) -> Result<Response<ListEnterpriseOrgsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::View,
                    "enterprise.org_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (orgs, next) = self
                .ent
                .accounts
                .list_orgs(&cx, &rs.id, &r.query, &after, limit)
                .await?;
            Ok(ListEnterpriseOrgsResponse {
                orgs,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_enterprise_org(
        &self,
        req: Request<AddEnterpriseOrgRequest>,
    ) -> Result<Response<EnterpriseOrg>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.org_add",
                )
                .await?;
            let org = if r.create {
                let owner = crate::enterprise::common::principal_of(self.forge(), cx.root()).await;
                self.forge()
                    .orgs
                    .create_org(&cx, &r.org, &r.display, owner)
                    .await?
            } else {
                let org = self.forge().orgs.get_org(&cx, &r.org).await?;
                // The caller must own both sides.
                let auth = self.forge().authz.org(&caller, &org.id, OrgPerm::Own).await;
                self.svc
                    .guarded(&caller, &org.id, "", "enterprise.org_add", auth)
                    .await?;
                org
            };
            self.ent.accounts.add_org(&cx, &rs.id, &org).await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_enterprise_org(
        &self,
        req: Request<RemoveEnterpriseOrgRequest>,
    ) -> Result<Response<RemoveEnterpriseOrgResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.org_remove",
                )
                .await?;
            let org = self.forge().orgs.get_org(&cx, &r.org).await?;
            self.ent.accounts.remove_org(&cx, &rs.id, &org).await?;
            Ok(RemoveEnterpriseOrgResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_enterprise_owners(
        &self,
        req: Request<ListEnterpriseOwnersRequest>,
    ) -> Result<Response<ListEnterpriseOwnersResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::View,
                    "enterprise.owner_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (owners, next) = self.ent.accounts.owners(&cx, &rs.id, &after, limit).await?;
            Ok(ListEnterpriseOwnersResponse {
                owners,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_enterprise_owner(
        &self,
        req: Request<AddEnterpriseOwnerRequest>,
    ) -> Result<Response<EnterpriseOwner>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.owner_add",
                )
                .await?;
            self.ent
                .accounts
                .add_owner(&cx, &rs.id, &r.principal_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_enterprise_owner(
        &self,
        req: Request<RemoveEnterpriseOwnerRequest>,
    ) -> Result<Response<RemoveEnterpriseOwnerResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.owner_remove",
                )
                .await?;
            self.ent
                .accounts
                .remove_owner(&cx, &rs.id, &r.principal_id)
                .await?;
            Ok(RemoveEnterpriseOwnerResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- SAML

    async fn get_saml_sso_config(
        &self,
        req: Request<GetSamlSsoConfigRequest>,
    ) -> Result<Response<SamlSsoConfig>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(&caller, &r.scope, ScopeNeed::Manage, "enterprise.sso_get")
                .await?;
            self.ent.sso.config(&cx, &rs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_saml_sso_config(
        &self,
        req: Request<UpdateSamlSsoConfigRequest>,
    ) -> Result<Response<SamlSsoConfig>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.sso_update",
                )
                .await?;
            self.ent
                .sso
                .update_config(
                    &cx,
                    &rs,
                    SsoPatch {
                        enabled: r.enabled,
                        enforced: r.enforced,
                        idp_metadata_url: r.idp_metadata_url,
                        idp_metadata_xml: r.idp_metadata_xml,
                        idp_entity_id: r.idp_entity_id,
                        idp_sso_url: r.idp_sso_url,
                        idp_certificate_pem: r.idp_certificate_pem,
                        signature_method: r.signature_method,
                        digest_method: r.digest_method,
                        session_lifetime_seconds: r.session_lifetime_seconds,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn test_saml_sso_config(
        &self,
        req: Request<TestSamlSsoConfigRequest>,
    ) -> Result<Response<TestSamlSsoConfigResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self.scope(&caller, &r.scope, ScopeNeed::Manage, "enterprise.sso_test").await?;
            let mut res = self.ent.sso.test_config(&cx, &rs, &r.idp_metadata_xml).await?;
            if !r.idp_metadata_url.is_empty() && r.idp_metadata_xml.is_empty() {
                res.problems.push(
                    "metadata URLs are fetched by the metadata worker; pass idp_metadata_xml to test it now".into(),
                );
            }
            Ok(TestSamlSsoConfigResponse {
                metadata_ok: res.metadata_ok,
                problems: res.problems,
                test_sign_in_url: res.test_sign_in_url,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_saml_identities(
        &self,
        req: Request<ListSamlIdentitiesRequest>,
    ) -> Result<Response<ListSamlIdentitiesResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.sso_identity_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (identities, next) = self
                .ent
                .sso
                .identities(&cx, &rs, &r.query, &after, limit)
                .await?;
            Ok(ListSamlIdentitiesResponse {
                identities,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn revoke_saml_session(
        &self,
        req: Request<RevokeSamlSessionRequest>,
    ) -> Result<Response<RevokeSamlSessionResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.sso_session_revoke",
                )
                .await?;
            self.ent
                .sso
                .revoke_session(&cx, &rs, &r.principal_id, r.unlink_identity)
                .await?;
            Ok(RevokeSamlSessionResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- SCIM

    async fn get_scim_config(
        &self,
        req: Request<GetScimConfigRequest>,
    ) -> Result<Response<ScimConfig>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(&caller, &r.scope, ScopeNeed::Manage, "enterprise.scim_get")
                .await?;
            self.ent.scim.config(&cx, &rs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_scim_config(
        &self,
        req: Request<UpdateScimConfigRequest>,
    ) -> Result<Response<ScimConfig>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.scim_config_update",
                )
                .await?;
            self.ent
                .scim
                .update_config(&cx, &rs, r.enabled, r.provisions_teams)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_scim_token(
        &self,
        req: Request<CreateScimTokenRequest>,
    ) -> Result<Response<CreateScimTokenResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.scim_token_create",
                )
                .await?;
            let (token, token_id, expires) = self
                .ent
                .scim
                .create_token(&cx, &rs, &r.description, r.ttl_seconds)
                .await?;
            Ok(CreateScimTokenResponse {
                token,
                token_id,
                expires_at: expires.map(crate::ids::ts),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_scim_identities(
        &self,
        req: Request<ListScimIdentitiesRequest>,
    ) -> Result<Response<ListScimIdentitiesResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.scim_identity_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (identities, next) = self
                .ent
                .scim
                .list_identities(&cx, &rs, &r.query, r.include_inactive, &after, limit)
                .await?;
            Ok(ListScimIdentitiesResponse {
                identities,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- IP allow lists

    async fn get_ip_allow_list_settings(
        &self,
        req: Request<GetIpAllowListSettingsRequest>,
    ) -> Result<Response<IpAllowListSettings>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_settings_get",
                )
                .await?;
            self.ent.network.ip_settings(&cx, &rs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_ip_allow_list_settings(
        &self,
        req: Request<UpdateIpAllowListSettingsRequest>,
    ) -> Result<Response<IpAllowListSettings>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_settings_update",
                )
                .await?;
            self.ent
                .network
                .update_ip_settings(
                    &cx,
                    &rs,
                    IpSettingsPatch {
                        enabled: r.enabled,
                        include_installed_app_ranges: r.include_installed_app_ranges,
                        applies_to_installed_apps: r.applies_to_installed_apps,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_ip_allow_list_entries(
        &self,
        req: Request<ListIpAllowListEntriesRequest>,
    ) -> Result<Response<ListIpAllowListEntriesResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_entry_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (entries, next) = self.ent.network.ip_entries(&cx, &rs, &after, limit).await?;
            Ok(ListIpAllowListEntriesResponse {
                entries,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_ip_allow_list_entry(
        &self,
        req: Request<CreateIpAllowListEntryRequest>,
    ) -> Result<Response<IpAllowListEntry>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_entry_create",
                )
                .await?;
            self.ent
                .network
                .create_ip_entry(&cx, &rs, &r.cidr, &r.description, r.active)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_ip_allow_list_entry(
        &self,
        req: Request<UpdateIpAllowListEntryRequest>,
    ) -> Result<Response<IpAllowListEntry>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_entry_update",
                )
                .await?;
            self.ent
                .network
                .update_ip_entry(
                    &cx,
                    &rs,
                    &r.entry_id,
                    r.cidr.as_deref(),
                    r.description.as_deref(),
                    r.active,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_ip_allow_list_entry(
        &self,
        req: Request<DeleteIpAllowListEntryRequest>,
    ) -> Result<Response<DeleteIpAllowListEntryResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.ip_entry_delete",
                )
                .await?;
            self.ent
                .network
                .delete_ip_entry(&cx, &rs, &r.entry_id)
                .await?;
            Ok(DeleteIpAllowListEntryResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- domains

    async fn add_verified_domain(
        &self,
        req: Request<AddVerifiedDomainRequest>,
    ) -> Result<Response<VerifiedDomain>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_add",
                )
                .await?;
            self.ent
                .network
                .add_domain(&cx, &rs, &r.domain, r.approved_only)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn check_verified_domain(
        &self,
        req: Request<CheckVerifiedDomainRequest>,
    ) -> Result<Response<VerifiedDomain>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_check",
                )
                .await?;
            self.ent.network.check_domain(&cx, &rs, &r.domain_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_verified_domains(
        &self,
        req: Request<ListVerifiedDomainsRequest>,
    ) -> Result<Response<ListVerifiedDomainsResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_list",
                )
                .await?;
            let (after, limit) = page_args(&r.page);
            let (domains, next) = self.ent.network.domains(&cx, &rs, &after, limit).await?;
            Ok(ListVerifiedDomainsResponse {
                domains,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_verified_domain(
        &self,
        req: Request<DeleteVerifiedDomainRequest>,
    ) -> Result<Response<DeleteVerifiedDomainResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_delete",
                )
                .await?;
            self.ent
                .network
                .delete_domain(&cx, &rs, &r.domain_id)
                .await?;
            Ok(DeleteVerifiedDomainResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_verified_domain_settings(
        &self,
        req: Request<GetVerifiedDomainSettingsRequest>,
    ) -> Result<Response<VerifiedDomainSettings>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_settings_get",
                )
                .await?;
            self.ent.network.domain_settings(&cx, &rs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_verified_domain_settings(
        &self,
        req: Request<UpdateVerifiedDomainSettingsRequest>,
    ) -> Result<Response<VerifiedDomainSettings>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.domain_settings_update",
                )
                .await?;
            self.ent
                .network
                .update_domain_settings(&cx, &rs, r.restrict_notifications)
                .await
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- two-factor

    async fn get_two_factor_requirement(
        &self,
        req: Request<GetTwoFactorRequirementRequest>,
    ) -> Result<Response<EnterpriseTwoFactorRequirement>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.two_factor_get",
                )
                .await?;
            self.ent.network.two_factor(&cx, &rs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_two_factor_requirement(
        &self,
        req: Request<UpdateTwoFactorRequirementRequest>,
    ) -> Result<Response<UpdateTwoFactorRequirementResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.two_factor_update",
                )
                .await?;
            let (requirement, removed) = self
                .ent
                .network
                .update_two_factor(
                    &cx,
                    &rs,
                    r.required,
                    r.secure_methods_only,
                    r.remove_noncompliant,
                )
                .await?;
            Ok(UpdateTwoFactorRequirementResponse {
                requirement: Some(requirement),
                removed_principal_ids: removed,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_two_factor_noncompliant_members(
        &self,
        req: Request<ListTwoFactorNoncompliantMembersRequest>,
    ) -> Result<Response<ListTwoFactorNoncompliantMembersResponse>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &r.scope,
                    ScopeNeed::Manage,
                    "enterprise.two_factor_list",
                )
                .await?;
            let secure = self
                .ent
                .network
                .two_factor(&cx, &rs)
                .await?
                .secure_methods_only;
            let all = self.ent.network.noncompliant(&cx, &rs, secure).await?;
            let (after, limit) = page_args(&r.page);
            let key = |m: &bee_proto::v1::EnterpriseTwoFactorMember| {
                format!(
                    "{}\u{1f}{}",
                    m.org,
                    m.principal
                        .as_ref()
                        .map(|p| p.id.clone())
                        .unwrap_or_default()
                )
            };
            let mut members: Vec<_> = all.into_iter().filter(|m| key(m) > after).collect();
            let more = members.len() > limit;
            members.truncate(limit);
            let next = if more {
                members.last().map(key).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(ListTwoFactorNoncompliantMembersResponse {
                members,
                page: page_response(next),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------------------------------------------------------- policies

    async fn get_enterprise_policies(
        &self,
        req: Request<GetEnterprisePoliciesRequest>,
    ) -> Result<Response<EnterprisePolicies>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::View,
                    "enterprise.policies_get",
                )
                .await?;
            self.ent.accounts.policies(&cx, &rs.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_enterprise_policies(
        &self,
        req: Request<UpdateEnterprisePoliciesRequest>,
    ) -> Result<Response<EnterprisePolicies>, Status> {
        let caller = self.svc.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rs, cx) = self
                .scope(
                    &caller,
                    &enterprise_scope_of(&r.enterprise),
                    ScopeNeed::Manage,
                    "enterprise.policies_update",
                )
                .await?;
            let check = |what: &str, v: Option<i32>, max: i32| -> Result<(), ForgeError> {
                match v {
                    Some(x) if x <= 0 || x > max => Err(ForgeError::invalid(
                        "invalid_setting",
                        format!("{what} is not a valid value"),
                    )),
                    _ => Ok(()),
                }
            };
            check("repo_creation", r.repo_creation, 5)?;
            check("forking", r.forking, 5)?;
            check("base_permission", r.base_permission, 5)?;
            for (n, t) in [
                ("members_can_delete_repos", r.members_can_delete_repos),
                (
                    "members_can_change_repo_visibility",
                    r.members_can_change_repo_visibility,
                ),
                (
                    "members_can_invite_outside_collaborators",
                    r.members_can_invite_outside_collaborators,
                ),
                ("members_can_create_teams", r.members_can_create_teams),
                ("members_can_install_apps", r.members_can_install_apps),
                (
                    "agents_can_land_without_human_approval",
                    r.agents_can_land_without_human_approval,
                ),
            ] {
                check(n, t, 3)?;
            }
            self.ent
                .accounts
                .update_policies(&cx, &rs.id, |p| {
                    if let Some(v) = r.repo_creation {
                        p.repo_creation = v;
                    }
                    if let Some(v) = r.forking {
                        p.forking = v;
                    }
                    if let Some(v) = r.base_permission {
                        p.base_permission = v;
                    }
                    if let Some(v) = r.members_can_delete_repos {
                        p.members_can_delete_repos = v;
                    }
                    if let Some(v) = r.members_can_change_repo_visibility {
                        p.members_can_change_repo_visibility = v;
                    }
                    if let Some(v) = r.members_can_invite_outside_collaborators {
                        p.members_can_invite_outside_collaborators = v;
                    }
                    if let Some(v) = r.members_can_create_teams {
                        p.members_can_create_teams = v;
                    }
                    if let Some(v) = r.members_can_install_apps {
                        p.members_can_install_apps = v;
                    }
                    if let Some(v) = r.agents_can_land_without_human_approval {
                        p.agents_can_land_without_human_approval = v;
                    }
                    if r.actions.is_some() {
                        p.actions = r.actions.clone();
                    }
                })
                .await
        }
        .await;
        reply(&caller, out)
    }
}
