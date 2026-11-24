//! Signature verification of changesets (SPEC §9.4 "signed changesets", PRODUCT §4.9).
//!
//! A changeset is signed when its Meta carries a git commit (the `git` extension written by
//! imports and git pushes) whose headers hold a `gpgsig` signature. Its git commit object is
//! rebuilt exactly as the gateway serves it ([`bee_git_http::gitmap::GitView`]), split into
//! signature and payload ([`bee_git::GitCommit::signature`]) and checked against the
//! forge's signing-key registry ([`bee_forge::community::keys::SigningKeys`]). Without a
//! forge there is no registry, so every signature is `UNKNOWN_KEY`.
//!
//! Changesets written natively (CreateChangeset, rebases and squashes at land) carry no git
//! signature; they are `UNSIGNED`.

use bee_core::ObjectId;
use bee_forge::community::keys::SignatureVerdict;
use bee_git::{GitCommit, GitOid};
use bee_git_http::gitmap::{GitView, detect_format};
use bee_policy::SignatureState;
use bee_proto::v1::{self, SecuritySignatureReason as R, SecuritySignatureScheme as S};

use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RepoCtx};

/// The verification of one changeset.
#[derive(Clone, Debug)]
pub struct ChangesetSignature {
    /// The changeset.
    pub changeset: ObjectId,
    /// Its git commit id, when it has a git commit with a signature.
    pub git_oid: Option<GitOid>,
    /// The verdict (`UNSIGNED` when there is no signature).
    pub verdict: SignatureVerdict,
}

impl ChangesetSignature {
    /// The land-policy state: verified, signed but not verified, or missing.
    pub fn state(&self) -> SignatureState {
        match self.verdict.reason {
            R::Valid => SignatureState::Verified,
            R::Unsigned | R::Unspecified => SignatureState::Missing,
            _ => SignatureState::Unverified,
        }
    }

    /// The policy wire form of the verdict.
    pub fn policy_state(&self) -> v1::PolicySignatureState {
        use v1::PolicySignatureState as P;
        match self.verdict.reason {
            R::Valid => P::Verified,
            R::Unsigned | R::Unspecified => P::Unsigned,
            R::UnknownKey => P::UnknownKey,
            R::BadSignature | R::Malformed | R::SignerMismatch => P::BadSignature,
            R::ExpiredKey => P::ExpiredKey,
            R::RevokedKey => P::RevokedKey,
            R::UnverifiedEmail => P::UnverifiedEmail,
        }
    }

    /// The format name (`gpg`, `ssh`, or empty when unsigned).
    pub fn format(&self) -> &'static str {
        match self.verdict.scheme {
            S::Gpg => "gpg",
            S::Ssh => "ssh",
            S::Smime => "x509",
            S::Unspecified => "",
        }
    }
}

fn unsigned(id: &ObjectId) -> ChangesetSignature {
    ChangesetSignature {
        changeset: *id,
        git_oid: None,
        verdict: SignatureVerdict {
            reason: R::Unsigned,
            scheme: S::Unspecified,
            key_id: String::new(),
            signer: None,
            signature: String::new(),
            detail: String::new(),
        },
    }
}

fn has_signature_header(ext: &[u8]) -> bool {
    let (headers, _) = match ext.windows(2).position(|w| w == b"\n\n") {
        Some(i) => ext.split_at(i + 1),
        None => (ext, &[][..]),
    };
    headers
        .split(|&b| b == b'\n')
        .any(|l| l.starts_with(b"gpgsig ") || l.starts_with(b"gpgsig-sha256 "))
}

fn git_err(e: bee_git_http::GitError) -> ApiError {
    let reason =
        bee_proto::errors::parse_reason(&e.reason).unwrap_or(bee_proto::v1::ErrorReason::Internal);
    ApiError::new(reason, e.message)
}

/// Verifies the signature of changeset `id` in `repo`.
pub async fn verify_changeset(
    state: &AppState,
    repo: &RepoCtx,
    id: &ObjectId,
) -> ApiResult<ChangesetSignature> {
    let history = state.history(repo);
    let cs = history.changeset(id).await?;
    let Some(meta) = history.meta_object(cs.meta()).await? else {
        return Ok(unsigned(id));
    };
    match &meta.git {
        Some(ext) if has_signature_header(ext) => {}
        _ => return Ok(unsigned(id)),
    }
    let format = detect_format(&history, &[*id]).await.map_err(git_err)?;
    let mut view = GitView::new(&history, format);
    let (oid, raw) = view.commit_object(id).await.map_err(git_err)?;
    let commit = GitCommit::parse(&raw.data, format)?;
    let Some(sig) = commit.signature() else {
        // The rebuilt commit is new history (the imported bytes no longer match), which
        // carries no signature.
        return Ok(unsigned(id));
    };
    let verdict = match state
        .forge
        .verify_signature(&sig.payload, &sig.signature)
        .await?
    {
        Some(v) => v,
        None => {
            let parsed = bee_forge::community::sigverify::parse(&sig.signature);
            SignatureVerdict {
                reason: if parsed.is_ok() {
                    R::UnknownKey
                } else {
                    R::Malformed
                },
                scheme: match parsed.as_ref().map(|p| p.scheme) {
                    Ok(bee_forge::community::sigverify::Scheme::Gpg) => S::Gpg,
                    Ok(bee_forge::community::sigverify::Scheme::Ssh) => S::Ssh,
                    Err(_) => S::Unspecified,
                },
                key_id: parsed.map(|p| p.issuer).unwrap_or_default(),
                signer: None,
                signature: String::from_utf8_lossy(&sig.signature).into_owned(),
                detail: "this server has no signing-key registry (the forge is off)".into(),
            }
        }
    };
    Ok(ChangesetSignature {
        changeset: *id,
        git_oid: Some(oid),
        verdict,
    })
}

/// Land-policy signature states of `ids`, in order.
pub async fn states(
    state: &AppState,
    repo: &RepoCtx,
    ids: &[ObjectId],
) -> ApiResult<Vec<SignatureState>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(verify_changeset(state, repo, id).await?.state());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_headers_are_found_only_among_headers() {
        assert!(has_signature_header(b"author a\ngpgsig -----BEGIN\n x\n\n"));
        assert!(has_signature_header(b"gpgsig-sha256 x\n"));
        assert!(!has_signature_header(
            b"author a\n\ngpgsig in the message\n"
        ));
        assert!(!has_signature_header(b"author a\n"));
    }
}
