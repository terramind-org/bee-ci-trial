//! The signed transfer URLs CI hands out (`/_ci/...`), independent of the HTTP stack: the
//! server routes `/_ci/*` here with the method, the path and query, and the body.
//!
//! | method | path | serves |
//! |---|---|---|
//! | `GET` | `/_ci/jobs/<job>/secrets/<nonce>` | the job's secrets, once (JSON object) |
//! | `PUT`/`POST` | `/_ci/repos/<repo>/artifacts/<id>/upload` | artifact bytes |
//! | `GET` | `/_ci/repos/<repo>/artifacts/<id>/download` | artifact bytes |
//! | `PUT`/`POST` | `/_ci/repos/<repo>/caches/<id>/upload` | cache bytes |
//! | `GET` | `/_ci/repos/<repo>/caches/<id>/download` | cache bytes |
//! | `GET` | `/_ci/repos/<repo>/jobs/<job>/logs` | one job's log (text) |
//! | `GET` | `/_ci/repos/<repo>/runs/<run>/attempts/<n>/logs.zip` | a run attempt's logs (zip) |
//!
//! Every URL must carry a valid, unexpired signature ([`super::UrlSigner`]); the signature is
//! the authorization, so these handlers run as the system principal.

use super::engine::Ci;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

/// Largest body an upload URL accepts in one request.
pub const MAX_TRANSFER_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// What a transfer URL answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferReply {
    /// `Content-Type` of `body`.
    pub content_type: &'static str,
    /// The response body (empty for uploads).
    pub body: Vec<u8>,
}

impl TransferReply {
    fn bytes(content_type: &'static str, body: Vec<u8>) -> TransferReply {
        TransferReply { content_type, body }
    }
}

impl Ci {
    /// Serves one signed transfer URL. `path_and_query` is the request target (`/_ci/...?
    /// expires=...&signature=...`). Errors: PERMISSION_DENIED (unsigned, bad signature, wrong
    /// method), TOKEN_EXPIRED, NOT_FOUND, plus the errors of the underlying store method.
    pub async fn serve_transfer(
        &self,
        method: &str,
        path_and_query: &str,
        body: Vec<u8>,
    ) -> Result<TransferReply> {
        let path = self.urls.verify(path_and_query, self.now())?;
        let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let cx = Cx::system();
        let get = method.eq_ignore_ascii_case("GET");
        let put = method.eq_ignore_ascii_case("PUT") || method.eq_ignore_ascii_case("POST");
        let wrong = || ForgeError::permission_denied(format!("{method} is not allowed on {path}"));
        match seg.as_slice() {
            ["_ci", "jobs", job, "secrets", nonce] if get => {
                let secrets = self.redeem_secrets(&cx, job, nonce).await?;
                let json = serde_json::to_vec(&secrets)
                    .map_err(|e| ForgeError::internal(format!("secrets json: {e}")))?;
                Ok(TransferReply::bytes("application/json", json))
            }
            [
                "_ci",
                "repos",
                repo,
                kind @ ("artifacts" | "caches"),
                id,
                "upload",
            ] if put => {
                if body.len() > MAX_TRANSFER_BYTES {
                    return Err(ForgeError::too_large(
                        "transfer_bytes",
                        MAX_TRANSFER_BYTES as u64,
                        body.len() as u64,
                    ));
                }
                if *kind == "artifacts" {
                    self.upload_artifact(&cx, repo, id, body).await?;
                } else {
                    self.upload_cache(&cx, repo, id, body).await?;
                }
                Ok(TransferReply::bytes("text/plain", Vec::new()))
            }
            ["_ci", "repos", repo, "artifacts", id, "download"] if get => Ok(TransferReply::bytes(
                "application/zip",
                self.artifact_bytes(&cx, repo, id).await?,
            )),
            ["_ci", "repos", repo, "caches", id, "download"] if get => Ok(TransferReply::bytes(
                "application/octet-stream",
                self.cache_bytes(&cx, repo, id).await?,
            )),
            ["_ci", "repos", repo, "jobs", job, "logs"] if get => Ok(TransferReply::bytes(
                "text/plain; charset=utf-8",
                self.job_log_bytes(&cx, repo, job).await?,
            )),
            ["_ci", "repos", repo, "runs", run, "attempts", n, "logs.zip"] if get => {
                let attempt = n
                    .parse()
                    .map_err(|_| ForgeError::invalid("bad_attempt", format!("attempt {n}")))?;
                Ok(TransferReply::bytes(
                    "application/zip",
                    self.run_logs_zip(&cx, repo, run, attempt).await?,
                ))
            }
            ["_ci", "jobs", _, "secrets", _]
            | [
                "_ci",
                "repos",
                _,
                "artifacts" | "caches",
                _,
                "upload" | "download",
            ]
            | ["_ci", "repos", _, "jobs", _, "logs"]
            | ["_ci", "repos", _, "runs", _, "attempts", _, "logs.zip"] => Err(wrong()),
            _ => Err(ForgeError::not_found(format!("transfer URL {path}"))),
        }
    }
}
