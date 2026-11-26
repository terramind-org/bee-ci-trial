//! Importing through a running `bee-server` over its HTTP/JSON surface.
//!
//! [`ServerTarget`] hands the local mirror to the server's `ImportService` (`StartImport`
//! with `source_kind = GIT` and the mirror path; the server must be allowed to read it,
//! which a `--dev` server on the same machine is), waits for it and reads the fidelity
//! report back. [`start_github_import`] asks the server to run the whole GitHub import
//! itself, for servers that host `bee-github`; servers that do not answer
//! `UNIMPLEMENTED`, and the CLI falls back to importing git here and keeping metadata in the
//! journal ([`UnwiredSink`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason};
use bee_import::ImportReport;
use bee_import::report::{Category, DroppedItem};
use bee_mcp::{ClientConfig, RpcClient};
use bee_proto::v1::ErrorDetail;
use serde_json::{Value, json};

use crate::clock::Clock;
use crate::error::{Result, unavailable};
use crate::principals::{Claim, Placeholder};
use crate::report::{Outcome, category, reason};
use crate::sink::{ChangeImport, ForgeSink};
use crate::target::{GitOptions, GitRepo, GitTarget, empty_report};

/// A server error as a [`BeeError`].
pub fn from_detail(d: ErrorDetail) -> BeeError {
    let r = Reason::parse(&d.reason).unwrap_or(Reason::Unavailable);
    let mut e = BeeError::new(r, d.message).with_retryable(d.retryable);
    if !d.subcode.is_empty() {
        e = e.with_payload("subcode", d.subcode.as_str());
    }
    e
}

/// A client of a bee server.
pub fn rpc(endpoint: &str, token: Option<String>) -> RpcClient {
    let mut c = ClientConfig::new(endpoint);
    c.token = token;
    c.request_timeout = Duration::from_secs(300);
    RpcClient::new(&c)
}

/// Imports git history through a server's `ImportService`.
pub struct ServerTarget {
    client: RpcClient,
    org: String,
    repo: String,
    clock: Arc<dyn Clock>,
    /// Wait between progress polls.
    pub poll: Duration,
    /// Polls before giving up (the server keeps importing; re-run to resume).
    pub max_polls: u32,
}

impl ServerTarget {
    /// A target writing to `org/repo` on the server behind `client`.
    pub fn new(client: RpcClient, org: &str, repo: &str, clock: Arc<dyn Clock>) -> ServerTarget {
        ServerTarget {
            client,
            org: org.into(),
            repo: repo.into(),
            clock,
            poll: Duration::from_millis(200),
            max_polls: 90_000,
        }
    }

    async fn call(&self, method: &str, req: Value) -> Result<Value> {
        self.client
            .call_rpc_json("ImportService", method, &req)
            .await
            .map_err(from_detail)
    }
}

fn report_from(source: &str, v: &Value) -> ImportReport {
    let mut r = empty_report(source);
    r.categories.clear();
    for it in v["items"].as_array().into_iter().flatten() {
        let n = |k: &str| {
            it[k]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or(it[k].as_u64())
                .unwrap_or(0)
        };
        let drops: Vec<DroppedItem> = it["drops"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| DroppedItem {
                item: d["ref"].as_str().unwrap_or("").to_string(),
                reason: d["reason"].as_str().unwrap_or("").to_string(),
                detail: "reported by the server".into(),
            })
            .collect();
        r.categories.push(Category {
            category: it["category"].as_str().unwrap_or("").to_string(),
            imported: n("imported"),
            archived: n("archived"),
            dropped: drops.len() as u64,
            excluded: 0,
            drops,
        });
    }
    r
}

#[async_trait]
impl GitTarget for ServerTarget {
    async fn import(
        &self,
        mirror: &Path,
        repo: GitRepo,
        opts: &GitOptions,
    ) -> Result<Option<ImportReport>> {
        let name = match repo {
            GitRepo::Main => self.repo.clone(),
            GitRepo::Wiki => format!("{}.wiki", self.repo),
        };
        let path = std::fs::canonicalize(mirror)
            .map_err(|e| crate::error::io("resolving the mirror path", e))?;
        // The server's git import leaves `refs/pull/*` out. `refs/heads/pull/<rest>` maps
        // to the same bee name (`pull/<rest>`), so the pull refs are exposed under it for
        // the duration of the import.
        let m = crate::git::Mirror::open(&path, None)?;
        let pulls: Vec<(String, String)> = m
            .refs()?
            .into_iter()
            .filter_map(|(n, sha)| {
                n.strip_prefix("refs/pull/")
                    .map(|rest| (format!("refs/heads/pull/{rest}"), sha))
            })
            .collect();
        if repo == GitRepo::Main {
            m.update_refs(&pulls)?;
        }
        let result = self.import_view(&path, &name, repo, opts).await;
        if repo == GitRepo::Main && !pulls.is_empty() {
            let input: String = pulls.iter().map(|(n, _)| format!("delete {n}\n")).collect();
            m.run_with_input(&["update-ref", "--stdin"], input.into_bytes())?;
        }
        result
    }

    async fn changeset(&self, _sha: &str) -> Result<Option<ObjectId>> {
        Ok(None)
    }
}

impl ServerTarget {
    async fn import_view(
        &self,
        path: &Path,
        name: &str,
        repo: GitRepo,
        opts: &GitOptions,
    ) -> Result<Option<ImportReport>> {
        let started = self
            .call(
                "StartImport",
                json!({
                    "source": format!("{}/{}", self.org, name),
                    "targetOrg": self.org,
                    "phase": "IMPORT_PHASE_MIRROR",
                    "sourceKind": "IMPORT_SOURCE_KIND_GIT",
                    "sourceUrl": path.display().to_string(),
                }),
            )
            .await;
        let started = match started {
            Ok(v) => v,
            Err(e) if repo == GitRepo::Wiki && e.reason == Reason::InvalidArgument => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let id = started["id"].as_str().unwrap_or("").to_string();
        for _ in 0..self.max_polls {
            let v = self.call("GetImport", json!({ "id": id })).await?;
            match v["state"].as_str().unwrap_or("") {
                "IMPORT_STATE_LIVE" => {
                    let rep = self.call("GetFidelityReport", json!({ "id": id })).await?;
                    return Ok(Some(report_from(&opts.source, &rep)));
                }
                "IMPORT_STATE_FAILED" | "IMPORT_STATE_CANCELLED" => {
                    return Err(unavailable(format!("the server's git import {id} failed")));
                }
                _ => self.clock.sleep(self.poll).await,
            }
        }
        Err(unavailable(format!(
            "the server's git import {id} is still running; re-run with --resume"
        )))
    }
}

/// Asks the server to run the whole GitHub import (`source_kind = GITHUB`). `Ok(None)`
/// when the server does not host forge imports (`UNIMPLEMENTED`).
pub async fn start_github_import(
    client: &RpcClient,
    repo: &str,
    target_org: &str,
    github_token: &str,
) -> Result<Option<String>> {
    match client
        .call_rpc_json(
            "ImportService",
            "StartImport",
            &json!({
                "source": repo,
                "targetOrg": target_org,
                "phase": "IMPORT_PHASE_MIRROR",
                "sourceKind": "IMPORT_SOURCE_KIND_GITHUB",
                "githubToken": github_token,
            }),
        )
        .await
    {
        Ok(v) => Ok(Some(v["id"].as_str().unwrap_or("").to_string())),
        Err(d) if d.reason == "UNIMPLEMENTED" => Ok(None),
        Err(d) => Err(from_detail(d)),
    }
}

/// A sink for servers that do not host the forge yet: every record is dropped with
/// [`reason::NOT_SUPPORTED_BY_TARGET`] (it stays in the journal for a later `--resume`).
#[derive(Debug, Default)]
pub struct UnwiredSink;

#[async_trait]
impl ForgeSink for UnwiredSink {
    fn name(&self) -> &str {
        "unwired"
    }

    async fn principals(&self, all: &[Placeholder]) -> Result<Vec<Outcome>> {
        Ok(all
            .iter()
            .map(|p| {
                Outcome::dropped(
                    category::PRINCIPALS,
                    format!("github-user/{}", p.github_id),
                    reason::NOT_SUPPORTED_BY_TARGET,
                    "the server does not run forge imports yet; kept in the journal",
                )
            })
            .collect())
    }

    async fn change(&self, c: &ChangeImport) -> Result<Vec<Outcome>> {
        Ok(vec![Outcome::dropped(
            category::PULL_REQUESTS,
            c.item(),
            reason::NOT_SUPPORTED_BY_TARGET,
            "the server does not run forge imports yet; kept in the journal",
        )])
    }

    async fn reassign(&self, _claim: &Claim) -> Result<u64> {
        Ok(0)
    }
}
