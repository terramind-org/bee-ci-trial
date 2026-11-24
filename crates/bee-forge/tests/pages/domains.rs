//! Custom domains, DNS verification, health checks and certificates.

use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::community::Ports;
use bee_forge::forge::Hooks;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::pages_service_server::PagesService;
use bee_proto::v1::repo_service_server::RepoService;

use super::deploy::{create_site, get};
use super::support::*;

fn set_domain(
    w: &World,
    tok: &str,
    repo: &str,
    domain: &str,
) -> impl std::future::Future<Output = Result<tonic::Response<PagesSite>, tonic::Status>> {
    let s = w.s.clone();
    let t = tok.to_string();
    let (r, d) = (repo.to_string(), domain.to_string());
    async move {
        s.set_pages_custom_domain(req(
            &t,
            SetPagesCustomDomainRequest {
                repo: rref(&r),
                domain: d,
            },
        ))
        .await
    }
}

#[tokio::test]
async fn org_domain_verification_with_dns_txt() {
    let w = world().await;
    let o = w.tok("owner");
    // Only org owners and admins manage verifications.
    assert_eq!(
        reason(
            w.s.add_pages_verified_domain(req(
                &w.tok("writer"),
                AddPagesVerifiedDomainRequest {
                    org: "acme".into(),
                    domain: "example.com".into()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            w.s.add_pages_verified_domain(req(
                &o,
                AddPagesVerifiedDomainRequest {
                    org: "acme".into(),
                    domain: "nodots".into()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let v =
        w.s.add_pages_verified_domain(req(
            &o,
            AddPagesVerifiedDomainRequest {
                org: "acme".into(),
                domain: "Example.COM".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(v.domain, "example.com");
    assert_eq!(v.state, PagesDomainVerificationState::Pending as i32);
    assert_eq!(
        v.txt_record_name,
        "_beehive-pages-challenge-acme.example.com"
    );
    assert!(v.txt_record_value.starts_with("beehive-pages-challenge="));
    // Adding again is idempotent.
    let again =
        w.s.add_pages_verified_domain(req(
            &o,
            AddPagesVerifiedDomainRequest {
                org: "acme".into(),
                domain: "example.com".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.id, v.id);
    // The TXT record is not there: FAILED, not an error.
    let verify = || {
        w.s.verify_pages_domain(req(
            &o,
            VerifyPagesDomainRequest {
                org: "acme".into(),
                verification_id: v.id.clone(),
            },
        ))
    };
    assert_eq!(
        verify().await.unwrap().into_inner().state,
        PagesDomainVerificationState::Failed as i32
    );
    // A wrong value still fails; the right one verifies.
    w.dns.add_txt(&v.txt_record_name, "something else");
    assert_eq!(
        verify().await.unwrap().into_inner().state,
        PagesDomainVerificationState::Failed as i32
    );
    w.dns.add_txt(&v.txt_record_name, &v.txt_record_value);
    let ok = verify().await.unwrap().into_inner();
    assert_eq!(ok.state, PagesDomainVerificationState::Verified as i32);
    assert!(ok.verified_at.is_some());
    let list =
        w.s.list_pages_verified_domains(req(
            &o,
            ListPagesVerifiedDomainsRequest {
                org: "acme".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.domains.len(), 1);
    // Another org cannot add or serve the verified domain, nor any subdomain of it.
    w.s.create_org(req(
        &w.tok("outsider"),
        CreateOrgRequest {
            name: "beta".into(),
            display: "Beta".into(),
        },
    ))
    .await
    .unwrap();
    let b = w.tok("outsider");
    assert_eq!(
        reason(
            w.s.add_pages_verified_domain(req(
                &b,
                AddPagesVerifiedDomainRequest {
                    org: "beta".into(),
                    domain: "example.com".into()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    w.s.create_repo(req(
        &b,
        CreateRepoRequest {
            org: "beta".into(),
            name: "site".into(),
            visibility: Visibility::Public as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    w.s.create_pages_site(req(
        &b,
        CreatePagesSiteRequest {
            repo: rref("beta/site"),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(set_domain(&w, &b, "beta/site", "docs.example.com").await),
        "PERMISSION_DENIED"
    );
    // The verifying org may.
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    assert_eq!(
        set_domain(&w, &o, "acme/app", "docs.example.com")
            .await
            .unwrap()
            .into_inner()
            .custom_domain,
        "docs.example.com"
    );
    // Removal.
    w.s.delete_pages_verified_domain(req(
        &o,
        DeletePagesVerifiedDomainRequest {
            org: "acme".into(),
            verification_id: v.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.delete_pages_verified_domain(req(
                &o,
                DeletePagesVerifiedDomainRequest {
                    org: "acme".into(),
                    verification_id: v.id
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    w.flush().await;
    assert!(
        w.event_kinds()
            .await
            .contains(&"pages.domain_verified".to_string())
    );
}

#[tokio::test]
async fn custom_domain_health_certificate_and_https() {
    let w = world().await;
    let o = w.tok("owner");
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    w.vcs
        .seed(
            &w.vr(&w.app),
            &[("index.html", "hello")],
            &Principal {
                id: "writer".into(),
                ..Default::default()
            },
            "c",
            T0,
        )
        .await
        .unwrap();
    w.s.request_pages_build(req(
        &w.tok("writer"),
        RequestPagesBuildRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    // Permissions.
    assert_eq!(
        reason(set_domain(&w, &w.tok("writer"), "acme/app", "www.example.org").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(set_domain(&w, &o, "acme/app", "not a domain").await),
        "INVALID_ARGUMENT"
    );
    // DNS is not ready: the certificate is requested, not issued, and HTTPS cannot be enforced.
    let s = set_domain(&w, &o, "acme/app", "www.example.org")
        .await
        .unwrap()
        .into_inner();
    assert_eq!(s.custom_domain, "www.example.org");
    assert_eq!(
        s.certificate.unwrap().state,
        PagesCertificateState::Requested as i32
    );
    assert_eq!(s.url, "http://www.example.org/");
    let enforce = |v: bool| {
        w.s.update_pages_site(req(
            &o,
            UpdatePagesSiteRequest {
                repo: rref("acme/app"),
                enforce_https: Some(v),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(enforce(true).await), "INVALID_STATE");
    let health =
        w.s.get_pages_health_check(req(
            &o,
            GetPagesHealthCheckRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let d = health.domain.unwrap();
    assert!(!d.dns_resolves && !d.https_eligible && !d.problems.is_empty());
    assert_eq!(health.alt_domain.unwrap().host, "example.org");
    assert_eq!(
        reason(
            w.s.request_pages_certificate(req(
                &o,
                RequestPagesCertificateRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // A CNAME to a third party is "proxied"; a CAA record excluding the CA blocks issuance.
    w.dns.set_cname("www.example.org", "cdn.thirdparty.net");
    let h =
        w.s.get_pages_health_check(req(
            &o,
            GetPagesHealthCheckRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .domain
        .unwrap();
    assert!(h.dns_resolves && h.is_proxied && !h.points_to_pages);
    w.dns.set_cname("www.example.org", "acme.pages.test");
    w.dns.add_caa("www.example.org", "0 issue \"digicert.com\"");
    let h =
        w.s.get_pages_health_check(req(
            &o,
            GetPagesHealthCheckRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .domain
        .unwrap();
    assert!(h.points_to_pages && h.has_caa_conflict && !h.https_eligible);
    assert_eq!(
        reason(
            w.s.request_pages_certificate(req(
                &o,
                RequestPagesCertificateRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Fix DNS (a clean zone), but the CA rejects the challenge once: ERRORED, recorded.
    w.dns.clear("www.example.org");
    w.dns.set_cname("www.example.org", "acme.pages.test");
    w.acme.fail_next("challenge rejected");
    let c =
        w.s.request_pages_certificate(req(
            &o,
            RequestPagesCertificateRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.state, PagesCertificateState::Errored as i32);
    assert!(c.error_message.contains("rejected"));
    // Retry succeeds: issued, with its dates; the apex counterpart joins when it points too.
    w.dns.add_a("example.org", "192.0.2.10");
    let c =
        w.s.request_pages_certificate(req(
            &o,
            RequestPagesCertificateRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.state, PagesCertificateState::Issued as i32);
    assert_eq!(
        c.domains,
        vec!["www.example.org".to_string(), "example.org".to_string()]
    );
    assert!(c.expires_at.is_some());
    // Now HTTPS can be enforced; the custom domain serves the site at its root; plain HTTP
    // redirects to HTTPS.
    let s = enforce(true).await.unwrap().into_inner();
    assert!(s.enforce_https);
    assert_eq!(s.url, "https://www.example.org/");
    let r = get(&w, "www.example.org", "/", None, true).await;
    assert_eq!(body_str(&r), "hello");
    let r = get(&w, "www.example.org", "/index.html", None, false).await;
    assert_eq!(
        (r.status().as_u16(), hdr(&r, "location")),
        (301, "https://www.example.org/index.html")
    );
    // Another site cannot take the domain.
    w.s.create_pages_site(req(
        &o,
        CreatePagesSiteRequest {
            repo: rref("acme/open"),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(set_domain(&w, &o, "acme/open", "WWW.example.org.").await),
        "ALREADY_EXISTS"
    );
    // Renewal: close to expiry the certificate is re-issued by maintenance.
    assert_eq!(w.acme.issued().len(), 1);
    w.clock.advance(65 * 86_400_000);
    w.env.forge.maintain().await.unwrap();
    assert_eq!(w.acme.issued().len(), 2);
    let o = w.tok("owner");
    // Clearing the domain drops the claim and certificate.
    let s = set_domain(&w, &o, "acme/app", "")
        .await
        .unwrap()
        .into_inner();
    assert!(s.custom_domain.is_empty());
    assert_eq!(
        s.certificate.unwrap().state,
        PagesCertificateState::None as i32
    );
    assert_eq!(
        get(&w, "www.example.org", "/", None, true).await.status(),
        404
    );
    assert_eq!(
        set_domain(&w, &o, "acme/open", "www.example.org")
            .await
            .unwrap()
            .into_inner()
            .custom_domain,
        "www.example.org"
    );
    let _ = Cx::system();
}

#[tokio::test]
async fn domain_with_ready_dns_is_issued_at_once() {
    let w = world().await;
    let o = w.tok("owner");
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    w.dns.add_a("docs.example.net", "192.0.2.11");
    let s = set_domain(&w, &o, "acme/app", "docs.example.net")
        .await
        .unwrap()
        .into_inner();
    let c = s.certificate.unwrap();
    assert_eq!(c.state, PagesCertificateState::Issued as i32);
    assert_eq!(s.url, "https://docs.example.net/");
}

/// Separate Postgres handles model two forge replicas racing to claim the same host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_custom_domain_claim_has_one_winner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };

    let mut cfg = SqlConfig::parse(&url).expect("parse BEE_TEST_PG");
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        *schema = format!("forge_pages_domain_claim_{}_{nonce}", std::process::id());
    }
    let first: std::sync::Arc<dyn ForgeStore> = std::sync::Arc::new(
        SqlStore::open(cfg.clone(), std::sync::Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: std::sync::Arc<dyn ForgeStore> = std::sync::Arc::new(
        SqlStore::open(cfg, std::sync::Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let w = world_on(first).await;

    let (replica, report) = Forge::open(
        second,
        std::sync::Arc::new(SystemClock),
        w.env.ring.verifying_keys(),
        w.env.revocations.clone(),
        Hooks {
            community: Ports::default(),
            ..Hooks::default()
        },
        {
            let mut config = common::test_config();
            config.pages.pages_domain = "pages.test".into();
            config
        },
    )
    .await
    .unwrap();
    assert!(report.pending_contract.is_empty());
    let replica_pages = replica.pages.clone();

    let owner = w.tok("owner");
    for repo in ["acme/app", "acme/open"] {
        w.s.create_pages_site(req(
            &owner,
            CreatePagesSiteRequest {
                repo: rref(repo),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    }

    for round in 0..24 {
        let domain = format!("race-{round}.example");
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let replica_pages = replica_pages.clone();
        let left = {
            let pages = w.env.forge.pages.clone();
            let repo = w.app.clone();
            let domain = domain.clone();
            async move {
                left_barrier.wait().await;
                pages.set_custom_domain(&Cx::system(), &repo, &domain).await
            }
        };
        let right = {
            let pages = replica_pages;
            let repo = w.open.clone();
            let domain = domain.clone();
            async move {
                right_barrier.wait().await;
                pages.set_custom_domain(&Cx::system(), &repo, &domain).await
            }
        };
        let (app, open) = tokio::join!(left, right);
        assert!(
            app.is_ok() ^ open.is_ok(),
            "exactly one replica must claim {domain}: app={app:?}, open={open:?}"
        );
        let (winner, loser_repo) = if app.is_ok() {
            (w.app.clone(), "acme/open")
        } else {
            (w.open.clone(), "acme/app")
        };
        let loser = app.as_ref().err().or(open.as_ref().err()).unwrap();
        assert_eq!(loser.reason, bee_proto::v1::ErrorReason::AlreadyExists);
        assert_eq!(
            loser.message,
            format!("domain {domain} (served by another site) already exists")
        );

        let loser_repo = if loser_repo == "acme/app" {
            &w.app
        } else {
            &w.open
        };
        let sequential = w
            .env
            .forge
            .pages
            .set_custom_domain(&Cx::system(), loser_repo, &domain)
            .await
            .unwrap_err();
        assert_eq!(sequential.reason, loser.reason);
        assert_eq!(sequential.message, loser.message);

        let mut tx = w
            .env
            .forge
            .db
            .begin(bee_forge::pages::SURFACE, &Cx::system())
            .await
            .unwrap();
        let host = tx
            .get(&bee_forge::pages::HOSTS, "", &domain)
            .await
            .unwrap()
            .expect("winning host row must exist");
        assert_eq!(host.k1.as_deref(), Some(winner.id.as_str()));
        drop(tx);

        let app_site =
            w.s.get_pages_site(req(
                &owner,
                GetPagesSiteRequest {
                    repo: rref("acme/app"),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let open_site =
            w.s.get_pages_site(req(
                &owner,
                GetPagesSiteRequest {
                    repo: rref("acme/open"),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let (winner_site, loser_site) = if winner.id == w.app.id {
            (&app_site, &open_site)
        } else {
            (&open_site, &app_site)
        };
        assert_eq!(winner_site.custom_domain, domain);
        assert_ne!(loser_site.custom_domain, domain);
    }
}
