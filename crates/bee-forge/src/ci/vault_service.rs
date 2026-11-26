//! The tonic `SecretService` over the CI secret and variable stores ([`super::secrets`]).
//!
//! Secret values arrive as libsodium sealed boxes made with the scope's public key
//! ([`super::sealed`]) and are never returned by any RPC. Org, repo and environment scopes
//! are served here; workspace-user and agent scopes belong to the workspaces and agents
//! modules (`UNIMPLEMENTED`, subcode `served_elsewhere`).
//!
//! | RPCs | repo / environment scope | org scope |
//! |---|---|---|
//! | `GetSecretPublicKey`, `GetSecret`, `ListSecrets`, `GetVariable`, `ListVariables` | `commit` (write role) | org admin |
//! | `PutSecret`, `DeleteSecret`, `CreateVariable`, `UpdateVariable`, `DeleteVariable` | `manage_settings` (admin) | org admin |
//! | `ListRepoOrgSecrets` | `commit` on the repo | — |
//! | selected-repo RPCs | — (`INVALID_ARGUMENT`) | org admin |

use bee_policy::Action;
use bee_proto::v1::secret_service_server::SecretService;
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::engine::repo_ref_of;
use super::secrets::VaultScope;
use super::service::{CiServices, paged};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::service::{reply, unserved};

type Reply<T> = std::result::Result<Response<T>, Status>;

impl CiServices {
    /// Resolves and authorizes a secret scope; `write` asks for the admin level.
    async fn vault(
        &self,
        caller: &Caller,
        s: &Option<SecretScope>,
        write: bool,
        audit: &str,
    ) -> Result<(VaultScope, Cx)> {
        let s = s.clone().ok_or_else(|| ForgeError::missing("scope"))?;
        let kind = SecretScopeKind::try_from(s.kind).unwrap_or(SecretScopeKind::Unspecified);
        match kind {
            SecretScopeKind::Repo | SecretScopeKind::Environment => {
                let repo = self.base.repo(caller, &s.repo).await?;
                let cx = if write {
                    self.admin(caller, &repo, audit).await?
                } else {
                    self.forge()
                        .authz
                        .repo(caller, &repo, Action::Commit, &[], None, None)
                        .await?
                };
                if kind == SecretScopeKind::Repo {
                    return Ok((VaultScope::Repo(repo.id.clone(), repo.org_id.clone()), cx));
                }
                if s.environment.is_empty() {
                    return Err(ForgeError::missing("scope.environment"));
                }
                let env = self.ci.get_environment(&cx, &repo, &s.environment).await?;
                Ok((
                    VaultScope::Environment(repo.id.clone(), repo.org_id.clone(), env.name),
                    cx,
                ))
            }
            SecretScopeKind::Org => {
                let (org_id, cx) = self.org(caller, &s.org, OrgPerm::Manage, audit).await?;
                Ok((VaultScope::Org(org_id), cx))
            }
            SecretScopeKind::WorkspaceUser | SecretScopeKind::Agent => Err(unserved(
                "SecretService (workspace-user and agent scopes)",
                "workspaces and agents",
            )),
            SecretScopeKind::Unspecified => Err(ForgeError::missing("scope.kind")),
        }
    }

    async fn org_vault(
        &self,
        caller: &Caller,
        s: &Option<SecretScope>,
        audit: &str,
    ) -> Result<(VaultScope, Cx)> {
        let kind = s
            .as_ref()
            .and_then(|s| SecretScopeKind::try_from(s.kind).ok())
            .unwrap_or(SecretScopeKind::Unspecified);
        if kind != SecretScopeKind::Org {
            return Err(ForgeError::invalid(
                "invalid_scope",
                "selected repositories apply to org secrets and variables only",
            ));
        }
        self.vault(caller, s, true, audit).await
    }
}

fn visibility(v: i32) -> SecretVisibility {
    SecretVisibility::try_from(v).unwrap_or(SecretVisibility::Unspecified)
}

fn refs(ids: Vec<String>) -> Vec<RepoRef> {
    ids.iter().filter_map(|id| repo_ref_of(id)).collect()
}

#[tonic::async_trait]
impl SecretService for CiServices {
    async fn get_secret_public_key(
        &self,
        req: Request<GetSecretPublicKeyRequest>,
    ) -> Reply<SecretPublicKey> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, _) = self
                .vault(&caller, &r.scope, false, "secret.public_key")
                .await?;
            let k = self.ci.secret_public_key(&scope).await?;
            Ok(SecretPublicKey {
                scope: r.scope.clone(),
                key_id: k.key_id,
                key: k.public_key.to_vec(),
                created_at: None,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn put_secret(&self, req: Request<PutSecretRequest>) -> Reply<Secret> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.vault(&caller, &r.scope, true, "secret.put").await?;
            let selected = self.repo_ids(&caller, &r.selected_repos).await?;
            self.ci
                .put_sealed_secret(
                    &cx,
                    &scope,
                    &r.name,
                    &r.encrypted_value,
                    &r.key_id,
                    visibility(r.visibility),
                    selected,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_secret(&self, req: Request<GetSecretRequest>) -> Reply<Secret> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.vault(&caller, &r.scope, false, "secret.get").await?;
            self.ci.get_secret(&cx, &scope, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_secrets(&self, req: Request<ListSecretsRequest>) -> Reply<ListSecretsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.vault(&caller, &r.scope, false, "secret.list").await?;
            let (secrets, page) = paged(self.ci.list_secrets(&cx, &scope).await?, &r.page);
            Ok(ListSecretsResponse { secrets, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_repo_org_secrets(
        &self,
        req: Request<ListRepoOrgSecretsRequest>,
    ) -> Reply<ListRepoOrgSecretsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .forge()
                .authz
                .repo(&caller, &repo, Action::Commit, &[], None, None)
                .await?;
            let (secrets, page) = paged(self.ci.repo_org_secrets(&cx, &repo).await?, &r.page);
            Ok(ListRepoOrgSecretsResponse { secrets, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_secret(
        &self,
        req: Request<DeleteSecretRequest>,
    ) -> Reply<DeleteSecretResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.vault(&caller, &r.scope, true, "secret.delete").await?;
            self.ci.delete_secret(&cx, &scope, &r.name).await?;
            Ok(DeleteSecretResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_secret_selected_repos(
        &self,
        req: Request<ListSecretSelectedReposRequest>,
    ) -> Reply<ListSecretSelectedReposResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "secret.list_repos")
                .await?;
            let ids = self.ci.secret_repos(&cx, &scope, &r.name).await?;
            let (repos, page) = paged(refs(ids), &r.page);
            Ok(ListSecretSelectedReposResponse { repos, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_secret_selected_repos(
        &self,
        req: Request<SetSecretSelectedReposRequest>,
    ) -> Reply<SetSecretSelectedReposResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "secret.set_repos")
                .await?;
            let ids = self.repo_ids(&caller, &r.repos).await?;
            self.ci.set_secret_repos(&cx, &scope, &r.name, ids).await?;
            Ok(SetSecretSelectedReposResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn add_secret_selected_repo(
        &self,
        req: Request<AddSecretSelectedRepoRequest>,
    ) -> Reply<AddSecretSelectedRepoResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "secret.set_repos")
                .await?;
            let id = self.base.repo(&caller, &r.selected_repo).await?.id;
            let mut ids = self.ci.secret_repos(&cx, &scope, &r.name).await?;
            ids.push(id);
            self.ci.set_secret_repos(&cx, &scope, &r.name, ids).await?;
            Ok(AddSecretSelectedRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_secret_selected_repo(
        &self,
        req: Request<RemoveSecretSelectedRepoRequest>,
    ) -> Reply<RemoveSecretSelectedRepoResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "secret.set_repos")
                .await?;
            let id = self.base.repo(&caller, &r.selected_repo).await?.id;
            let mut ids = self.ci.secret_repos(&cx, &scope, &r.name).await?;
            ids.retain(|x| *x != id);
            self.ci.set_secret_repos(&cx, &scope, &r.name, ids).await?;
            Ok(RemoveSecretSelectedRepoResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_variable(&self, req: Request<CreateVariableRequest>) -> Reply<Variable> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .vault(&caller, &r.scope, true, "variable.create")
                .await?;
            let selected = self.repo_ids(&caller, &r.selected_repos).await?;
            self.ci
                .create_variable(
                    &cx,
                    &scope,
                    &r.name,
                    &r.value,
                    visibility(r.visibility),
                    selected,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_variable(&self, req: Request<GetVariableRequest>) -> Reply<Variable> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.vault(&caller, &r.scope, false, "variable.get").await?;
            self.ci.get_variable(&cx, &scope, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_variables(
        &self,
        req: Request<ListVariablesRequest>,
    ) -> Reply<ListVariablesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .vault(&caller, &r.scope, false, "variable.list")
                .await?;
            let (variables, page) = paged(self.ci.list_variables(&cx, &scope).await?, &r.page);
            Ok(ListVariablesResponse { variables, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_variable(&self, req: Request<UpdateVariableRequest>) -> Reply<Variable> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .vault(&caller, &r.scope, true, "variable.update")
                .await?;
            self.ci
                .update_variable(
                    &cx,
                    &scope,
                    &r.name,
                    r.new_name.as_deref(),
                    r.value.as_deref(),
                    r.visibility.map(visibility),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_variable(
        &self,
        req: Request<DeleteVariableRequest>,
    ) -> Reply<DeleteVariableResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .vault(&caller, &r.scope, true, "variable.delete")
                .await?;
            self.ci.delete_variable(&cx, &scope, &r.name).await?;
            Ok(DeleteVariableResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_variable_selected_repos(
        &self,
        req: Request<ListVariableSelectedReposRequest>,
    ) -> Reply<ListVariableSelectedReposResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "variable.list_repos")
                .await?;
            let ids = self.ci.variable_repos(&cx, &scope, &r.name).await?;
            let (repos, page) = paged(refs(ids), &r.page);
            Ok(ListVariableSelectedReposResponse { repos, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_variable_selected_repos(
        &self,
        req: Request<SetVariableSelectedReposRequest>,
    ) -> Reply<SetVariableSelectedReposResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .org_vault(&caller, &r.scope, "variable.set_repos")
                .await?;
            let ids = self.repo_ids(&caller, &r.repos).await?;
            self.ci
                .set_variable_repos(&cx, &scope, &r.name, ids)
                .await?;
            Ok(SetVariableSelectedReposResponse {})
        }
        .await;
        reply(&caller, out)
    }
}
