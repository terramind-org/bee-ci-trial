//! Pending uploads and signed media URLs.
//!
//! An upload is a [`MediaRec`] the owning module stores in its own table (so it commits with
//! that module's other state) and moves through three states:
//!
//! 1. **Begin** ([`Uploads::begin`]): the declared content type must name a kind of the
//!    [`Limit`] and the declared size must fit it. The record is `PENDING` and the client gets a
//!    signed `PUT` URL ([`MediaSigner::upload_url`]) that expires with the upload
//!    ([`UPLOAD_TTL_MS`]).
//! 2. **Receive** ([`Uploads::receive`], the PUT URL's handler): the bytes must arrive before
//!    the upload expires, fit the limit and sniff ([`super::sniff`]) as one of its kinds. The
//!    server computes their SHA-256, reads image dimensions, stores them in the blob store and
//!    marks the record `RECEIVED`.
//! 3. **Finalize** ([`Uploads::finalize`]): the SHA-256 the client declares must equal the one
//!    the server computed and the size must equal the declared one, else `INTEGRITY`. The record
//!    is `FINAL` and may be attached.
//!
//! Downloads go through [`MediaSigner::download_url`]: the URL names the principal chain it
//! was issued to (`as`), bound into its signature, so the server that redeems it decides repo
//! access again for that chain (SPEC §9.6).

use std::sync::Arc;

use bee_proto::v1::{ErrorReason, Principal};
use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::sniff::{Kind, image_size, sniff, sniff_as};
use crate::community::ports::{HmacSigner, SignedUrl};
use crate::error::{ForgeError, Result};
use crate::ids::{hex, unhex};
use crate::packages::ports::BlobPort;

/// How long a pending upload (and its PUT URL) is valid.
pub const UPLOAD_TTL_MS: u64 = 15 * 60 * 1000;
/// How long a signed download URL is valid.
pub const DOWNLOAD_TTL_MS: u64 = 15 * 60 * 1000;

/// What one class of media may be: its kinds and largest size. Modules pick a limit per
/// upload (posts take [`IMAGE`] or [`VIDEO`]); new classes are new entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    /// Class name, as limits and errors name it (`image`, `video`).
    pub name: &'static str,
    /// Largest accepted size in bytes.
    pub max_bytes: u64,
    /// Kinds accepted for the class.
    pub kinds: &'static [Kind],
}

/// Still and animated images.
pub const IMAGE: Limit = Limit {
    name: "image",
    max_bytes: 8 << 20,
    kinds: &[Kind::Png, Kind::Jpeg, Kind::Gif, Kind::Webp],
};

/// Videos. The bound is low because the PUT handler buffers the whole body in memory.
pub const VIDEO: Limit = Limit {
    name: "video",
    max_bytes: 64 << 20,
    kinds: &[Kind::Mp4, Kind::Webm],
};

/// Every post limit, in a fixed order.
pub const LIMITS: &[Limit] = &[IMAGE, VIDEO];

/// Change request evidence images (screenshots, before and after, image sets, posters).
pub const EVIDENCE_IMAGE: Limit = Limit {
    name: "evidence_image",
    max_bytes: 10 << 20,
    kinds: &[Kind::Png, Kind::Jpeg, Kind::Gif, Kind::Webp],
};

/// Change request evidence videos.
pub const EVIDENCE_VIDEO: Limit = Limit {
    name: "evidence_video",
    max_bytes: 100 << 20,
    kinds: &[Kind::Mp4, Kind::Webm],
};

/// Change request evidence audio.
pub const EVIDENCE_AUDIO: Limit = Limit {
    name: "evidence_audio",
    max_bytes: 25 << 20,
    kinds: &[Kind::Mpeg, Kind::Ogg, Kind::Wav],
};

/// Change request evidence documents: reports (PDF, plain text, Markdown, CSV) and WebVTT
/// captions.
pub const EVIDENCE_DOCUMENT: Limit = Limit {
    name: "evidence_document",
    max_bytes: 20 << 20,
    kinds: &[Kind::Pdf, Kind::Text, Kind::Markdown, Kind::Csv, Kind::Vtt],
};

/// Every evidence limit, in a fixed order.
pub const EVIDENCE_LIMITS: &[Limit] = &[
    EVIDENCE_IMAGE,
    EVIDENCE_VIDEO,
    EVIDENCE_AUDIO,
    EVIDENCE_DOCUMENT,
];

/// Most evidence items on one change request.
pub const EVIDENCE_ITEMS_PER_CHANGE: u64 = 64;
/// Most bytes of evidence assets on one change request.
pub const EVIDENCE_BYTES_PER_CHANGE: u64 = 512 << 20;
/// Most bytes of the assets sent inline with one request.
pub const EVIDENCE_INLINE_BYTES: u64 = 8 << 20;

/// The evidence limit whose kinds include `kind`.
pub fn evidence_limit(kind: Kind) -> Option<Limit> {
    EVIDENCE_LIMITS
        .iter()
        .copied()
        .find(|l| l.kinds.contains(&kind))
}

/// The largest body an evidence upload URL accepts (the PUT handler's read bound).
pub fn max_evidence_bytes() -> u64 {
    EVIDENCE_LIMITS
        .iter()
        .map(|l| l.max_bytes)
        .max()
        .unwrap_or(0)
}

/// What [`inspect`] found in a body of bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspected {
    /// The kind the bytes are.
    pub kind: Kind,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Image width (0 for other kinds).
    pub width: u32,
    /// Image height (0 for other kinds).
    pub height: u32,
}

/// Checks `bytes` against `limit`, a pure function of its inputs: `TOO_LARGE` above the
/// limit, `INVALID_ARGUMENT` (`bad_media`) when they are empty, not one of the limit's kinds
/// (as `declared` when given, see [`super::sniff::sniff_as`]; by their signature otherwise),
/// or an image whose header cannot be read. Returns the kind, digest, size and image size.
pub fn inspect(limit: Limit, declared: Option<Kind>, bytes: &[u8]) -> Result<Inspected> {
    let size = bytes.len() as u64;
    if size > limit.max_bytes {
        return Err(ForgeError::too_large(
            &format!("{}_bytes", limit.name),
            limit.max_bytes,
            size,
        ));
    }
    if size == 0 {
        return Err(ForgeError::invalid("bad_media", "the upload is empty"));
    }
    let found = match declared {
        Some(k) => sniff_as(k, bytes),
        None => sniff(bytes),
    };
    let kind = found
        .filter(|k| limit.kinds.contains(k))
        .ok_or_else(|| match declared {
            Some(k) => ForgeError::invalid(
                "bad_media",
                format!("the bytes are not {}", k.content_type()),
            ),
            None => ForgeError::invalid(
                "bad_media",
                format!("the bytes are not an accepted {}", limit.name),
            ),
        })?;
    let (width, height) = if kind.is_image() {
        image_size(kind, bytes)
            .ok_or_else(|| ForgeError::invalid("bad_media", "the image header cannot be read"))?
    } else {
        (0, 0)
    };
    Ok(Inspected {
        kind,
        sha256: hex(&Sha256::digest(bytes)),
        size,
        width,
        height,
    })
}

/// The largest body any limit accepts (the PUT handler's read bound).
pub fn max_upload_bytes() -> u64 {
    LIMITS.iter().map(|l| l.max_bytes).max().unwrap_or(0)
}

/// State of an upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum UploadState {
    /// Begun; no bytes yet.
    Pending = 0,
    /// Bytes stored, digest computed; waiting for finalize.
    Received = 1,
    /// Finalized: the client's digest matched.
    Final = 2,
}

/// A stored upload. Field numbers follow the proto rule: never renumbered or retyped.
#[derive(Clone, PartialEq, prost::Message)]
pub struct MediaRec {
    /// Media id (a ULID).
    #[prost(string, tag = "1")]
    pub id: String,
    /// Scope of the owning row (a repo id).
    #[prost(string, tag = "2")]
    pub scope: String,
    /// What the media was uploaded for (a change request id).
    #[prost(string, tag = "3")]
    pub owner: String,
    /// The limit's class name.
    #[prost(string, tag = "4")]
    pub class: String,
    /// Declared size.
    #[prost(uint64, tag = "5")]
    pub declared_size: u64,
    /// Declared content type.
    #[prost(string, tag = "6")]
    pub declared_type: String,
    /// [`UploadState`].
    #[prost(enumeration = "UploadState", tag = "7")]
    pub state: i32,
    /// Blob key.
    #[prost(string, tag = "8")]
    pub key: String,
    /// Content type from sniffing (what is served).
    #[prost(string, tag = "9")]
    pub content_type: String,
    /// Received size.
    #[prost(uint64, tag = "10")]
    pub size: u64,
    /// Lower-case hex SHA-256 of the received bytes, computed by the server.
    #[prost(string, tag = "11")]
    pub sha256: String,
    /// Image width (read from the header) or video width (declared).
    #[prost(uint32, tag = "12")]
    pub width: u32,
    /// Image height (read from the header) or video height (declared).
    #[prost(uint32, tag = "13")]
    pub height: u32,
    /// Video duration (declared).
    #[prost(uint32, tag = "14")]
    pub duration_ms: u32,
    /// Alt text.
    #[prost(string, tag = "15")]
    pub alt_text: String,
    /// Who began the upload.
    #[prost(message, optional, tag = "16")]
    pub uploader: Option<Principal>,
    /// When the upload began (ms).
    #[prost(uint64, tag = "17")]
    pub created_ms: u64,
    /// When the pending upload expires (ms).
    #[prost(uint64, tag = "18")]
    pub expires_ms: u64,
    /// What the media is attached to (a post id); empty while unattached.
    #[prost(string, tag = "19")]
    pub attached: String,
}

impl MediaRec {
    /// The upload state.
    pub fn upload_state(&self) -> UploadState {
        UploadState::try_from(self.state).unwrap_or(UploadState::Pending)
    }

    /// The sniffed kind (once received).
    pub fn kind(&self) -> Option<Kind> {
        Kind::from_content_type(&self.content_type)
    }

    /// The limit it was begun under.
    pub fn limit(&self) -> Option<Limit> {
        LIMITS.iter().copied().find(|l| l.name == self.class)
    }
}

/// Signs media URLs: `PUT` for uploads, `GET` bound to a principal chain for downloads.
#[derive(Clone, Debug)]
pub struct MediaSigner {
    base: String,
    hmac: HmacSigner,
}

/// One principal of a download URL's chain: id and kind name (`human`, `agent`, `service`).
pub type ChainLink = (String, String);

/// The hex text of a principal chain, as download URLs carry it (`as=`; evidence URLs carry it
/// in the path).
pub fn encode_chain(chain: &[ChainLink]) -> String {
    let joined: Vec<String> = chain.iter().map(|(id, k)| format!("{id}:{k}")).collect();
    hex(joined.join(",").as_bytes())
}

/// The chain [`encode_chain`] wrote; `None` when `s` is not one.
pub fn decode_chain(s: &str) -> Option<Vec<ChainLink>> {
    let text = String::from_utf8(unhex(s)?).ok()?;
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(',')
        .map(|p| {
            let (id, k) = p.split_once(':')?;
            (!id.is_empty()).then(|| (id.to_string(), k.to_string()))
        })
        .collect()
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

impl MediaSigner {
    /// A signer for URLs under `base` (no trailing slash) with `secret`.
    pub fn new(base: &str, secret: &[u8]) -> MediaSigner {
        MediaSigner {
            base: base.trim_end_matches('/').to_string(),
            hmac: HmacSigner::new(base, secret),
        }
    }

    /// A signer with a random secret (tests and detached forges).
    pub fn random(base: &str) -> MediaSigner {
        let mut secret = [0u8; 32];
        let _ = getrandom::fill(&mut secret);
        MediaSigner::new(base, &secret)
    }

    /// A URL to `PUT` the bytes of `key` until `expires_ms`.
    pub fn upload_url(&self, key: &str, expires_ms: u64) -> SignedUrl {
        let sig = self.hmac.signature("PUT", key, expires_ms);
        SignedUrl {
            url: format!("{}/{key}?expires={expires_ms}&signature={sig}", self.base),
            headers: Vec::new(),
            expires_ms,
        }
    }

    /// A URL to `GET` `key` for `chain` (empty for a signed-out reader), valid for
    /// [`DOWNLOAD_TTL_MS`].
    pub fn download_url(&self, key: &str, chain: &[ChainLink], now_ms: u64) -> SignedUrl {
        let exp = now_ms + DOWNLOAD_TTL_MS;
        let who = encode_chain(chain);
        let sig = self.hmac.signature("GET", &format!("{key}\n{who}"), exp);
        SignedUrl {
            url: format!("{}/{key}?expires={exp}&as={who}&signature={sig}", self.base),
            headers: Vec::new(),
            expires_ms: exp,
        }
    }

    fn key_of<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
        let key = path.trim_start_matches('/');
        (key.starts_with(prefix) && !key.contains("..")).then_some(key)
    }

    /// The key of a valid, unexpired upload URL (`path` and `query` of the request) under
    /// `prefix`.
    pub fn verify_upload(
        &self,
        prefix: &str,
        path: &str,
        query: &str,
        now_ms: u64,
    ) -> Option<String> {
        let key = Self::key_of(path, prefix)?;
        let exp = query_param(query, "expires")?.parse::<u64>().ok()?;
        let sig = query_param(query, "signature")?;
        self.hmac
            .verify("PUT", key, exp, sig, now_ms)
            .then(|| key.to_string())
    }

    /// The key and principal chain of a valid, unexpired download URL under `prefix`. The
    /// caller still decides repo access for the chain.
    pub fn verify_download(
        &self,
        prefix: &str,
        path: &str,
        query: &str,
        now_ms: u64,
    ) -> Option<(String, Vec<ChainLink>)> {
        let key = Self::key_of(path, prefix)?;
        let exp = query_param(query, "expires")?.parse::<u64>().ok()?;
        let who = query_param(query, "as")?;
        let sig = query_param(query, "signature")?;
        if !self
            .hmac
            .verify("GET", &format!("{key}\n{who}"), exp, sig, now_ms)
        {
            return None;
        }
        Some((key.to_string(), decode_chain(who)?))
    }
}

/// The upload flow over a blob store and a signer.
#[derive(Clone)]
pub struct Uploads {
    blobs: Arc<dyn BlobPort>,
    signer: MediaSigner,
}

impl Uploads {
    /// Uploads into `blobs`, with URLs from `signer`.
    pub fn new(blobs: Arc<dyn BlobPort>, signer: MediaSigner) -> Uploads {
        Uploads { blobs, signer }
    }

    /// The signer.
    pub fn signer(&self) -> &MediaSigner {
        &self.signer
    }

    /// Starts an upload of `size` bytes of `content_type` under `limit`; the blob key is
    /// `<prefix>/<scope>/<id>`. Returns the pending record and its `PUT` URL.
    #[allow(clippy::too_many_arguments)]
    pub fn begin(
        &self,
        prefix: &str,
        id: &str,
        scope: &str,
        owner: &str,
        limit: Limit,
        content_type: &str,
        size: u64,
        uploader: Principal,
        now_ms: u64,
    ) -> Result<(MediaRec, SignedUrl)> {
        let kind = Kind::from_content_type(content_type)
            .filter(|k| limit.kinds.contains(k))
            .ok_or_else(|| {
                ForgeError::invalid(
                    "bad_media",
                    format!(
                        "{content_type:?} is not an accepted {} type ({})",
                        limit.name,
                        limit
                            .kinds
                            .iter()
                            .map(|k| k.content_type())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
        if size == 0 {
            return Err(ForgeError::invalid("bad_media", "the upload is empty"));
        }
        if size > limit.max_bytes {
            return Err(ForgeError::too_large(
                &format!("{}_bytes", limit.name),
                limit.max_bytes,
                size,
            ));
        }
        let key = format!("{prefix}/{scope}/{id}");
        let expires_ms = now_ms + UPLOAD_TTL_MS;
        let url = self.signer.upload_url(&key, expires_ms);
        Ok((
            MediaRec {
                id: id.to_string(),
                scope: scope.to_string(),
                owner: owner.to_string(),
                class: limit.name.to_string(),
                declared_size: size,
                declared_type: kind.content_type().to_string(),
                state: UploadState::Pending as i32,
                key,
                uploader: Some(uploader),
                created_ms: now_ms,
                expires_ms,
                ..MediaRec::default()
            },
            url,
        ))
    }

    /// Checks and stores the uploaded bytes of `rec` (any earlier bytes are replaced):
    /// `INVALID_STATE` once finalized or expired, `TOO_LARGE` above the limit,
    /// `INVALID_ARGUMENT` (`bad_media`) when the bytes are not one of the limit's kinds or an
    /// image header is unreadable.
    pub async fn receive(&self, rec: &mut MediaRec, bytes: Bytes, now_ms: u64) -> Result<()> {
        if rec.upload_state() == UploadState::Final {
            return Err(ForgeError::invalid_state("the upload is already finalized"));
        }
        if now_ms >= rec.expires_ms {
            return Err(
                ForgeError::invalid_state("the upload expired").with_subcode("upload_expired")
            );
        }
        let limit = rec
            .limit()
            .ok_or_else(|| ForgeError::internal(format!("unknown media class {}", rec.class)))?;
        let found = inspect(limit, None, &bytes)?;
        rec.sha256 = found.sha256;
        rec.size = found.size;
        rec.content_type = found.kind.content_type().to_string();
        rec.width = found.width;
        rec.height = found.height;
        self.blobs.put(&rec.key, bytes).await?;
        rec.state = UploadState::Received as i32;
        Ok(())
    }

    /// Finalizes `rec` against the client's `sha256` (lower- or upper-case hex): `INTEGRITY`
    /// when it or the size differs from what was received, `INVALID_STATE` before any bytes
    /// arrived or after the upload expired. Finalizing again with the same digest is a no-op.
    pub fn finalize(&self, rec: &mut MediaRec, sha256: &str, now_ms: u64) -> Result<()> {
        let declared = sha256.trim().to_ascii_lowercase();
        if declared.len() != 64 || unhex(&declared).is_none() {
            return Err(ForgeError::invalid(
                "bad_digest",
                "sha256 must be 64 hex characters",
            ));
        }
        match rec.upload_state() {
            UploadState::Pending => {
                return Err(ForgeError::invalid_state("no bytes were uploaded yet")
                    .with_subcode("upload_pending"));
            }
            UploadState::Final if rec.sha256 == declared => return Ok(()),
            UploadState::Final => {
                return Err(ForgeError::invalid_state("the upload is already finalized"));
            }
            UploadState::Received => {}
        }
        if now_ms >= rec.expires_ms {
            return Err(
                ForgeError::invalid_state("the upload expired").with_subcode("upload_expired")
            );
        }
        if rec.sha256 != declared || rec.size != rec.declared_size {
            return Err(ForgeError::new(
                ErrorReason::Integrity,
                format!(
                    "received {} bytes with sha256 {}, declared {} bytes with sha256 {declared}",
                    rec.size, rec.sha256, rec.declared_size
                ),
            ));
        }
        rec.state = UploadState::Final as i32;
        Ok(())
    }

    /// A download URL of finalized media for `chain`.
    pub fn download_url(&self, rec: &MediaRec, chain: &[ChainLink], now_ms: u64) -> String {
        self.signer.download_url(&rec.key, chain, now_ms).url
    }

    /// The stored bytes.
    pub async fn bytes(&self, rec: &MediaRec) -> Result<Option<Bytes>> {
        self.blobs.get(&rec.key).await
    }

    /// Deletes the stored bytes (absent is fine).
    pub async fn discard(&self, rec: &MediaRec) -> Result<()> {
        self.blobs.delete(&rec.key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::sniff::tests::png;
    use crate::packages::ports::MemoryBlobs;

    fn uploads() -> Uploads {
        Uploads::new(
            Arc::new(MemoryBlobs::new()),
            MediaSigner::new("https://h.example", &[7u8; 32]),
        )
    }

    fn begin(u: &Uploads, limit: Limit, ct: &str, size: u64) -> Result<(MediaRec, SignedUrl)> {
        u.begin(
            "posts",
            "m1",
            "repo1",
            "cr1",
            limit,
            ct,
            size,
            Principal::default(),
            1_000,
        )
    }

    #[test]
    fn limits_are_data() {
        assert_eq!(IMAGE.max_bytes, 8 * 1024 * 1024);
        assert_eq!(VIDEO.max_bytes, 64 * 1024 * 1024);
        assert_eq!(max_upload_bytes(), VIDEO.max_bytes);
        assert!(LIMITS.iter().all(|l| !l.kinds.is_empty()));
    }

    #[test]
    fn evidence_limits_are_data() {
        assert_eq!(EVIDENCE_IMAGE.max_bytes, 10 * 1024 * 1024);
        assert_eq!(EVIDENCE_VIDEO.max_bytes, 100 * 1024 * 1024);
        assert_eq!(EVIDENCE_AUDIO.max_bytes, 25 * 1024 * 1024);
        assert_eq!(EVIDENCE_DOCUMENT.max_bytes, 20 * 1024 * 1024);
        assert_eq!(EVIDENCE_ITEMS_PER_CHANGE, 64);
        assert_eq!(EVIDENCE_BYTES_PER_CHANGE, 512 * 1024 * 1024);
        assert_eq!(max_evidence_bytes(), EVIDENCE_VIDEO.max_bytes);
        // Every accepted kind has exactly one evidence class; posts keep their own limits.
        for k in crate::media::sniff::ALL_KINDS {
            let n = EVIDENCE_LIMITS
                .iter()
                .filter(|l| l.kinds.contains(k))
                .count();
            assert_eq!(n, 1, "{k:?}");
            assert!(evidence_limit(*k).is_some());
        }
        assert_eq!(max_upload_bytes(), VIDEO.max_bytes);
    }

    #[test]
    fn inspect_checks_declared_kind_size_and_reads_images() {
        let bytes = png(5, 7);
        let got = inspect(EVIDENCE_IMAGE, Some(Kind::Png), &bytes).unwrap();
        assert_eq!((got.kind, got.width, got.height), (Kind::Png, 5, 7));
        assert_eq!(got.sha256, hex(&Sha256::digest(&bytes)));
        assert_eq!(
            inspect(EVIDENCE_IMAGE, Some(Kind::Jpeg), &bytes)
                .unwrap_err()
                .subcode,
            "bad_media"
        );
        assert_eq!(
            inspect(EVIDENCE_DOCUMENT, Some(Kind::Text), b"<svg/>")
                .unwrap_err()
                .subcode,
            "bad_media"
        );
        assert_eq!(
            inspect(EVIDENCE_DOCUMENT, Some(Kind::Csv), b"a,b\n1,2\n")
                .unwrap()
                .kind,
            Kind::Csv
        );
        let big = vec![b'a'; (EVIDENCE_DOCUMENT.max_bytes + 1) as usize];
        assert_eq!(
            inspect(EVIDENCE_DOCUMENT, Some(Kind::Text), &big)
                .unwrap_err()
                .reason,
            ErrorReason::TooLarge
        );
        assert_eq!(
            inspect(EVIDENCE_IMAGE, None, b"").unwrap_err().subcode,
            "bad_media"
        );
    }

    #[test]
    fn begin_checks_type_and_size() {
        let u = uploads();
        let (rec, url) = begin(&u, IMAGE, "image/png", 100).unwrap();
        assert_eq!(rec.key, "posts/repo1/m1");
        assert_eq!(rec.expires_ms, 1_000 + UPLOAD_TTL_MS);
        assert!(
            url.url
                .starts_with("https://h.example/posts/repo1/m1?expires=")
        );
        let e = begin(&u, IMAGE, "image/svg+xml", 100).unwrap_err();
        assert_eq!(e.subcode, "bad_media");
        let e = begin(&u, IMAGE, "video/mp4", 100).unwrap_err();
        assert_eq!(e.subcode, "bad_media");
        let e = begin(&u, IMAGE, "image/png", IMAGE.max_bytes + 1).unwrap_err();
        assert_eq!(e.reason, ErrorReason::TooLarge);
        assert!(begin(&u, VIDEO, "video/mp4", IMAGE.max_bytes + 1).is_ok());
        assert_eq!(
            begin(&u, IMAGE, "image/png", 0).unwrap_err().subcode,
            "bad_media"
        );
    }

    #[tokio::test]
    async fn receive_sniffs_and_finalize_compares_the_server_digest() {
        let u = uploads();
        let bytes = png(4, 3);
        let (mut rec, _) = begin(&u, IMAGE, "image/png", bytes.len() as u64).unwrap();
        // HTML declared as PNG is refused by its bytes.
        let e = u
            .receive(&mut rec, Bytes::from_static(b"<html><script>"), 2_000)
            .await
            .unwrap_err();
        assert_eq!(e.subcode, "bad_media");
        assert!(u.finalize(&mut rec, &"0".repeat(64), 2_000).is_err());
        u.receive(&mut rec, Bytes::from(bytes.clone()), 2_000)
            .await
            .unwrap();
        assert_eq!((rec.width, rec.height), (4, 3));
        assert_eq!(rec.content_type, "image/png");
        let good = hex(&Sha256::digest(&bytes));
        let e = u.finalize(&mut rec, &"a".repeat(64), 2_000).unwrap_err();
        assert_eq!(e.reason, ErrorReason::Integrity);
        u.finalize(&mut rec, &good.to_uppercase(), 2_000).unwrap();
        assert_eq!(rec.upload_state(), UploadState::Final);
        u.finalize(&mut rec, &good, 2_000).unwrap();
        assert_eq!(u.bytes(&rec).await.unwrap().unwrap().as_ref(), &bytes[..]);
        assert!(
            u.receive(&mut rec, Bytes::from(bytes), 2_000)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn uploads_expire_and_sizes_must_match() {
        let u = uploads();
        let bytes = png(1, 1);
        let (mut rec, _) = begin(&u, IMAGE, "image/png", bytes.len() as u64 + 5).unwrap();
        let late = 1_000 + UPLOAD_TTL_MS;
        let e = u
            .receive(&mut rec, Bytes::from(bytes.clone()), late)
            .await
            .unwrap_err();
        assert_eq!(e.subcode, "upload_expired");
        u.receive(&mut rec, Bytes::from(bytes.clone()), 5_000)
            .await
            .unwrap();
        let good = hex(&Sha256::digest(&bytes));
        // The digest matches, but fewer bytes arrived than were declared.
        assert_eq!(
            u.finalize(&mut rec, &good, 5_000).unwrap_err().reason,
            ErrorReason::Integrity
        );
        assert_eq!(
            u.finalize(&mut rec, &good, late).unwrap_err().subcode,
            "upload_expired"
        );
    }

    #[test]
    fn urls_verify_only_as_signed() {
        let s = MediaSigner::new("https://h.example/", &[1u8; 32]);
        let up = s.upload_url("posts/r/m", 10_000);
        let (path, query) = up.url["https://h.example".len()..].split_once('?').unwrap();
        assert_eq!(
            s.verify_upload("posts/", path, query, 9_999).as_deref(),
            Some("posts/r/m")
        );
        assert!(s.verify_upload("posts/", path, query, 10_000).is_none());
        assert!(s.verify_upload("releases/", path, query, 1).is_none());
        // An upload signature is no download signature.
        assert!(s.verify_download("posts/", path, query, 1).is_none());
        let chain = vec![
            ("agent1".to_string(), "agent".to_string()),
            ("owner".to_string(), "human".to_string()),
        ];
        let down = s.download_url("posts/r/m", &chain, 0);
        let (path, query) = down.url["https://h.example".len()..]
            .split_once('?')
            .unwrap();
        let (key, got) = s.verify_download("posts/", path, query, 1).unwrap();
        assert_eq!((key.as_str(), got), ("posts/r/m", chain));
        // Swapping the chain breaks the signature.
        let forged = query.replace(
            &encode_chain(&[
                ("agent1".to_string(), "agent".to_string()),
                ("owner".to_string(), "human".to_string()),
            ]),
            &encode_chain(&[("admin".to_string(), "human".to_string())]),
        );
        assert!(s.verify_download("posts/", path, &forged, 1).is_none());
        let anon = s.download_url("posts/r/m", &[], 0);
        let (path, query) = anon.url["https://h.example".len()..]
            .split_once('?')
            .unwrap();
        assert_eq!(
            s.verify_download("posts/", path, query, 1).unwrap().1,
            Vec::<ChainLink>::new()
        );
        assert!(
            s.verify_download("posts/", path, query, DOWNLOAD_TTL_MS)
                .is_none()
        );
    }
}
