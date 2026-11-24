//! Media uploads shared by the forge areas that accept media: posts (images and video) and
//! change request evidence (`crate::briefs`: images, video, audio, PDF and text).
//!
//! | file | owns |
//! |---|---|
//! | [`sniff`] | kinds by magic bytes (png, jpeg, gif, webp, mp4, webm, mpeg, ogg, wav, pdf), text kinds as declared and only when inert (plain text, Markdown, CSV, WebVTT); never SVG, HTML or JavaScript; image dimensions |
//! | [`upload`] | per-class [`upload::Limit`]s as data (posts: [`upload::LIMITS`]; evidence: [`upload::EVIDENCE_LIMITS`] and the per-change bounds), the pure byte check [`upload::inspect`], the Begin → signed `PUT` → Finalize flow with a server-computed SHA-256 (`INTEGRITY` on mismatch), expiring uploads, and signed download URLs that carry the principal chain |
//! | [`serve`] | the headers every media response carries (`nosniff`, a sandbox CSP), `Content-Disposition` by kind, and `Range` parsing |
//!
//! Posts store each [`upload::MediaRec`] in their own table, so attaching media
//! commits with the rest of its state; this module never opens a transaction. Bytes live in
//! the blob store ([`crate::packages::ports::BlobPort`]) under `<prefix>/<scope>/<id>`, and the
//! server serves the signed URLs under the same paths, deciding repo access again for the
//! URL's principal chain when one is redeemed (SPEC §9.6). Evidence keeps its own records
//! and its bytes in the CI blob store (`crate::ci::ports::CiBlobs`, signed with
//! `crate::ci::ports::UrlSigner`), and reuses the kinds, limits, [`upload::inspect`] and the
//! serving rules here.

pub mod serve;
pub mod sniff;
pub mod upload;

pub use sniff::{Kind, image_size, inert_text, sniff, sniff_as};
pub use upload::{
    ChainLink, EVIDENCE_AUDIO, EVIDENCE_BYTES_PER_CHANGE, EVIDENCE_DOCUMENT, EVIDENCE_IMAGE,
    EVIDENCE_INLINE_BYTES, EVIDENCE_ITEMS_PER_CHANGE, EVIDENCE_LIMITS, EVIDENCE_VIDEO, IMAGE,
    Inspected, LIMITS, Limit, MediaRec, MediaSigner, UploadState, Uploads, VIDEO, evidence_limit,
    inspect, max_evidence_bytes,
};
