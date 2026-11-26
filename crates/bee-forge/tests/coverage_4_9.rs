//! Coverage for F49-016: GPG and SSH signing keys on user accounts, parsed on upload (key
//! ids, subkeys, verified user-id emails, SSH fingerprints), unique across accounts and
//! resolvable to their owner for signature verification. The fixtures were generated with
//! `gpg --quick-gen-key` (ed25519 primary, cv25519 subkey) and `ssh-keygen -t ed25519`; the
//! expected ids are what those tools print.

use crate::community_support as support;

use bee_forge::Cx;
use bee_policy::Action;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use support::*;

const GPG_KEY: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEataxbBYJKwYBBAHaRw8BAQdAK7FQVW1nuKBpSfbguMk42mW+BIqWDy6F+2jI
gfwC0bK0IVdlbmR5IFdyaXRlciA8d3JpdGVyQGV4YW1wbGUuY29tPoivBBMWCgBX
FiEEAAfjU0u+Gi7fIJlD2Ckf7LemkUwFAmrWsWwbFIAAAAAABAAObWFudTIsMi41
KzEuMTIsMCwzAhsDBQsJCAcCAiICBhUKCQgLAgQWAgMBAh4HAheAAAoJENgpH+y3
ppFMO5cA/2kCIDdumsLLNCyKb0D05ykzD9qZBbmQvMsXd2+kynnlAQCxP6m2zBee
yayV1KOBLdpgj264gEncFK9xncvn/hYNArg4BGrWsXISCisGAQQBl1UBBQEBB0DZ
4DS/aE05crGhMLedCfpl5WLY8uEvBo1bP2Pg+Y3WDAMBCAeIlAQYFgoAPBYhBAAH
41NLvhou3yCZQ9gpH+y3ppFMBQJq1rFyGxSAAAAAAAQADm1hbnUyLDIuNSsxLjEy
LDAsMwIbDAAKCRDYKR/st6aRTCdnAP0RaiZ+sxEegI0aCwtBjTYmm6K8B1LUBDmA
kz4w94L8yAD/cJ5olX1RzAfFZTpl2xupgsO9khFhZhBK5pQGn0CpRQg=
=wiOw
-----END PGP PUBLIC KEY BLOCK-----
";
/// `gpg --list-keys --keyid-format long`.
const GPG_ID: &str = "D8291FECB7A6914C";
const GPG_SUB: &str = "B4468200230B7650";

const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKeNcRRA9NoRkyq1+PxtPqGTLOzHONZHYZQ1pOsaQ6IM test";
/// `ssh-keygen -l`.
const SSH_FP: &str = "SHA256:PNMe4PbyneSgUZ9rv48odgKqpWr/wz/BJ2X46qVW6pI";

async fn add(
    w: &World,
    token: &str,
    kind: UserSigningKeyKind,
    key: &str,
) -> Result<UserSigningKey, String> {
    w.s.add_user_signing_key(req(
        token,
        AddUserSigningKeyRequest {
            kind: kind as i32,
            title: "laptop".into(),
            public_key: key.into(),
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|e| {
        bee_proto::errors::detail_from_status(&e)
            .map(|d| d.reason)
            .unwrap_or_default()
    })
}

async fn list(w: &World, who: &str, login: &str, kind: UserSigningKeyKind) -> Vec<UserSigningKey> {
    w.s.list_user_signing_keys(req(
        &w.tok(who),
        ListUserSigningKeysRequest {
            login: login.into(),
            kind: kind as i32,
            page: None,
        },
    ))
    .await
    .unwrap()
    .into_inner()
    .keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gpg_and_ssh_signing_keys_are_parsed_stored_and_resolved() {
    let w = world().await;
    let writer = w.tok("writer");
    let gpg = add(&w, &writer, UserSigningKeyKind::Gpg, GPG_KEY)
        .await
        .unwrap();
    assert_eq!(gpg.kind, UserSigningKeyKind::Gpg as i32);
    assert_eq!(gpg.key_id, GPG_ID);
    assert_eq!(gpg.subkey_ids, vec![GPG_SUB]);
    assert_eq!(gpg.title, "laptop");
    assert!(
        gpg.public_key
            .starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----")
    );
    assert!(gpg.created_at.is_some());
    let ssh = add(&w, &writer, UserSigningKeyKind::Ssh, SSH_KEY)
        .await
        .unwrap();
    assert_eq!(ssh.key_id, SSH_FP);
    assert_eq!(
        ssh.public_key,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKeNcRRA9NoRkyq1+PxtPqGTLOzHONZHYZQ1pOsaQ6IM",
        "the comment is dropped"
    );

    // Unverified on the account: the user id's email is not attached.
    assert!(gpg.emails.is_empty());
    let reader_copy = add(&w, &w.tok("reader"), UserSigningKeyKind::Gpg, GPG_KEY).await;
    assert_eq!(
        reader_copy.unwrap_err(),
        "ALREADY_EXISTS",
        "one owner per key"
    );

    // Anyone may list a user's keys (they are public), by kind or all.
    assert_eq!(
        list(
            &w,
            "outsider",
            "writer-login",
            UserSigningKeyKind::Unspecified
        )
        .await
        .len(),
        2
    );
    let only_ssh = list(&w, "outsider", "writer-login", UserSigningKeyKind::Ssh).await;
    assert_eq!(only_ssh.len(), 1);
    assert_eq!(only_ssh[0].key_id, SSH_FP);
    assert!(
        list(
            &w,
            "writer",
            "reader-login",
            UserSigningKeyKind::Unspecified
        )
        .await
        .is_empty()
    );

    // Verification resolves a signature's key id (primary, subkey or fingerprint) to its owner.
    let keys = &w.env.forge.community.keys;
    for id in [GPG_ID, GPG_SUB, SSH_FP] {
        let (owner, key) = keys.owner_of(&Cx::system(), id).await.unwrap().unwrap();
        assert_eq!(owner, "writer", "{id}");
        assert!(key.key_id == id || key.subkey_ids.iter().any(|s| s == id));
    }
    assert!(
        keys.owner_of(&Cx::system(), "0000000000000000")
            .await
            .unwrap()
            .is_none()
    );

    // Bad input.
    for (kind, text) in [
        (UserSigningKeyKind::Gpg, "not a key"),
        (
            UserSigningKeyKind::Gpg,
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nAAAA\n-----END PGP PUBLIC KEY BLOCK-----\n",
        ),
        (
            UserSigningKeyKind::Ssh,
            "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAABAQ",
        ),
        (UserSigningKeyKind::Ssh, "ssh-dss AAAA"),
        (UserSigningKeyKind::Unspecified, SSH_KEY),
    ] {
        assert_eq!(
            add(&w, &writer, kind, text).await.unwrap_err(),
            "INVALID_ARGUMENT",
            "{text}"
        );
    }
    // A read-only token cannot add keys.
    let ro = w.env.token("writer", actions(&[Action::Read]));
    assert_eq!(
        add(&w, &ro, UserSigningKeyKind::Ssh, SSH_KEY)
            .await
            .unwrap_err(),
        "POLICY_DENIED"
    );

    // Deleting: only one's own keys, by id or key id.
    assert_eq!(
        reason(
            w.s.delete_user_signing_key(req(
                &w.tok("reader"),
                DeleteUserSigningKeyRequest {
                    key_id: gpg.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    w.s.delete_user_signing_key(req(
        &writer,
        DeleteUserSigningKeyRequest {
            key_id: SSH_FP.into(),
        },
    ))
    .await
    .unwrap();
    w.s.delete_user_signing_key(req(&writer, DeleteUserSigningKeyRequest { key_id: gpg.id }))
        .await
        .unwrap();
    assert!(
        list(&w, "writer", "", UserSigningKeyKind::Unspecified)
            .await
            .is_empty()
    );
    assert!(
        keys.owner_of(&Cx::system(), SSH_FP)
            .await
            .unwrap()
            .is_none()
    );
    // With the address verified on the account, the user id's email is attached.
    let users = &w.env.forge.community.users;
    w.s.add_user_email(req(
        &writer,
        AddUserEmailRequest {
            email: "writer@example.com".into(),
        },
    ))
    .await
    .unwrap();
    let tokens = users.claim_verifications(&Cx::system(), 10).await.unwrap();
    users.verify_email(&tokens[0].token).await.unwrap();
    let again = add(&w, &writer, UserSigningKeyKind::Gpg, GPG_KEY)
        .await
        .unwrap();
    assert_eq!(again.emails, vec!["writer@example.com"]);
    // Once removed, another account may claim the key.
    add(&w, &w.tok("reader"), UserSigningKeyKind::Ssh, SSH_KEY)
        .await
        .unwrap();
}
