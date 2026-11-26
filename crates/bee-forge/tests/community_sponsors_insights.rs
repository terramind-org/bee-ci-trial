//! `SponsorService` (profiles, tiers, sponsorships, sponsor-only repo access, payouts,
//! FUNDING.yml) and `InsightsService` (repo analysis, pulse, contributors, traffic, network
//! graph, agent statistics, org dependencies), plus the RPCs served elsewhere.

use crate::community_support as support;

use bee_forge::Cx;
use bee_forge::community::ports::{ChangeActivity, ChangeOutcome, IssueActivity, LogEntry};
use bee_policy::Action;
use bee_proto::v1::insights_service_server::InsightsService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::sponsor_service_server::SponsorService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use support::*;

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

fn oid(n: u8) -> ObjectId {
    ObjectId { raw: vec![n; 4] }
}

fn entry(
    n: u8,
    parents: &[u8],
    who: Principal,
    ago: u64,
    adds: u64,
    dels: u64,
    refs: &[&str],
) -> LogEntry {
    LogEntry {
        changeset: oid(n),
        parents: parents.iter().map(|p| oid(*p)).collect(),
        author: who,
        time_ms: T0 - ago,
        additions: adds,
        deletions: dels,
        files_changed: 1,
        summary: format!("change {n}\n\nlong body"),
        refs: refs.iter().map(|r| r.to_string()).collect(),
        ..Default::default()
    }
}

async fn sponsor_tier(
    w: &World,
    who: &str,
    name: &str,
    cents: u64,
    repos: &[&str],
    publish: bool,
) -> Result<SponsorTier, String> {
    w.s.create_sponsor_tier(req(
        &w.tok(who),
        CreateSponsorTierRequest {
            name: name.into(),
            amount_cents: cents,
            frequency: SponsorFrequency::Monthly as i32,
            sponsor_only_repos: repos.iter().map(|r| rref(r).unwrap()).collect(),
            publish,
            ..Default::default()
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

async fn sponsor(
    w: &World,
    who: &str,
    login: &str,
    tier: &str,
    cents: u64,
    private: bool,
) -> Result<CreateSponsorshipResponse, String> {
    w.s.create_sponsorship(req(
        &w.tok(who),
        CreateSponsorshipRequest {
            maintainee_login: login.into(),
            tier_id: tier.into(),
            amount_cents: cents,
            private,
            ..Default::default()
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

async fn can_read_app(w: &World, who: &str) -> bool {
    w.s.get_repo(req(
        &w.tok(who),
        GetRepoRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiers_profiles_and_sponsorships_follow_their_state_machines() {
    let w = world().await;
    assert_eq!(
        reason(
            w.s.update_sponsor_profile(req(
                &w.tok("owner"),
                UpdateSponsorProfileRequest {
                    enabled: Some(true),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        sponsor_tier(&w, "reader", "Fan", 500, &["acme/app"], true)
            .await
            .unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        sponsor_tier(&w, "owner", "Cheap", 50, &[], true)
            .await
            .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    let draft = sponsor_tier(&w, "owner", "Supporter", 500, &["acme/app"], false)
        .await
        .unwrap();
    assert_eq!(draft.state, SponsorTierState::Draft as i32);
    assert_eq!(
        sponsor_tier(&w, "owner", "supporter", 700, &[], false)
            .await
            .unwrap_err(),
        "ALREADY_EXISTS"
    );
    let upd = |who: &str, id: &str, f: &dyn Fn(&mut UpdateSponsorTierRequest)| {
        let mut r = UpdateSponsorTierRequest {
            tier_id: id.into(),
            ..Default::default()
        };
        f(&mut r);
        w.s.update_sponsor_tier(req(&w.tok(who), r))
    };
    assert_eq!(
        reason(upd("reader", &draft.id, &|r| r.name = Some("x".into())).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        upd("owner", &draft.id, &|r| r.amount_cents = Some(600))
            .await
            .unwrap()
            .into_inner()
            .amount_cents,
        600
    );
    upd("owner", &draft.id, &|r| {
        r.state = Some(SponsorTierState::Published as i32)
    })
    .await
    .unwrap();
    assert_eq!(
        reason(upd("owner", &draft.id, &|r| r.amount_cents = Some(900)).await),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            upd("owner", &draft.id, &|r| r.state =
                Some(SponsorTierState::Draft as i32))
            .await
        ),
        "INVALID_STATE"
    );
    let profile =
        w.s.update_sponsor_profile(req(
            &w.tok("owner"),
            UpdateSponsorProfileRequest {
                enabled: Some(true),
                introduction: Some("hi".into()),
                goal_cents: Some(5000),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(profile.enabled && profile.tiers.len() == 1);
    assert_eq!(
        sponsor(&w, "owner", "owner-login", &draft.id, 0, false)
            .await
            .unwrap_err(),
        "INVALID_STATE"
    );
    assert_eq!(
        sponsor(&w, "outsider", "owner-login", &draft.id, 123, false)
            .await
            .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    assert!(!can_read_app(&w, "outsider").await);
    let ok = sponsor(&w, "outsider", "owner-login", &draft.id, 0, false)
        .await
        .unwrap();
    let sp = ok.sponsorship.unwrap();
    assert_eq!(
        (sp.state, sp.amount_cents, ok.checkout_url.is_empty()),
        (SponsorshipState::Active as i32, 600, true)
    );
    assert_eq!(
        sponsor(&w, "outsider", "owner-login", &draft.id, 0, false)
            .await
            .unwrap_err(),
        "ALREADY_EXISTS"
    );
    // Sponsor-only access: the sponsor now reads the private repo.
    assert!(can_read_app(&w, "outsider").await);
    let access =
        w.s.list_sponsor_content_access(req(
            &w.tok("owner"),
            ListSponsorContentAccessRequest::default(),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            access.accesses.len(),
            access.accesses[0].sponsor.as_ref().unwrap().id.as_str()
        ),
        (1, "outsider")
    );
    assert_eq!(
        reason(
            w.s.list_sponsor_content_access(req(
                &w.tok("reader"),
                ListSponsorContentAccessRequest {
                    login: "owner-login".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    // Private sponsorships are hidden from everyone but the parties and out of public totals.
    let secret = sponsor(&w, "writer", "owner-login", &draft.id, 0, true)
        .await
        .unwrap()
        .sponsorship
        .unwrap();
    assert_eq!(
        reason(
            w.s.get_sponsorship(req(
                &w.tok("reader"),
                GetSponsorshipRequest {
                    sponsorship_id: secret.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert!(
        w.s.get_sponsorship(req(
            &w.tok("writer"),
            GetSponsorshipRequest {
                sponsorship_id: secret.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    assert!(
        w.s.get_sponsorship(req(
            &w.tok("owner"),
            GetSponsorshipRequest {
                sponsorship_id: secret.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    let public_total =
        w.s.get_sponsor_profile(req(
            &w.tok("reader"),
            GetSponsorProfileRequest {
                login: "owner-login".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let owner_total =
        w.s.get_sponsor_profile(req(
            &w.tok("owner"),
            GetSponsorProfileRequest {
                login: "owner-login".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            public_total.monthly_total_cents,
            public_total.sponsor_count,
            owner_total.monthly_total_cents,
            owner_total.sponsor_count
        ),
        (600, 1, 1200, 2)
    );
    let list = |who: &str, role: SponsorshipRole, login: &str, private: bool| {
        w.s.list_sponsorships(req(
            &w.tok(who),
            ListSponsorshipsRequest {
                role: role as i32,
                login: login.into(),
                include_private: private,
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        list("reader", SponsorshipRole::Maintainee, "owner-login", true)
            .await
            .unwrap()
            .into_inner()
            .sponsorships
            .len(),
        1
    );
    assert_eq!(
        list("owner", SponsorshipRole::Maintainee, "", true)
            .await
            .unwrap()
            .into_inner()
            .sponsorships
            .len(),
        2
    );
    assert_eq!(
        list("writer", SponsorshipRole::Sponsor, "", true)
            .await
            .unwrap()
            .into_inner()
            .sponsorships
            .len(),
        1
    );
    // Cancelling a monthly sponsorship keeps access to the end of the paid period.
    assert_eq!(
        reason(
            w.s.cancel_sponsorship(req(
                &w.tok("reader"),
                CancelSponsorshipRequest {
                    sponsorship_id: sp.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let cancelling =
        w.s.cancel_sponsorship(req(
            &w.tok("outsider"),
            CancelSponsorshipRequest {
                sponsorship_id: sp.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cancelling.state, SponsorshipState::Cancelling as i32);
    assert!(w.pay.cancelled.lock().unwrap().contains(&sp.id));
    assert!(can_read_app(&w, "outsider").await);
    w.clock.advance(31 * DAY);
    w.env
        .forge
        .community
        .sponsors
        .maintain(&Cx::system())
        .await
        .unwrap();
    assert!(!can_read_app(&w, "outsider").await);
    let ended =
        w.s.get_sponsorship(req(
            &w.tok("outsider"),
            GetSponsorshipRequest {
                sponsorship_id: sp.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ended.state, SponsorshipState::Ended as i32);
    // Checkout flow: pending until the provider confirms.
    w.pay
        .pending
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let pend = sponsor(&w, "reader", "owner-login", &draft.id, 0, false)
        .await
        .unwrap();
    assert!(
        pend.checkout_url
            .starts_with("https://pay.example/checkout/")
    );
    let ps = pend.sponsorship.unwrap();
    assert_eq!(ps.state, SponsorshipState::Pending as i32);
    let sponsors = &w.env.forge.community.sponsors;
    let active = sponsors
        .confirm_payment(&Cx::system(), &ps.id, true)
        .await
        .unwrap();
    assert_eq!(active.state, SponsorshipState::Active as i32);
    assert!(
        sponsors
            .confirm_payment(&Cx::system(), &ps.id, true)
            .await
            .is_err()
    );
    let failed = sponsor(&w, "ci", "owner-login", &draft.id, 0, false)
        .await
        .unwrap()
        .sponsorship
        .unwrap();
    assert_eq!(
        sponsors
            .confirm_payment(&Cx::system(), &failed.id, false)
            .await
            .unwrap()
            .state,
        SponsorshipState::Failed as i32
    );
    // A tier with live sponsorships is retired, not deleted.
    assert_eq!(
        reason(
            w.s.delete_sponsor_tier(req(
                &w.tok("owner"),
                DeleteSponsorTierRequest {
                    tier_id: draft.id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    upd("owner", &draft.id, &|r| {
        r.state = Some(SponsorTierState::Retired as i32)
    })
    .await
    .unwrap();
    assert_eq!(
        sponsor(&w, "admin", "owner-login", &draft.id, 0, false)
            .await
            .unwrap_err(),
        "INVALID_STATE"
    );
    // Payouts and the fiscal host.
    let owner = bee_forge::community::sponsors::Owner::user(&human("owner"), "owner-login");
    sponsors
        .record_payout(&Cx::system(), &owner, 4000, "paid", T0 + 29 * DAY)
        .await
        .unwrap();
    sponsors
        .record_payout(&Cx::system(), &owner, 1500, "scheduled", T0 + 40 * DAY)
        .await
        .unwrap();
    let summary =
        w.s.get_sponsor_payout_summary(req(
            &w.tok("owner"),
            GetSponsorPayoutSummaryRequest::default(),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            summary.pending_cents,
            summary.paid_lifetime_cents,
            summary.paid_last_30_days_cents,
            summary.recent_payouts.len()
        ),
        (1500, 4000, 4000, 2)
    );
    assert_eq!(
        reason(
            w.s.get_sponsor_payout_summary(req(
                &w.tok("reader"),
                GetSponsorPayoutSummaryRequest {
                    login: "owner-login".into()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            w.s.get_sponsor_fiscal_host(req(
                &w.tok("reader"),
                GetSponsorFiscalHostRequest {
                    login: "owner-login".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    sponsors
        .set_fiscal_host(
            &Cx::system(),
            &owner,
            Some(SponsorFiscalHost {
                name: "OSC".into(),
                fee_basis_points: 500,
                verified: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        w.s.get_sponsor_fiscal_host(req(
            &w.tok("reader"),
            GetSponsorFiscalHostRequest {
                login: "owner-login".into()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .fee_basis_points,
        500
    );
    // Orgs are sponsorable too; only owners and admins manage them.
    assert_eq!(
        reason(
            w.s.update_sponsor_profile(req(
                &w.tok("writer"),
                UpdateSponsorProfileRequest {
                    login: "acme".into(),
                    introduction: Some("x".into()),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert!(
        w.s.update_sponsor_profile(req(
            &w.tok("admin"),
            UpdateSponsorProfileRequest {
                login: "acme".into(),
                introduction: Some("We build".into()),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
    w.flush().await;
    assert!(
        w.env
            .forge
            .events
            .read(&Cx::system(), "system", 0, &[], 1000)
            .await
            .unwrap()
            .0
            .iter()
            .any(|e| e.kind == "sponsor.created")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn funding_files_are_parsed_into_links() {
    let w = world().await;
    let funding =
        "github: [octocat, hubot]\nko_fi: me\nunknown_site: x\ncustom: [example.com/donate]\n";
    w.vcs
        .seed(
            &w.vr(&w.open),
            &[(".github/FUNDING.yml", funding)],
            &human("owner"),
            "funding",
            T0,
        )
        .await
        .unwrap();
    let f =
        w.s.get_sponsor_funding_file(req(
            &w.tok("outsider"),
            GetSponsorFundingFileRequest {
                repo: rref("acme/open"),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(f.path, b".github/FUNDING.yml".to_vec());
    assert_eq!(
        f.links
            .iter()
            .map(|l| l.platform.as_str())
            .collect::<Vec<_>>(),
        vec!["github", "ko_fi", "custom"]
    );
    assert_eq!(f.links[0].urls[1], "https://github.com/sponsors/hubot");
    assert_eq!(f.warnings.len(), 1);
    w.vcs
        .seed(
            &w.vr(&w.app),
            &[("README.md", "x")],
            &human("owner"),
            "init",
            T0,
        )
        .await
        .unwrap();
    assert_eq!(
        reason(
            w.s.get_sponsor_funding_file(req(
                &w.tok("reader"),
                GetSponsorFundingFileRequest {
                    repo: rref("acme/app"),
                    at: None
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            w.s.get_sponsor_funding_file(req(
                &w.tok("outsider"),
                GetSponsorFundingFileRequest {
                    repo: rref("acme/app"),
                    at: None
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_files_yield_languages_license_community_profile_and_citation() {
    let w = world().await;
    let mit = "MIT License\n\nPermission is hereby granted, free of charge, to any person obtaining a copy";
    let cff = "cff-version: 1.2.0\ntitle: App\nversion: 2.0.0\ndate-released: 2026-10-01\nauthors:\n  - family-names: Doe\n    given-names: Jane\n";
    let rust = "fn main() {}\n".repeat(30);
    let big = "x".repeat(5000);
    w.vcs
        .seed(
            &w.vr(&w.app),
            &[
                ("src/main.rs", &rust),
                ("web/app.js", "let a;\n"),
                ("vendor/big.js", &big),
                ("docs/guide.md", "g"),
                ("LICENSE", mit),
                ("CITATION.cff", cff),
                (".github/CONTRIBUTING.md", "c"),
                (".github/ISSUE_TEMPLATE/bug.yml", "name: bug"),
                ("README.md", "# app"),
            ],
            &human("owner"),
            "init",
            T0,
        )
        .await
        .unwrap();
    let repo = rref("acme/app");
    let langs =
        w.s.get_language_stats(req(
            &w.tok("reader"),
            GetLanguageStatsRequest {
                repo: repo.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        langs
            .languages
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Rust", "JavaScript"]
    );
    assert_eq!(langs.total_bytes, 390 + 7);
    let with_vendor =
        w.s.get_language_stats(req(
            &w.tok("reader"),
            GetLanguageStatsRequest {
                repo: repo.clone(),
                include_vendored: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(with_vendor.languages[0].name, "JavaScript");
    let lic =
        w.s.get_license_info(req(
            &w.tok("reader"),
            GetLicenseInfoRequest {
                repo: repo.clone(),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (lic.spdx_id.as_str(), lic.matcher.as_str()),
        ("MIT", "fingerprint")
    );
    let cp =
        w.s.get_community_profile(req(
            &w.tok("reader"),
            GetCommunityProfileRequest {
                repo: repo.clone(),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    // description, README, contributing, license and a template: 5 of 6 (no code of conduct).
    assert_eq!(cp.health_percentage, 83);
    let present = |k: CommunityFileKind| cp.files.iter().any(|f| f.kind == k as i32 && f.present);
    assert!(
        present(CommunityFileKind::Readme)
            && present(CommunityFileKind::Contributing)
            && present(CommunityFileKind::IssueTemplate)
            && !present(CommunityFileKind::CodeOfConduct)
    );
    let cite =
        w.s.get_citation(req(
            &w.tok("reader"),
            GetCitationRequest {
                repo: repo.clone(),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        cite.apa,
        "Doe, J. (2026). App (Version 2.0.0) [Computer software]."
    );
    assert!(cite.bibtex.starts_with("@software{doe2026app,"));
    assert_eq!(
        reason(
            w.s.get_language_stats(req(
                &w.tok("outsider"),
                GetLanguageStatsRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Without a license file, Cargo.toml's license is used.
    w.vcs
        .seed(
            &w.vr(&w.open),
            &[(
                "Cargo.toml",
                "[package]\nname = \"x\"\nlicense = \"Apache-2.0\"\n",
            )],
            &human("owner"),
            "init",
            T0,
        )
        .await
        .unwrap();
    let l2 =
        w.s.get_license_info(req(
            &w.tok("outsider"),
            GetLicenseInfoRequest {
                repo: rref("acme/open"),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (l2.spdx_id.as_str(), l2.matcher.as_str()),
        ("Apache-2.0", "package_manifest")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pulse_contributors_activity_network_and_agent_stats() {
    let w = world().await;
    let (ann, bot) = (human("ann"), agent("bot"));
    let mut reverted = entry(4, &[3], bot.clone(), 3 * HOUR, 5, 5, &[]);
    reverted.reverted = true;
    reverted.cost = (1000, 500, 20_000);
    let mut e5 = entry(5, &[4], bot.clone(), 2 * HOUR, 7, 0, &["main"]);
    e5.cost = (2000, 1000, 40_000);
    let placeholder = LogEntry {
        author: Principal::default(),
        placeholder_email: "writer@example.com".into(),
        ..entry(6, &[5], Principal::default(), HOUR, 1, 1, &[])
    };
    w.activity.set_log(
        &w.app.id,
        vec![
            entry(1, &[], ann.clone(), 20 * DAY, 100, 0, &[]),
            entry(2, &[1], ann.clone(), 2 * DAY, 10, 2, &[]),
            entry(3, &[2], ann.clone(), 5 * HOUR, 3, 1, &["feature"]),
            reverted,
            e5,
            placeholder,
        ],
    );
    w.activity.set_change_requests(
        &w.app.id,
        vec![
            ChangeActivity {
                number: 1,
                author: ann.clone(),
                outcome: ChangeOutcome::Landed,
                opened_ms: T0 - 2 * DAY,
                ended_ms: T0 - DAY,
                review_rounds: 1,
            },
            ChangeActivity {
                number: 2,
                author: bot.clone(),
                outcome: ChangeOutcome::Landed,
                opened_ms: T0 - 4 * HOUR,
                ended_ms: T0 - 2 * HOUR,
                review_rounds: 3,
            },
            ChangeActivity {
                number: 3,
                author: bot.clone(),
                outcome: ChangeOutcome::Closed,
                opened_ms: T0 - 3 * HOUR,
                ended_ms: T0 - HOUR,
                review_rounds: 0,
            },
            ChangeActivity {
                number: 4,
                author: ann.clone(),
                outcome: ChangeOutcome::Open,
                opened_ms: T0 - HOUR,
                ended_ms: 0,
                review_rounds: 0,
            },
        ],
    );
    w.activity.set_issues(
        &w.app.id,
        vec![
            IssueActivity {
                number: 7,
                opened_ms: T0 - DAY,
                closed_ms: T0 - HOUR,
            },
            IssueActivity {
                number: 8,
                opened_ms: T0 - 30 * DAY,
                closed_ms: 0,
            },
        ],
    );
    let repo = rref("acme/app");
    let pulse = |window: InsightsWindow| {
        w.s.get_pulse(req(
            &w.tok("reader"),
            GetPulseRequest {
                repo: repo.clone(),
                window: window as i32,
            },
        ))
    };
    let p = pulse(InsightsWindow::Week).await.unwrap().into_inner();
    assert_eq!(
        (
            p.change_requests_opened,
            p.change_requests_landed,
            p.change_requests_closed
        ),
        (4, 2, 1)
    );
    assert_eq!((p.issues_opened, p.issues_closed), (1, 1));
    assert_eq!((p.changesets_landed, p.additions, p.deletions), (5, 26, 9));
    assert_eq!((p.human_authors, p.agent_authors), (2, 1));
    assert_eq!(p.top_authors[0].author.as_ref().unwrap().id, "ann");
    assert_eq!(
        p.landed_change_requests
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["2", "1"]
    );
    assert_eq!(
        pulse(InsightsWindow::Day)
            .await
            .unwrap()
            .into_inner()
            .changesets_landed,
        4
    );
    assert_eq!(
        pulse(InsightsWindow::Month)
            .await
            .unwrap()
            .into_inner()
            .changesets_landed,
        6
    );
    assert_eq!(
        pulse(InsightsWindow::Year)
            .await
            .unwrap()
            .into_inner()
            .changesets_landed,
        6
    );
    // Contributors: agents are counted beside humans; a placeholder whose email an account
    // has since verified counts for the account.
    let list = |f: &dyn Fn(&mut ListContributorsRequest)| {
        let mut r = ListContributorsRequest {
            repo: repo.clone(),
            ..Default::default()
        };
        f(&mut r);
        w.s.list_contributors(req(&w.tok("reader"), r))
    };
    let c = list(&|_| {}).await.unwrap().into_inner();
    assert_eq!(
        (c.human_count, c.agent_count, c.placeholder_count),
        (1, 1, 1)
    );
    assert_eq!(c.contributors[0].principal.as_ref().unwrap().id, "ann");
    assert_eq!(c.contributors[0].changesets, 3);
    let ph = c.contributors.iter().find(|x| x.placeholder).unwrap();
    assert_eq!(ph.placeholder_email, "writer@example.com");
    let users = &w.env.forge.community.users;
    users
        .add_email(&w.cx("writer"), "writer@example.com")
        .await
        .unwrap();
    let t = users.claim_verifications(&Cx::system(), 5).await.unwrap();
    users.verify_email(&t[0].token).await.unwrap();
    let c = list(&|r| r.include_weeks = true)
        .await
        .unwrap()
        .into_inner();
    assert_eq!((c.human_count, c.placeholder_count), (2, 0));
    assert!(
        c.contributors
            .iter()
            .any(|x| x.principal.as_ref().is_some_and(|p| p.id == "writer") && x.changesets == 1)
    );
    assert!(c.contributors[0].weeks.len() >= 2);
    assert_eq!(
        list(&|r| r.kind = PrincipalKind::Agent as i32)
            .await
            .unwrap()
            .into_inner()
            .contributors
            .len(),
        1
    );
    assert_eq!(
        list(&|r| r.since = Some(bee_forge::ids::ts(T0 - DAY)))
            .await
            .unwrap()
            .into_inner()
            .contributors
            .len(),
        3
    );
    let paged = list(&|r| {
        r.page = Some(PageRequest {
            page_size: 1,
            page_token: String::new(),
        })
    })
    .await
    .unwrap()
    .into_inner();
    assert_eq!(paged.contributors.len(), 1);
    assert!(!paged.page.unwrap().next_page_token.is_empty());
    // Commit activity: 52 weeks oldest first, days Sunday..Saturday; code frequency per week.
    let ca =
        w.s.get_commit_activity(req(
            &w.tok("reader"),
            GetCommitActivityRequest { repo: repo.clone() },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ca.weeks.len(), 52);
    assert_eq!(ca.weeks.iter().map(|x| x.total).sum::<u32>(), 6);
    let cf =
        w.s.get_code_frequency(req(
            &w.tok("reader"),
            GetCodeFrequencyRequest { repo: repo.clone() },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cf.weeks.iter().map(|x| x.additions).sum::<u64>(), 126);
    // Network graph: nodes newest first, parent edges, lanes, truncation, ref filter.
    let net = |f: &dyn Fn(&mut GetNetworkGraphRequest)| {
        let mut r = GetNetworkGraphRequest {
            repo: repo.clone(),
            ..Default::default()
        };
        f(&mut r);
        w.s.get_network_graph(req(&w.tok("reader"), r))
    };
    let g = net(&|_| {}).await.unwrap().into_inner();
    assert_eq!(g.nodes.len(), 6);
    assert_eq!(g.nodes[0].changeset.as_ref().unwrap(), &oid(6));
    assert_eq!(g.edges.len(), 5);
    assert!(g.nodes.iter().all(|n| n.lane == 0));
    assert!(!g.truncated);
    let t = net(&|r| r.max_nodes = 3).await.unwrap().into_inner();
    assert!(t.truncated && t.nodes.len() == 3 && t.edges.len() == 2);
    assert_eq!(
        net(&|r| r.refs = vec!["feature".into()])
            .await
            .unwrap()
            .into_inner()
            .nodes
            .len(),
        1
    );
    // Agent statistics: landed, reverted, cost and review rounds.
    let a =
        w.s.list_insights_agent_stats(req(
            &w.tok("reader"),
            ListInsightsAgentStatsRequest {
                repo: repo.clone(),
                window: InsightsWindow::Week as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a.agents.len(), 1);
    let s = &a.agents[0];
    assert_eq!(
        (
            s.change_requests_opened,
            s.change_requests_landed,
            s.change_requests_closed,
            s.changesets_landed,
            s.changesets_reverted
        ),
        (2, 1, 1, 2, 1)
    );
    assert!((s.revert_rate - 0.5).abs() < 1e-9);
    assert_eq!(
        (
            s.cost.as_ref().unwrap().input_tokens,
            s.cost.as_ref().unwrap().usd_micros
        ),
        (3000, 60_000)
    );
    assert_eq!(a.total_cost.unwrap().output_tokens, 1500);
    assert_eq!(
        reason(
            w.s.get_pulse(req(
                &w.tok("outsider"),
                GetPulseRequest {
                    repo: repo.clone(),
                    window: 0
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traffic_counters_cover_fourteen_days_and_need_push_access() {
    let w = world().await;
    let ins = &w.env.forge.community.insights;
    let repo = rref("acme/open");
    let views = |who: &str, interval: TrafficInterval| {
        w.s.get_traffic_views(req(
            &w.tok(who),
            GetTrafficViewsRequest {
                repo: repo.clone(),
                interval: interval as i32,
            },
        ))
    };
    assert_eq!(
        reason(views("reader", TrafficInterval::Day).await),
        "PERMISSION_DENIED"
    );
    for (who, path, referrer) in [
        ("v1", "/acme/open", "https://news.example/item/1"),
        (
            "v1",
            "/acme/open/tree/main/docs",
            "https://news.example/item/1",
        ),
        ("v2", "/acme/open", "https://search.example/?q=x"),
        ("v3", "/acme/open", ""),
    ] {
        ins.record_view(&w.open.id, who, path, referrer)
            .await
            .unwrap();
    }
    ins.record_clone(&w.open.id, "ci-1").await.unwrap();
    ins.record_clone(&w.open.id, "ci-1").await.unwrap();
    ins.record_clone(&w.open.id, "ci-2").await.unwrap();
    let v = views("writer", TrafficInterval::Day)
        .await
        .unwrap()
        .into_inner();
    assert_eq!((v.count, v.uniques, v.points.len()), (4, 3, 14));
    assert_eq!(v.points.last().unwrap().count, 4);
    let c =
        w.s.get_traffic_clones(req(
            &w.tok("writer"),
            GetTrafficClonesRequest {
                repo: repo.clone(),
                interval: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((c.count, c.uniques), (3, 2));
    let refs =
        w.s.list_traffic_referrers(req(
            &w.tok("writer"),
            ListTrafficReferrersRequest { repo: repo.clone() },
        ))
        .await
        .unwrap()
        .into_inner()
        .referrers;
    assert_eq!(
        (refs[0].referrer.as_str(), refs[0].count, refs[0].uniques),
        ("news.example", 2, 1)
    );
    let paths =
        w.s.list_traffic_popular_paths(req(
            &w.tok("writer"),
            ListTrafficPopularPathsRequest { repo: repo.clone() },
        ))
        .await
        .unwrap()
        .into_inner()
        .paths;
    assert_eq!(
        (paths[0].path.as_str(), paths[0].count, paths[0].uniques),
        ("/acme/open", 3, 3)
    );
    let weekly = views("writer", TrafficInterval::Week)
        .await
        .unwrap()
        .into_inner();
    assert!(weekly.points.len() <= 3 && weekly.points.iter().map(|p| p.count).sum::<u64>() == 4);
    // Old days fall out of the window, and pruning removes them.
    w.clock.advance(20 * DAY);
    let v = views("writer", TrafficInterval::Day)
        .await
        .unwrap()
        .into_inner();
    assert_eq!((v.count, v.uniques), (0, 0));
    assert!(ins.prune_traffic(&Cx::system()).await.unwrap() > 0);
    assert_eq!(ins.prune_traffic(&Cx::system()).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_dependency_insights_use_the_dependency_hook() {
    let w = world().await;
    *w.deps.0.lock().unwrap() = vec![
        InsightsOrgDependency {
            ecosystem: "cargo".into(),
            package_name: "serde".into(),
            versions: vec!["1.0".into()],
            repo_count: 2,
            open_advisories: 0,
            ..Default::default()
        },
        InsightsOrgDependency {
            ecosystem: "cargo".into(),
            package_name: "openssl".into(),
            repo_count: 1,
            open_advisories: 2,
            highest_severity: "high".into(),
            ..Default::default()
        },
        InsightsOrgDependency {
            ecosystem: "npm".into(),
            package_name: "left-pad".into(),
            repo_count: 1,
            ..Default::default()
        },
    ];
    let list = |who: &str, f: &dyn Fn(&mut ListInsightsOrgDependenciesRequest)| {
        let mut r = ListInsightsOrgDependenciesRequest {
            org: "acme".into(),
            ..Default::default()
        };
        f(&mut r);
        w.s.list_insights_org_dependencies(req(&w.tok(who), r))
    };
    let all = list("reader", &|_| {}).await.unwrap().into_inner();
    assert_eq!(
        (all.total_count, all.dependencies[0].package_name.as_str()),
        (3, "openssl")
    );
    assert_eq!(
        list("reader", &|r| r.ecosystem = "npm".into())
            .await
            .unwrap()
            .into_inner()
            .dependencies
            .len(),
        1
    );
    assert_eq!(
        list("reader", &|r| r.vulnerable_only = true)
            .await
            .unwrap()
            .into_inner()
            .dependencies[0]
            .package_name,
        "openssl"
    );
    assert_eq!(
        list("reader", &|r| r.query = "ser".into())
            .await
            .unwrap()
            .into_inner()
            .dependencies
            .len(),
        1
    );
    assert_eq!(reason(list("outsider", &|_| {}).await), "PERMISSION_DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpcs_owned_by_other_subsystems_say_who_serves_them() {
    let w = world().await;
    let t = w.tok("reader");
    let detail = |st: tonic::Status| {
        let d = bee_proto::errors::detail_from_status(&st).unwrap();
        (d.reason, d.subcode, d.message)
    };
    let a = detail(
        w.s.list_user_sessions(req(&t, ListUserSessionsRequest::default()))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (a.0.as_str(), a.1.as_str()),
        ("UNIMPLEMENTED", "served_elsewhere")
    );
    assert!(a.2.contains("identity"));
    let b = detail(
        w.s.list_user_ssh_keys(req(&t, ListUserSshKeysRequest::default()))
            .await
            .unwrap_err(),
    );
    assert!(b.2.contains("identity"));
    assert!(
        w.s.add_user_ssh_key(req(&t, AddUserSshKeyRequest::default()))
            .await
            .is_err()
    );
    // Unauthenticated callers are refused everywhere.
    let anon =
        w.s.get_viewer(tonic::Request::new(GetViewerRequest {}))
            .await
            .unwrap_err();
    assert_eq!(
        bee_proto::errors::detail_from_status(&anon).unwrap().reason,
        "UNAUTHENTICATED"
    );
    // A token limited to `read` cannot change anything.
    let ro = w.env.token("reader", actions(&[Action::Read]));
    assert_eq!(
        reason(
            w.s.follow_user(req(
                &ro,
                FollowUserRequest {
                    login: "writer-login".into()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    assert!(w.s.get_viewer(req(&ro, GetViewerRequest {})).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_of_conduct_is_detected_and_completes_the_community_profile() {
    let w = world().await;
    let covenant = "# Contributor Covenant Code of Conduct\n\n## Our Pledge\n\nWe as members, \
                    contributors, and leaders pledge to make participation in our community a \
                    harassment-free experience for everyone.\n\n## Attribution\n\nThis Code of \
                    Conduct is adapted from the Contributor Covenant, version 2.1.\n";
    w.vcs
        .seed(
            &w.vr(&w.app),
            &[
                ("README.md", "# app"),
                (
                    "LICENSE",
                    "MIT License\n\nPermission is hereby granted, free of charge",
                ),
                ("CONTRIBUTING.md", "how to contribute"),
                (".github/CODE_OF_CONDUCT.md", covenant),
                ("docs/CODE_OF_CONDUCT.md", "an older copy"),
                (".github/PULL_REQUEST_TEMPLATE.md", "## What"),
            ],
            &human("owner"),
            "community files",
            T0,
        )
        .await
        .unwrap();
    let cp =
        w.s.get_community_profile(req(
            &w.tok("reader"),
            GetCommunityProfileRequest {
                repo: rref("acme/app"),
                at: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    // Description, README, code of conduct, contributing, license and a template: 6 of 6.
    assert_eq!(cp.health_percentage, 100);
    let coc = cp
        .files
        .iter()
        .find(|f| f.kind == CommunityFileKind::CodeOfConduct as i32)
        .expect("a code-of-conduct entry");
    assert!(coc.present);
    assert_eq!(coc.detail, "contributor_covenant_2.1");
    assert_eq!(
        coc.paths,
        vec![
            b".github/CODE_OF_CONDUCT.md".to_vec(),
            b"docs/CODE_OF_CONDUCT.md".to_vec()
        ],
        "the .github copy wins over docs"
    );
    let present = |k: CommunityFileKind| cp.files.iter().any(|f| f.kind == k as i32 && f.present);
    assert!(
        present(CommunityFileKind::Readme)
            && present(CommunityFileKind::License)
            && present(CommunityFileKind::Contributing)
            && present(CommunityFileKind::ChangeRequestTemplate)
    );
    assert!(cp.has_description);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_time_sponsorships_sit_beside_monthly_ones_and_end_after_their_period() {
    let w = world().await;
    let tier = sponsor_tier(&w, "owner", "Monthly fan", 500, &[], true)
        .await
        .unwrap();
    w.s.update_sponsor_profile(req(
        &w.tok("owner"),
        UpdateSponsorProfileRequest {
            enabled: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let one_time = |cents: u64, frequency: SponsorFrequency| {
        w.s.create_sponsorship(req(
            &w.tok("outsider"),
            CreateSponsorshipRequest {
                maintainee_login: "owner-login".into(),
                amount_cents: cents,
                frequency: frequency as i32,
                note: "thanks".into(),
                ..Default::default()
            },
        ))
    };
    // A custom amount with no frequency is a one-time sponsorship.
    let first = one_time(2500, SponsorFrequency::Unspecified)
        .await
        .unwrap()
        .into_inner()
        .sponsorship
        .unwrap();
    assert_eq!(
        (first.frequency, first.state, first.amount_cents),
        (
            SponsorFrequency::OneTime as i32,
            SponsorshipState::Active as i32,
            2500
        )
    );
    // One-time gifts repeat; a monthly sponsorship of the same maintainer is separate.
    let second = one_time(1000, SponsorFrequency::OneTime)
        .await
        .unwrap()
        .into_inner()
        .sponsorship
        .unwrap();
    assert_ne!(second.id, first.id);
    let monthly = sponsor(&w, "outsider", "owner-login", &tier.id, 0, false)
        .await
        .unwrap()
        .sponsorship
        .unwrap();
    assert_eq!(monthly.frequency, SponsorFrequency::Monthly as i32);
    assert_eq!(
        sponsor(&w, "outsider", "owner-login", &tier.id, 0, false)
            .await
            .unwrap_err(),
        "ALREADY_EXISTS",
        "only one monthly sponsorship per maintainer"
    );
    // Monthly totals count only monthly money; the sponsor counts once.
    let profile =
        w.s.get_sponsor_profile(req(
            &w.tok("reader"),
            GetSponsorProfileRequest {
                login: "owner-login".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (profile.monthly_total_cents, profile.sponsor_count),
        (500, 1)
    );
    // After the period, one-time sponsorships end; the monthly one carries on.
    w.clock.advance(31 * DAY);
    let ended = w
        .env
        .forge
        .community
        .sponsors
        .maintain(&Cx::system())
        .await
        .unwrap();
    assert_eq!(ended, 2);
    let state = |id: String| {
        let w = &w;
        async move {
            w.s.get_sponsorship(req(
                &w.tok("outsider"),
                GetSponsorshipRequest { sponsorship_id: id },
            ))
            .await
            .unwrap()
            .into_inner()
            .state
        }
    };
    assert_eq!(state(first.id).await, SponsorshipState::Ended as i32);
    assert_eq!(state(second.id).await, SponsorshipState::Ended as i32);
    assert_ne!(state(monthly.id).await, SponsorshipState::Ended as i32);
}
