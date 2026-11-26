//! Coverage for F49-004 (verification half): commit signatures written by real `git` with
//! `gpg` (Ed25519, RSA-3072, NIST P-256, and an Ed25519 signing subkey under a
//! certification-only primary) and with `ssh-keygen -Y sign` (Ed25519, ECDSA P-256, RSA)
//! verify against the keys their owners registered, name the owner as the signer, and fail
//! as `BAD_SIGNATURE`, `UNKNOWN_KEY` or `MALFORMED` when tampered with, unregistered or
//! garbled. The fixtures under `fixtures/signatures` are `git cat-file commit` output and the
//! matching public keys (`gpg --armor --export`, `*.pub`).

use crate::community_support as support;

use bee_git::{GitCommit, ObjectFormat};
use bee_proto::v1::{
    SecuritySignatureReason as R, SecuritySignatureScheme as S, UserSigningKeyKind,
};
use support::*;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/signatures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn split(name: &str) -> (Vec<u8>, Vec<u8>) {
    let c = GitCommit::parse(fixture(name).as_bytes(), ObjectFormat::Sha1).unwrap();
    let s = c.signature().expect("signed fixture");
    (s.payload, s.signature)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_written_gpg_and_ssh_signatures_verify_against_registered_keys() {
    let w = world().await;
    let keys = &w.env.forge.community.keys;
    // (commit fixture, key fixture, kind, owner, key id the signature names)
    let cases = [
        (
            "commit_gpg_alice.txt",
            "gpg_alice.asc",
            UserSigningKeyKind::Gpg,
            "writer",
            "7113972A98D23852",
        ),
        (
            "commit_gpg_bob.txt",
            "gpg_bob.asc",
            UserSigningKeyKind::Gpg,
            "reader",
            "39D22BB6D1DB1C71",
        ),
        (
            "commit_gpg_carol.txt",
            "gpg_carol.asc",
            UserSigningKeyKind::Gpg,
            "triager",
            "031DA6F82DA77223",
        ),
        // Signed by the subkey; the registered key's primary id is 3C3D82536E3A6DC5.
        (
            "commit_gpg_dave.txt",
            "gpg_dave.asc",
            UserSigningKeyKind::Gpg,
            "admin",
            "569BD29C032A3A91",
        ),
        (
            "commit_ssh_ed25519.txt",
            "ssh_ed25519.pub",
            UserSigningKeyKind::Ssh,
            "writer",
            "",
        ),
        (
            "commit_ssh_ecdsa.txt",
            "ssh_ecdsa.pub",
            UserSigningKeyKind::Ssh,
            "reader",
            "",
        ),
        (
            "commit_ssh_rsa.txt",
            "ssh_rsa.pub",
            UserSigningKeyKind::Ssh,
            "agent",
            "",
        ),
    ];

    // Before any key is registered every signature is well formed but unknown.
    for (commit, _, _, _, _) in &cases {
        let (payload, sig) = split(commit);
        let v = keys
            .verify_signature(&w.cx("ci"), &payload, &sig, 0)
            .await
            .unwrap();
        assert_eq!(v.reason, R::UnknownKey, "{commit}");
        assert!(v.signer.is_none());
    }

    for (commit, key, kind, owner, key_id) in &cases {
        let added = keys
            .add(&w.cx(owner), *kind, "laptop", &fixture(key), &[])
            .await
            .unwrap();
        let (payload, sig) = split(commit);
        let v = keys
            .verify_signature(&w.cx("ci"), &payload, &sig, 0)
            .await
            .unwrap();
        assert_eq!(v.reason, R::Valid, "{commit}: {}", v.detail);
        assert!(v.verified());
        assert_eq!(v.signer.as_deref(), Some(*owner), "{commit}");
        let expect_id = if key_id.is_empty() {
            added.key_id.as_str()
        } else {
            key_id
        };
        assert_eq!(v.key_id, expect_id, "{commit}");
        assert_eq!(
            v.scheme,
            if *kind == UserSigningKeyKind::Gpg {
                S::Gpg
            } else {
                S::Ssh
            }
        );
        assert!(v.signature.contains("-----BEGIN"));

        // One changed byte of the signed commit breaks it.
        let mut tampered = payload.clone();
        let at = tampered.len() - 2;
        tampered[at] ^= 1;
        let v = keys
            .verify_signature(&w.cx("ci"), &tampered, &sig, 0)
            .await
            .unwrap();
        assert_eq!(v.reason, R::BadSignature, "{commit}");
        assert!(v.signer.is_none());
    }

    // A signature over one commit does not verify another.
    let (other, _) = split("commit_ssh_rsa.txt");
    let (_, ed_sig) = split("commit_ssh_ed25519.txt");
    let v = keys
        .verify_signature(&w.cx("ci"), &other, &ed_sig, 0)
        .await
        .unwrap();
    assert_eq!(v.reason, R::BadSignature);

    // Garbage is malformed, not an error.
    for junk in [
        &b"not a signature"[..],
        b"-----BEGIN PGP SIGNATURE-----\n\nAAAA\n-----END PGP SIGNATURE-----\n",
    ] {
        let v = keys
            .verify_signature(&w.cx("ci"), b"x", junk, 0)
            .await
            .unwrap();
        assert_eq!(v.reason, R::Malformed);
    }

    // Removing the key makes its signatures unknown again.
    keys.delete(&w.cx("writer"), "7113972A98D23852")
        .await
        .unwrap();
    let (payload, sig) = split("commit_gpg_alice.txt");
    let v = keys
        .verify_signature(&w.cx("ci"), &payload, &sig, 0)
        .await
        .unwrap();
    assert_eq!(v.reason, R::UnknownKey);
}
