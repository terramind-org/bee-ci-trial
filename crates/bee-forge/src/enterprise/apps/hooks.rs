//! Per-app webhooks, kept in sync over the core webhook machinery.
//!
//! An app's webhook URL, secret and event list apply to every installation. For each
//! *active* installation the apps module owns webhook rows in the webhooks surface, so the
//! existing dispatcher, signing (`X-Bee-Signature-256` with the app's webhook secret),
//! retry and delivery log are reused as they are:
//!
//! - an `ALL` installation owns one org hook (the org log carries every repo's events);
//! - a `SELECTED` installation owns one repo hook per selected repo, so events of repos the
//!   app was not given are never delivered to it;
//! - a suspended or deleted installation owns none; an app without a URL, with the webhook
//!   switched off or with no subscribed events owns none either.
//!
//! The hook rows an installation owns are tracked in `forge_apps_hook_links`
//! (`log key -> hook id + settings fingerprint`); [`Apps::resync_installation_hooks`] diffs
//! the wanted rows against the tracked ones and creates, updates or deletes accordingly, so
//! it is idempotent and safe to call from a maintenance pass if a previous sync was cut
//! short. Org and repo hooks are capped per owner by the webhook module (20), which also
//! bounds how many apps can subscribe on one org.

use std::collections::BTreeMap;

use bee_proto::v1::{
    AppInstallationTargetKind, AppRepositorySelection, ErrorReason, WebhookFormat,
};

use super::{
    APPS, AppRec, Apps, HOOK_LINKS, HookLink, INSTALL_REPOS, INSTALLS, InstallRec, SURFACE,
};
use crate::cx::Cx;
use crate::enterprise::common::{scan_all, sha256_hex};
use crate::error::Result;
use crate::events::webhooks::{HookOwner, HookPatch, NewHook};
use crate::store::{Key, Row};

impl Apps {
    /// Brings the webhook rows of one installation in line with the app's settings and the
    /// installation's state.
    pub async fn resync_installation_hooks(&self, cx: &Cx, installation_id: &str) -> Result<()> {
        let (inst_rec, app_rec, links, repos) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let inst = tx
                .get_msg::<InstallRec>(&INSTALLS, "", installation_id)
                .await?
                .map(|(r, _)| r);
            let links = scan_all::<HookLink>(&mut tx, &HOOK_LINKS, installation_id, None).await?;
            let (app, repos) = match &inst {
                Some(rec) => {
                    let app_id = rec
                        .inst
                        .as_ref()
                        .map(|i| i.app_id.clone())
                        .unwrap_or_default();
                    let app = tx
                        .get_msg::<AppRec>(&APPS, "", &app_id)
                        .await?
                        .map(|(a, _)| a);
                    let repos = tx
                        .scan(
                            &INSTALL_REPOS,
                            &crate::store::Scan::scope(installation_id, crate::store::MAX_SCAN),
                        )
                        .await?
                        .into_iter()
                        .map(|r| r.id)
                        .collect::<Vec<_>>();
                    (app, repos)
                }
                None => (None, Vec::new()),
            };
            (inst, app, links, repos)
        };

        // The hooks this installation should own: log key -> owner.
        let mut want: BTreeMap<String, HookOwner> = BTreeMap::new();
        let mut settings = None;
        if let (Some(rec), Some(app_rec)) = (&inst_rec, &app_rec)
            && let (Some(inst), Some(app)) = (&rec.inst, &app_rec.app)
            && !inst.suspended
            && app.webhook_active
            && !app.webhook_url.is_empty()
            && !app.events.is_empty()
            && inst.target_kind == AppInstallationTargetKind::Org as i32
        {
            let org = &rec.target_id;
            if inst.repository_selection == AppRepositorySelection::All as i32 {
                let o = HookOwner::org(org);
                want.insert(o.log.clone(), o);
            } else {
                for r in repos {
                    let o = HookOwner::repo(&r, org);
                    want.insert(o.log.clone(), o);
                }
            }
            let fp = sha256_hex(&format!(
                "{}|{}|{}",
                app.webhook_url,
                app.events.join(","),
                app_rec.webhook_secret
            ));
            settings = Some((
                app.webhook_url.clone(),
                app.events.clone(),
                app_rec.webhook_secret.clone(),
                fp,
            ));
        }

        let mut existing: BTreeMap<String, HookLink> =
            links.into_iter().map(|(l, row)| (row.id, l)).collect();

        // Remove hooks that are no longer wanted.
        let stale: Vec<String> = existing
            .keys()
            .filter(|k| !want.contains_key(*k))
            .cloned()
            .collect();
        for key in stale {
            if let Some(link) = existing.remove(&key) {
                self.delete_hooks(cx, vec![link]).await;
            }
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.delete(&HOOK_LINKS, installation_id, &key).await?;
            tx.commit().await?;
        }

        let Some((url, events, secret, fp)) = settings else {
            return Ok(());
        };
        for (key, owner) in want {
            let mut link = existing.remove(&key);
            if let Some(l) = &link {
                if l.fingerprint == fp {
                    continue;
                }
                let patch = HookPatch {
                    url: Some(url.clone()),
                    kinds: Some(events.clone()),
                    secret: Some(secret.clone()),
                    active: Some(true),
                    ..HookPatch::default()
                };
                match self
                    .forge
                    .webhooks
                    .update(cx, &owner, &l.hook_id, patch)
                    .await
                {
                    Ok(_) => {}
                    Err(e) if e.reason == ErrorReason::NotFound => link = None,
                    Err(e) => return Err(e),
                }
            }
            let hook_id = match &link {
                Some(l) => l.hook_id.clone(),
                None => {
                    self.forge
                        .webhooks
                        .create(
                            cx,
                            &owner,
                            NewHook {
                                url: url.clone(),
                                kinds: events.clone(),
                                secret: secret.clone(),
                                format: WebhookFormat::Bee,
                                active: true,
                                insecure_ssl: false,
                            },
                        )
                        .await?
                        .id
                }
            };
            let record = HookLink {
                hook_id,
                org_id: owner.org_id.clone(),
                repo_id: owner.repo_id.clone(),
                fingerprint: fp.clone(),
            };
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.put(&HOOK_LINKS, &Row::new(installation_id, key).msg(&record))
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// Resyncs the hooks of every installation of an app.
    pub(crate) async fn resync_app_hooks(&self, cx: &Cx, app_id: &str) -> Result<()> {
        let ids: Vec<String> = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &INSTALLS,
                &crate::store::Scan::scope("", crate::store::MAX_SCAN)
                    .key(Key::K1(app_id.to_string())),
            )
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect()
        };
        for id in ids {
            self.resync_installation_hooks(cx, &id).await?;
        }
        Ok(())
    }
}
