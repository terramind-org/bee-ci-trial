//! GitHub imports run by this server (`docs/spec/importers.md` "What the server wires"):
//! `bee_github::Importer` with a [`StoreTarget`] over the server's stores (with the eager
//! derived rows CreateChangeset writes) and a [`HubSink`] over the forge, keeping its state
//! under `<data>/github-imports/<import id>`; live sync by webhook (`POST /webhooks/github`)
//! and by polling; identity claims verified with a GitHub OAuth code.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bee_github::auth::{AppCredential, Credential};
use bee_github::clock::SystemClock;
use bee_github::hub::HubSink;
use bee_github::principals::ClaimProof;
use bee_github::state::StateDir;
use bee_github::target::StoreTarget;
use bee_github::{Client, ClientConfig, Importer, Options};
use bee_import::ImportTarget;
use bee_proto::v1::{ErrorReason, Principal, PrincipalKind};
use bee_store::{OrgRow, RepoRow};
use serde_json::{Value, json};

use super::ForgeWiring;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::state::{AppState, RepoCtx};

/// One running GitHub import.
pub struct GithubImport {
    /// The server's import id.
    pub id: String,
    /// `owner/name` on GitHub.
    pub github_repo: String,
    /// The target repo id.
    pub repo_id: String,
    /// The importer.
    pub importer: Arc<Importer>,
}

/// The GitHub imports of this process.
#[derive(Default)]
pub struct GithubImports {
    entries: Mutex<BTreeMap<String, Arc<GithubImport>>>,
}

impl GithubImports {
    /// Every running import.
    pub fn all(&self) -> Vec<Arc<GithubImport>> {
        self.entries
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// One import by id.
    pub fn get(&self, id: &str) -> Option<Arc<GithubImport>> {
        self.entries.lock().ok()?.get(id).cloned()
    }

    /// The import of a GitHub repository (`owner/name`, case-insensitive).
    pub fn by_github_repo(&self, full_name: &str) -> Option<Arc<GithubImport>> {
        self.all()
            .into_iter()
            .find(|e| e.github_repo.eq_ignore_ascii_case(full_name))
    }

    fn insert(&self, e: Arc<GithubImport>) {
        if let Ok(mut m) = self.entries.lock() {
            m.insert(e.id.clone(), e);
        }
    }
}

fn gh(e: bee_core::BeeError) -> ApiError {
    ApiError::from(e)
}

/// The GitHub client settings of the server.
pub fn client_config(state: &AppState) -> ClientConfig {
    if state.cfg.import.github_url.is_empty() {
        ClientConfig::default()
    } else {
        ClientConfig::at(&state.cfg.import.github_url)
    }
}

/// The credential for a `StartImport`: the request's token, else the configured GitHub App
/// with the request's installation id.
pub fn credential(state: &AppState, token: &str, installation_id: &str) -> ApiResult<Credential> {
    if !token.is_empty() {
        return Ok(Credential::Token(token.to_string()));
    }
    let cfg = &state.cfg.import;
    if cfg.github_app_id.is_empty() || cfg.github_app_private_key_file.is_empty() {
        return Err(ApiError::new(
            ErrorReason::InvalidState,
            "this server has no GitHub App (import.github_app_id); pass github_token",
        ));
    }
    let pem = std::fs::read_to_string(&cfg.github_app_private_key_file).map_err(|e| {
        ApiError::new(
            ErrorReason::Internal,
            format!("cannot read the GitHub App key: {e}"),
        )
    })?;
    let inst: u64 = installation_id
        .parse()
        .map_err(|_| ApiError::invalid("invalid_field", "github_installation_id is a number"))?;
    Ok(Credential::App(
        AppCredential::new(cfg.github_app_id.clone(), &pem, inst).map_err(gh)?,
    ))
}

impl ForgeWiring {
    /// Builds the importer of `github_repo` into `repo` (giving the repo its forge row
    /// first, with `owner` as the org's owner when the org is new to the forge).
    #[allow(clippy::too_many_arguments)]
    pub async fn github_importer(
        &self,
        id: &str,
        github_repo: &str,
        org: &OrgRow,
        repo: &RepoRow,
        owner: &str,
        creator: &str,
        credential: Credential,
    ) -> ApiResult<Arc<GithubImport>> {
        let state = self
            .state()
            .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "server stopping"))?;
        let ctx = RepoCtx {
            full_name: format!("{}/{}", org.name, repo.name),
            org: org.clone(),
            repo: repo.clone(),
        };
        let owner_p = match state.principal(owner).await? {
            Some(r) => super::adapters::principal_of(&r),
            None => Principal {
                id: owner.to_string(),
                kind: PrincipalKind::Human as i32,
                ..Default::default()
            },
        };
        self.adopt(&ctx, &owner_p).await?;
        let frepo = self
            .forge
            .orgs
            .repo_any(&bee_forge::Cx::system(), &ctx.id_text())
            .await
            .map_err(super::forge_to_api)?;
        let target = Arc::new(StoreTarget {
            main: ImportTarget {
                writer: state.writer.clone(),
                org: org.id,
                repo: repo.id,
            },
            wiki: None,
            creator: creator.to_string(),
            derived: Some(Arc::new(crate::services::import::EagerHook::new(
                state.registry.clone(),
            ))),
        });
        let sink = HubSink::new(
            self.forge.clone(),
            self.changes.clone(),
            frepo,
            state.writer.clone(),
        )
        .map_err(gh)?;
        let client = Client::new(client_config(&state), credential, Arc::new(SystemClock));
        let dir = state.cfg.server.data_dir.join("github-imports").join(id);
        let mut opts = Options::new(github_repo);
        opts.hub_url = state.public_url();
        opts.hub_org = Some(org.name.clone());
        opts.wiki = false;
        opts.at_ms = state.now_ms();
        let importer = Importer::new(
            client,
            StateDir::open(dir).map_err(gh)?,
            target,
            Arc::new(sink),
            opts,
        );
        let e = Arc::new(GithubImport {
            id: id.to_string(),
            github_repo: github_repo.to_string(),
            repo_id: ctx.id_text(),
            importer: Arc::new(importer),
        });
        self.github.insert(e.clone());
        Ok(e)
    }

    /// Polls every live import once (`Importer::sync`).
    pub async fn sync_github(&self) {
        for e in self.github.all() {
            if let Err(err) = e.importer.sync().await {
                logging::log(
                    Level::Warn,
                    "import",
                    "GitHub live sync failed",
                    &[
                        ("import", e.id.clone().into()),
                        ("error", err.message.into()),
                    ],
                );
            }
        }
    }

    /// Claims the placeholders of a GitHub account for `principal` in every import of this
    /// process; returns how many records moved.
    pub async fn claim_github(&self, github_id: u64, principal: &str) -> ApiResult<u64> {
        let mut moved = 0;
        let mut found = false;
        for e in self.github.all() {
            match e
                .importer
                .claim(
                    github_id,
                    &ClaimProof::VerifiedGithubId(github_id),
                    principal,
                )
                .await
            {
                Ok((_, n)) => {
                    found = true;
                    moved += n;
                }
                Err(err) if err.reason == bee_core::Reason::NotFound => {}
                Err(err) => return Err(gh(err)),
            }
        }
        if !found {
            return Err(ApiError::not_found(format!(
                "GitHub user {github_id} has no placeholder in an import here"
            )));
        }
        Ok(moved)
    }
}

/// Exchanges a GitHub OAuth code (the server's `import.github_oauth_*` app) for the GitHub
/// account it proves: `(numeric id, login)`.
pub async fn verify_github_code(state: &AppState, code: &str) -> ApiResult<(u64, String)> {
    let cfg = &state.cfg.import;
    if cfg.github_oauth_client_id.is_empty() {
        return Err(ApiError::new(
            ErrorReason::InvalidState,
            "this server has no GitHub OAuth app (import.github_oauth_client_id)",
        ));
    }
    let cc = client_config(state);
    let anon = Client::new(cc.clone(), Credential::Anonymous, Arc::new(SystemClock));
    let r = anon
        .post_raw(
            &format!("{}/login/oauth/access_token", cc.web_url),
            &json!({
                "client_id": cfg.github_oauth_client_id,
                "client_secret": cfg.github_oauth_client_secret,
                "code": code,
            }),
            &[("accept", "application/json".to_string())],
            false,
        )
        .await
        .map_err(gh)?;
    let v: Value = r.json().map_err(gh)?;
    let Some(token) = v["access_token"].as_str() else {
        return Err(ApiError::new(
            ErrorReason::PermissionDenied,
            format!(
                "GitHub refused the code: {}",
                v["error_description"].as_str().unwrap_or("no access token")
            ),
        ));
    };
    let user = Client::new(
        cc,
        Credential::Token(token.to_string()),
        Arc::new(SystemClock),
    )
    .get_json("/user")
    .await
    .map_err(gh)?;
    let id = user["id"]
        .as_u64()
        .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "GitHub returned no user id"))?;
    Ok((id, user["login"].as_str().unwrap_or_default().to_string()))
}
