//! Custom domains, DNS health, HTTPS certificates and org domain verification.
//!
//! - **Claims.** A custom domain belongs to one site ([`super::HOSTS`] is unique per host). An
//!   org that verified a domain (or a parent of it) with a DNS TXT record excludes every other
//!   org from serving it.
//! - **Verification.** `AddPagesVerifiedDomain` returns the TXT record
//!   `_beehive-pages-challenge-<org>.<domain>` and a value to publish; `VerifyPagesDomain`
//!   looks it up through the [`super::ports::DnsResolver`] port. A failed lookup is a `FAILED`
//!   verification, not an error, and can be retried.
//! - **Certificates.** Once DNS points at Pages (CNAME to `<org>.<pages domain>` or A records at
//!   the Pages addresses) and no CAA record excludes the ACME issuer, the
//!   [`super::ports::AcmeIssuer`] port issues a certificate; the record here carries state,
//!   domains and dates. Maintenance renews certificates 30 days before expiry.

use std::collections::BTreeSet;

use bee_proto::v1::{
    PagesCertificate, PagesCertificateState, PagesDomainHealth, PagesDomainVerification,
    PagesDomainVerificationState, PagesHealthCheck, Repo, SubjectKind,
};
use serde_json::json;

use super::model::{HostRec, SiteRec, VerificationRec};
use super::ports::DnsAnswer;
use super::site::announce;
use super::{HOSTS, Pages, SITES, SURFACE, VERIFICATIONS};
use crate::community::common::paged;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::packages::hash::sha256_hex;
use crate::store::{Key, Row, Scan, Tx};

/// Normalizes a host name: lower case, no trailing dot, valid labels, at least two of them,
/// not an IP address.
pub fn normalize_domain(d: &str) -> Result<String> {
    let d = d.trim().trim_end_matches('.').to_lowercase();
    let bad = |why: &str| {
        ForgeError::invalid("bad_domain", format!("`{d}` is not a valid domain: {why}"))
    };
    if d.is_empty() || d.len() > 253 {
        return Err(bad("1 to 253 characters"));
    }
    let labels: Vec<&str> = d.split('.').collect();
    if labels.len() < 2 {
        return Err(bad("a domain has at least two labels"));
    }
    for l in &labels {
        if l.is_empty()
            || l.len() > 63
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(bad("labels are 1 to 63 letters, digits or `-`"));
        }
    }
    if labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit())) {
        return Err(bad("an IP address is not a domain"));
    }
    Ok(d)
}

fn parents(d: &str) -> Vec<String> {
    let labels: Vec<&str> = d.split('.').collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

fn caa_allows(records: &[String], issuer: &str) -> bool {
    let mut any_issue = false;
    for r in records {
        let t = r.trim();
        let t = t
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start();
        let Some(rest) = t.strip_prefix("issue ") else {
            continue;
        };
        if t.starts_with("issuewild") {
            continue;
        }
        any_issue = true;
        let v = rest.trim().trim_matches('"');
        let v = v.split(';').next().unwrap_or("").trim();
        if v == issuer {
            return true;
        }
    }
    !any_issue
}

fn vstate(n: i32) -> PagesDomainVerificationState {
    PagesDomainVerificationState::try_from(n).unwrap_or(PagesDomainVerificationState::Pending)
}

impl Pages {
    fn points_to_pages(&self, a: &DnsAnswer) -> bool {
        let suffix = format!(".{}", self.config.pages_domain);
        a.cname
            .as_ref()
            .is_some_and(|c| c.ends_with(&suffix) || *c == self.config.pages_domain)
            || a.a.iter().any(|ip| self.config.pages_ips.contains(ip))
    }

    /// DNS findings for one host.
    pub async fn domain_health(&self, host: &str) -> Result<PagesDomainHealth> {
        let a = self.ports.dns.lookup(host).await?;
        let resolves = a.cname.is_some() || !a.a.is_empty() || !a.aaaa.is_empty();
        let is_apex = host.split('.').count() == 2;
        let points = self.points_to_pages(&a);
        let caa_conflict = !a.caa.is_empty() && !caa_allows(&a.caa, &self.config.acme_caa_issuer);
        let mut problems = Vec::new();
        if !resolves {
            problems.push(format!("{host} does not resolve; add a DNS record"));
        } else if !points {
            problems.push(format!(
                "{host} does not point at Pages; use a CNAME to <org>.{} or A records {}",
                self.config.pages_domain,
                self.config.pages_ips.join(", ")
            ));
        }
        if is_apex && a.cname.is_some() {
            problems.push("an apex domain cannot have a CNAME record; use A records".to_string());
        }
        if caa_conflict {
            problems.push(format!(
                "a CAA record does not allow {} to issue certificates",
                self.config.acme_caa_issuer
            ));
        }
        Ok(PagesDomainHealth {
            host: host.to_string(),
            dns_resolves: resolves,
            is_apex,
            points_to_pages: points,
            has_cname_record: a.cname.is_some(),
            cname_target: a.cname.clone().unwrap_or_default(),
            a_records: a.a.clone(),
            aaaa_records: a.aaaa.clone(),
            is_proxied: resolves && !points,
            has_caa_conflict: caa_conflict,
            https_eligible: resolves && points && !caa_conflict,
            problems,
        })
    }

    /// DNS health of the site's custom domain and its `www`/apex counterpart.
    pub async fn health_check(&self, cx: &Cx, repo: &Repo) -> Result<PagesHealthCheck> {
        let s = self.site(cx, repo).await?;
        if s.custom_domain.is_empty() {
            return Err(ForgeError::invalid_state("the site has no custom domain"));
        }
        let alt = match s.custom_domain.strip_prefix("www.") {
            Some(apex) => apex.to_string(),
            None => format!("www.{}", s.custom_domain),
        };
        Ok(PagesHealthCheck {
            domain: Some(self.domain_health(&s.custom_domain).await?),
            alt_domain: Some(self.domain_health(&alt).await?),
            checked_at: Some(ts(self.db.now_ms())),
        })
    }

    async fn foreign_verification(tx: &mut Tx, org_id: &str, domain: &str) -> Result<bool> {
        for cand in parents(domain) {
            let rows = tx
                .scan(
                    &VERIFICATIONS,
                    &Scan {
                        key: Some(Key::K1(cand)),
                        limit: 100,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                let v: VerificationRec = r.decode()?;
                if v.org_id != org_id && vstate(v.state) == PagesDomainVerificationState::Verified {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Sets (or, when empty, clears) the site's custom domain and starts certificate issuance.
    /// `ALREADY_EXISTS` when another site serves the domain, `PERMISSION_DENIED` when another
    /// org verified it.
    pub async fn set_custom_domain(&self, cx: &Cx, repo: &Repo, domain: &str) -> Result<SiteRec> {
        let new = if domain.trim().is_empty() {
            String::new()
        } else {
            normalize_domain(domain)?
        };
        if !new.is_empty()
            && (new == self.config.pages_domain
                || new.ends_with(&format!(".{}", self.config.pages_domain)))
        {
            return Err(ForgeError::invalid(
                "bad_domain",
                "that domain is served by Pages itself",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut s, row)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        if !new.is_empty() {
            if let Some(h) = tx.get(&HOSTS, "", &new).await? {
                let h: HostRec = h.decode()?;
                if h.repo_id != repo.id {
                    return Err(ForgeError::already_exists(format!(
                        "domain {new} (served by another site)"
                    )));
                }
            }
            if Self::foreign_verification(&mut tx, &repo.org_id, &new).await? {
                return Err(ForgeError::permission_denied(format!(
                    "{new} is verified by another organization"
                )));
            }
        }
        if s.custom_domain == new {
            return Ok(s);
        }
        if !s.custom_domain.is_empty() {
            tx.delete(&HOSTS, "", &s.custom_domain).await?;
        }
        s.custom_domain = new.clone();
        s.enforce_https = false;
        s.cert_domains.clear();
        s.cert_error.clear();
        s.cert_issued_ms = 0;
        s.cert_expires_ms = 0;
        s.cert_ref.clear();
        s.cert_state = if new.is_empty() {
            PagesCertificateState::None
        } else {
            PagesCertificateState::Requested
        } as i32;
        s.updated_ms = tx.now_ms();
        if !new.is_empty() {
            tx.insert(
                &HOSTS,
                &Row::new("", &new).k1(&repo.id).msg(&HostRec {
                    repo_id: repo.id.clone(),
                    kind: "custom".into(),
                    deployment_id: String::new(),
                }),
            )
            .await
            .map_err(|error| {
                if error.reason == bee_proto::v1::ErrorReason::AlreadyExists {
                    ForgeError::already_exists(format!("domain {new} (served by another site)"))
                } else {
                    error
                }
            })?;
        }
        tx.update(
            &SITES,
            &Row::new(&repo.org_id, &repo.id)
                .msg(&s)
                .version(row.version),
        )
        .await?;
        announce(
            &mut tx,
            "pages.domain_set",
            "pages.domain.set",
            &repo.org_id,
            &repo.id,
            &repo.id,
            json!({"repo": repo.full_name, "domain": new}),
        )
        .await?;
        tx.commit().await?;
        if new.is_empty() {
            return Ok(s);
        }
        // Issue now if DNS is ready; otherwise the certificate stays REQUESTED.
        match self.issue_certificate(cx, repo, false).await {
            Ok(c) => Ok(c),
            Err(_) => self.site(cx, repo).await,
        }
    }

    /// Requests a certificate for the custom domain (and its `www`/apex counterpart when that
    /// points at Pages too). With `strict`, `INVALID_STATE` when DNS is not ready; otherwise a
    /// not-ready domain is left `REQUESTED`.
    async fn issue_certificate(&self, cx: &Cx, repo: &Repo, strict: bool) -> Result<SiteRec> {
        let s = self.site(cx, repo).await?;
        if s.custom_domain.is_empty() {
            return Err(ForgeError::invalid_state("the site has no custom domain"));
        }
        let main = self.domain_health(&s.custom_domain).await?;
        if !main.https_eligible {
            if strict {
                return Err(ForgeError::invalid_state(format!(
                    "DNS of {} is not ready: {}",
                    s.custom_domain,
                    main.problems.join("; ")
                )));
            }
            return Ok(s);
        }
        let alt = match s.custom_domain.strip_prefix("www.") {
            Some(apex) => apex.to_string(),
            None => format!("www.{}", s.custom_domain),
        };
        let mut domains = vec![s.custom_domain.clone()];
        if self
            .domain_health(&alt)
            .await
            .is_ok_and(|h| h.https_eligible)
        {
            domains.push(alt);
        }
        let result = self.ports.acme.issue(&domains).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut cur, row)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Err(ForgeError::not_found(format!(
                "Pages site of {}",
                repo.full_name
            )));
        };
        if cur.custom_domain != s.custom_domain {
            return Ok(cur);
        }
        let (kind, audit, payload) = match result {
            Ok(c) => {
                cur.cert_state = PagesCertificateState::Issued as i32;
                cur.cert_domains = c.domains.clone();
                cur.cert_error.clear();
                cur.cert_issued_ms = c.issued_ms;
                cur.cert_expires_ms = c.expires_ms;
                cur.cert_ref = c.id.clone();
                (
                    "pages.certificate_issued",
                    "pages.certificate.issue",
                    json!({"repo": repo.full_name, "domains": c.domains, "expires_ms": c.expires_ms}),
                )
            }
            Err(e) => {
                cur.cert_state = PagesCertificateState::Errored as i32;
                cur.cert_error = e.message.clone();
                (
                    "pages.certificate_failed",
                    "pages.certificate.fail",
                    json!({"repo": repo.full_name, "error": e.message}),
                )
            }
        };
        cur.updated_ms = tx.now_ms();
        tx.update(
            &SITES,
            &Row::new(&repo.org_id, &repo.id)
                .msg(&cur)
                .version(row.version),
        )
        .await?;
        announce(
            &mut tx,
            kind,
            audit,
            &repo.org_id,
            &repo.id,
            &repo.id,
            payload,
        )
        .await?;
        tx.commit().await?;
        Ok(cur)
    }

    /// Re-requests the certificate. `INVALID_STATE` when DNS is not ready or no custom domain
    /// is set; an ACME failure is recorded and returned as an `ERRORED` certificate.
    pub async fn request_certificate(&self, cx: &Cx, repo: &Repo) -> Result<PagesCertificate> {
        let s = self.issue_certificate(cx, repo, true).await?;
        Ok(self.site_proto(&s).certificate.unwrap_or_default())
    }

    /// Renews certificates close to expiry and marks expired ones.
    pub async fn renew_certificates(&self, cx: &Cx) -> Result<usize> {
        let now = self.db.now_ms();
        let window = u64::from(self.config.renew_before_days) * 86_400_000;
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &SITES,
                &Scan {
                    limit: 10_000,
                    ..Scan::default()
                },
            )
            .await?
        };
        let mut n = 0;
        for r in rows {
            let s: SiteRec = r.decode()?;
            if s.cert_state != PagesCertificateState::Issued as i32 {
                continue;
            }
            let Ok(repo) = self.orgs.repo_any(cx, &s.repo_id).await else {
                continue;
            };
            if s.cert_expires_ms <= now + window
                && self.issue_certificate(cx, &repo, false).await.is_ok()
            {
                let cur = self.site(cx, &repo).await?;
                if cur.cert_state == PagesCertificateState::Issued as i32
                    && cur.cert_expires_ms > s.cert_expires_ms
                {
                    n += 1;
                    continue;
                }
            }
            if s.cert_expires_ms <= now {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                if let Some((mut cur, row)) =
                    Self::load_site(&mut tx, &s.org_id, &s.repo_id).await?
                {
                    cur.cert_state = PagesCertificateState::Expired as i32;
                    cur.updated_ms = now;
                    tx.update(
                        &SITES,
                        &Row::new(&s.org_id, &s.repo_id)
                            .msg(&cur)
                            .version(row.version),
                    )
                    .await?;
                    tx.commit().await?;
                }
            }
        }
        Ok(n)
    }

    // ---------- org domain verification ----------

    fn challenge(org_name: &str, domain: &str) -> String {
        format!("_beehive-pages-challenge-{org_name}.{domain}")
    }

    /// The `bee.v1.PagesDomainVerification` of a record.
    pub fn verification_proto(&self, v: &VerificationRec) -> PagesDomainVerification {
        PagesDomainVerification {
            id: v.id.clone(),
            org: v.org_name.clone(),
            domain: v.domain.clone(),
            state: v.state,
            txt_record_name: Self::challenge(&v.org_name, &v.domain),
            txt_record_value: v.txt_value.clone(),
            verified_at: (v.verified_ms != 0).then(|| ts(v.verified_ms)),
            created_at: Some(ts(v.created_ms)),
        }
    }

    /// Starts verification of `domain` for the org. Idempotent per org and domain;
    /// `ALREADY_EXISTS` when another org already verified it.
    pub async fn add_verified_domain(
        &self,
        cx: &Cx,
        org_id: &str,
        domain: &str,
    ) -> Result<VerificationRec> {
        let domain = normalize_domain(domain)?;
        let org = self.orgs.org_any(cx, org_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mine = tx
            .scan(
                &VERIFICATIONS,
                &Scan::scope(org_id, 1).key(Key::K1(domain.clone())),
            )
            .await?;
        if let Some(r) = mine.first() {
            return r.decode();
        }
        if Self::foreign_verification(&mut tx, org_id, &domain).await? {
            return Err(ForgeError::already_exists(format!(
                "domain {domain} (verified by another organization)"
            )));
        }
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let v = VerificationRec {
            txt_value: format!(
                "beehive-pages-challenge={}",
                &sha256_hex(format!("{id}:{org_id}:{domain}").as_bytes())[..32]
            ),
            id,
            org_id: org_id.to_string(),
            org_name: org.name,
            domain,
            state: PagesDomainVerificationState::Pending as i32,
            verified_ms: 0,
            created_ms: now,
        };
        tx.insert(
            &VERIFICATIONS,
            &Row::new(org_id, &v.id).k1(&v.domain).msg(&v),
        )
        .await?;
        tx.audit(crate::community::common::draft(
            "pages.domain.verification.add",
            org_id,
            "",
            SubjectKind::Org,
            &v.id,
            &json!({"domain": v.domain}),
        ))
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// An org's verifications.
    pub async fn verified_domains(
        &self,
        cx: &Cx,
        org_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<VerificationRec>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = paged(
            &mut tx,
            &VERIFICATIONS,
            Scan::scope(org_id, limit),
            token,
            limit,
        )
        .await?;
        Ok((
            rows.iter()
                .map(|r| r.decode())
                .collect::<Result<Vec<_>>>()?,
            next,
        ))
    }

    /// Checks the TXT record now. A failed lookup is not an error: the verification becomes (or
    /// stays) `FAILED` and can be retried.
    pub async fn verify_domain(&self, cx: &Cx, org_id: &str, id: &str) -> Result<VerificationRec> {
        let mut v = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get_msg::<VerificationRec>(&VERIFICATIONS, org_id, id)
                .await?
                .map(|(v, _)| v)
                .ok_or_else(|| ForgeError::not_found(format!("domain verification {id}")))?
        };
        let name = Self::challenge(&v.org_name, &v.domain);
        let found = self
            .ports
            .dns
            .txt(&name)
            .await
            .map(|t| t.iter().any(|x| x.trim().trim_matches('"') == v.txt_value))
            .unwrap_or(false);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(row) = tx.get(&VERIFICATIONS, org_id, id).await? else {
            return Err(ForgeError::not_found(format!("domain verification {id}")));
        };
        if found && Self::foreign_verification(&mut tx, org_id, &v.domain).await? {
            return Err(ForgeError::already_exists(format!(
                "domain {} (verified by another organization)",
                v.domain
            )));
        }
        let now = tx.now_ms();
        let was = vstate(v.state);
        v.state = if found {
            PagesDomainVerificationState::Verified
        } else {
            PagesDomainVerificationState::Failed
        } as i32;
        if found && was != PagesDomainVerificationState::Verified {
            v.verified_ms = now;
        }
        tx.update(
            &VERIFICATIONS,
            &Row::new(org_id, id)
                .k1(&v.domain)
                .msg(&v)
                .version(row.version),
        )
        .await?;
        if found && was != PagesDomainVerificationState::Verified {
            let payload = json!({"org": v.org_name, "domain": v.domain});
            let subject = bee_proto::v1::SubjectRef {
                kind: SubjectKind::Org as i32,
                repo: None,
                id: org_id.to_string(),
            };
            crate::community::common::emit(
                &mut tx,
                "pages.domain_verified",
                org_id,
                "",
                subject,
                payload.clone(),
            )
            .await?;
            tx.audit(crate::community::common::draft(
                "pages.domain.verify",
                org_id,
                "",
                SubjectKind::Org,
                id,
                &payload,
            ))
            .await?;
        }
        tx.commit().await?;
        Ok(v)
    }

    /// Removes a verification.
    pub async fn delete_verified_domain(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&VERIFICATIONS, org_id, id).await? {
            return Err(ForgeError::not_found(format!("domain verification {id}")));
        }
        tx.audit(crate::community::common::draft(
            "pages.domain.verification.delete",
            org_id,
            "",
            SubjectKind::Org,
            id,
            &json!({}),
        ))
        .await?;
        tx.commit().await
    }

    /// Whether `org_id` holds a verified claim covering `domain`.
    pub async fn is_verified_for(&self, cx: &Cx, org_id: &str, domain: &str) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut seen = BTreeSet::new();
        for cand in parents(domain) {
            if !seen.insert(cand.clone()) {
                continue;
            }
            for r in tx
                .scan(&VERIFICATIONS, &Scan::scope(org_id, 10).key(Key::K1(cand)))
                .await?
            {
                let v: VerificationRec = r.decode()?;
                if vstate(v.state) == PagesDomainVerificationState::Verified {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_normalize() {
        assert_eq!(
            normalize_domain("Docs.Example.COM.").unwrap(),
            "docs.example.com"
        );
        assert!(normalize_domain("localhost").is_err());
        assert!(normalize_domain("a..b").is_err());
        assert!(normalize_domain("-a.com").is_err());
        assert!(normalize_domain("10.0.0.1").is_err());
        assert_eq!(
            parents("a.b.example.com"),
            ["a.b.example.com", "b.example.com", "example.com"]
        );
    }

    #[test]
    fn caa_rules() {
        let le = "letsencrypt.org";
        assert!(caa_allows(&[], le));
        assert!(caa_allows(&["0 issue \"letsencrypt.org\"".into()], le));
        assert!(caa_allows(&["issue letsencrypt.org; account=1".into()], le));
        assert!(!caa_allows(&["0 issue \"digicert.com\"".into()], le));
        assert!(!caa_allows(&["issue \";\"".into()], le));
        assert!(caa_allows(&["iodef mailto:a@b.c".into()], le));
    }
}
