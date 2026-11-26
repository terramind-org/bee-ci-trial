//! The signed evidence URLs (`/_evidence/...`), independent of the HTTP stack: the server
//! routes `/_evidence/*` here with the request target and body.
//!
//! | method | path | serves |
//! |---|---|---|
//! | `PUT` | `/_evidence/<repo>/<evidence>/<asset>/upload` | the bytes of one asset of a pending item ([`Briefs::receive`]) |
//! | `GET`, `HEAD` | `/_evidence/<repo>/<evidence>/<asset>/as/<chain>` | the bytes of one asset of a ready item |
//!
//! `<asset>` is the asset's index in `Evidence.assets`. Every URL carries a signature
//! ([`crate::ci::ports::UrlSigner`]) over its path and expiry. A download path names the
//! principal chain it was issued to (`<chain>`, [`crate::media::upload::encode_chain`], `-`
//! for a signed-out reader), so the signature binds it: the server that redeems a download
//! URL decides repo `read` again for that chain before calling [`Briefs::download`]
//! (SPEC §9.6). Responses carry [`crate::media::serve::SAFETY_HEADERS`], the sniffed content
//! type, `Content-Disposition: attachment` for anything but images, video and audio
//! ([`Download::disposition`]), and support `Range` ([`crate::media::serve::parse_range`]).

use bee_proto::v1::EvidenceState;

use super::store::Briefs;
use super::{DOWNLOAD_URL_TTL_MS, EVIDENCE, SURFACE, UPLOAD_URL_TTL_MS, URL_PREFIX};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::media::upload::{decode_chain, encode_chain};
use crate::media::{ChainLink, Kind, max_evidence_bytes};

/// What a valid upload URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadTarget {
    /// Repo id.
    pub repo_id: String,
    /// Evidence id.
    pub evidence_id: String,
    /// Asset index.
    pub index: usize,
}

/// What a valid download URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadTarget {
    /// Repo id.
    pub repo_id: String,
    /// Evidence id.
    pub evidence_id: String,
    /// Asset index.
    pub index: usize,
    /// The principal chain the URL was issued to (empty: a signed-out reader). The caller
    /// decides repo `read` for it before serving.
    pub chain: Vec<ChainLink>,
}

/// The bytes of one asset, and how to serve them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    /// The sniffed content type.
    pub content_type: String,
    /// `Content-Disposition`, when the asset must not be shown in the page.
    pub disposition: Option<&'static str>,
    /// `sha256:<hex>` of the bytes (an `ETag`).
    pub digest: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

fn chain_segment(chain: &[ChainLink]) -> String {
    if chain.is_empty() {
        "-".to_string()
    } else {
        encode_chain(chain)
    }
}

fn index_of(s: &str) -> Result<usize> {
    s.parse::<usize>()
        .map_err(|_| ForgeError::not_found(format!("evidence asset {s}")))
}

fn segments(path: &str) -> Result<Vec<&str>> {
    let rest = path
        .strip_prefix(URL_PREFIX)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| ForgeError::not_found(format!("evidence URL {path}")))?;
    Ok(rest.split('/').collect())
}

impl Briefs {
    /// The signed `PUT` URL of asset `index` of a pending item, valid from `now_ms` for
    /// [`UPLOAD_URL_TTL_MS`] (never past the item's own expiry); with its expiry.
    pub fn upload_url(
        &self,
        repo_id: &str,
        id: &str,
        index: usize,
        now_ms: u64,
        item_expires_ms: u64,
    ) -> (String, u64) {
        let exp = (now_ms + UPLOAD_URL_TTL_MS).min(item_expires_ms.max(now_ms));
        let path = format!("{URL_PREFIX}/{repo_id}/{id}/{index}/upload");
        (self.urls.sign(&path, exp), exp)
    }

    /// The signed `GET` URL of asset `index` for `chain`, valid from `now_ms` for
    /// [`DOWNLOAD_URL_TTL_MS`]; with its expiry.
    pub fn download_url(
        &self,
        repo_id: &str,
        id: &str,
        index: usize,
        chain: &[ChainLink],
        now_ms: u64,
    ) -> (String, u64) {
        let exp = now_ms + DOWNLOAD_URL_TTL_MS;
        let path = format!(
            "{URL_PREFIX}/{repo_id}/{id}/{index}/as/{}",
            chain_segment(chain)
        );
        (self.urls.sign(&path, exp), exp)
    }

    /// The largest body an upload URL accepts.
    pub fn max_upload_bytes(&self) -> u64 {
        max_evidence_bytes()
    }

    /// What a signed upload URL (`path_and_query`, or the full URL) names. `PERMISSION_DENIED`
    /// for an unsigned or tampered URL or a download URL, `TOKEN_EXPIRED` once expired.
    pub fn verify_upload(&self, path_and_query: &str) -> Result<UploadTarget> {
        let path = self.urls.verify(path_and_query, self.db.now_ms())?;
        match segments(&path)?.as_slice() {
            [repo, id, index, "upload"] => Ok(UploadTarget {
                repo_id: repo.to_string(),
                evidence_id: id.to_string(),
                index: index_of(index)?,
            }),
            _ => Err(ForgeError::permission_denied(format!(
                "{path} is not an evidence upload URL"
            ))),
        }
    }

    /// What a signed download URL names, with the chain it was issued to.
    /// `PERMISSION_DENIED` for an unsigned or tampered URL or an upload URL, `TOKEN_EXPIRED`
    /// once expired.
    pub fn verify_download(&self, path_and_query: &str) -> Result<DownloadTarget> {
        let path = self.urls.verify(path_and_query, self.db.now_ms())?;
        match segments(&path)?.as_slice() {
            [repo, id, index, "as", who] => {
                let chain = if *who == "-" {
                    Vec::new()
                } else {
                    decode_chain(who).ok_or_else(|| {
                        ForgeError::permission_denied("the URL names no principal chain")
                    })?
                };
                Ok(DownloadTarget {
                    repo_id: repo.to_string(),
                    evidence_id: id.to_string(),
                    index: index_of(index)?,
                    chain,
                })
            }
            _ => Err(ForgeError::permission_denied(format!(
                "{path} is not an evidence download URL"
            ))),
        }
    }

    /// Stores an upload URL's body. The signature is the authorization, so this runs as the
    /// system principal; `TOO_LARGE` above [`Briefs::max_upload_bytes`].
    pub async fn receive_upload(&self, target: &UploadTarget, body: Vec<u8>) -> Result<()> {
        let max = self.max_upload_bytes();
        if body.len() as u64 > max {
            return Err(ForgeError::too_large(
                "evidence_upload_bytes",
                max,
                body.len() as u64,
            ));
        }
        self.receive(
            &Cx::system(),
            &target.repo_id,
            &target.evidence_id,
            target.index,
            body,
        )
        .await
    }

    /// The bytes of a ready item's asset, for a download URL whose chain the caller has
    /// already re-authorized. `NOT_FOUND` when the item is gone or not ready.
    pub async fn download(&self, target: &DownloadTarget) -> Result<Download> {
        let rec = {
            let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
            tx.get_msg::<super::model::EvidenceRec>(&EVIDENCE, &target.repo_id, &target.evidence_id)
                .await?
                .map(|(r, _)| r)
                .filter(|r| r.state() == EvidenceState::Ready)
                .ok_or_else(|| ForgeError::not_found(format!("evidence {}", target.evidence_id)))?
        };
        let asset = rec
            .evidence()
            .assets
            .get(target.index)
            .cloned()
            .ok_or_else(|| {
                ForgeError::not_found(format!(
                    "asset {} of evidence {}",
                    target.index, target.evidence_id
                ))
            })?;
        let bytes = self.asset_bytes(&rec, target.index).await?;
        let disposition = match Kind::from_content_type(&asset.content_type) {
            Some(k) => crate::media::serve::content_disposition(k),
            None => Some("attachment"),
        };
        Ok(Download {
            content_type: asset.content_type,
            disposition,
            digest: asset.digest,
            bytes,
        })
    }
}
