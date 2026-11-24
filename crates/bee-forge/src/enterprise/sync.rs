//! IdP team sync: makes the membership of teams linked to IdP groups follow the groups.
//!
//! A team linked to groups (`TeamService.LinkTeamExternalGroup`) has the members of the union
//! of its groups. Group membership comes from SCIM Groups (`provider = "scim"`, the SCIM group
//! id) and from the groups attribute of the last SAML assertion of each linked identity
//! (`provider = "saml"`, the IdP's group id or name). Changes to a group, a user or a link call
//! [`resync_groups`] / [`resync_team`], which adds missing members, removes members that came
//! from a group and no longer belong to it, and leaves manually added members alone. SCIM
//! members who are not yet org members are added to the org as members, since the IdP is the
//! authority for them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::{Role, ScimIdentity};

use super::SURFACE;
use super::accounts::Accounts;
use super::common::principal_of;
use super::scim::{GroupRec, SCIM_GROUPS, SCIM_USERS};
use super::sso::Sso;
use super::teams::{SyncReport, Teams};
use crate::cx::Cx;
use crate::error::Result;
use crate::forge::Forge;

/// The scope keys whose IdP data applies to an org: the org's own and its enterprise's.
pub async fn scope_keys_of_org(forge: &Forge, cx: &Cx, org_id: &str) -> Result<Vec<String>> {
    let mut tx = forge.db.begin(SURFACE, cx).await?;
    let mut keys = vec![format!("org:{org_id}")];
    if let Some(e) = Accounts::enterprise_of_org_in(&mut tx, org_id).await? {
        keys.push(format!("enterprise:{}", e.id));
    }
    Ok(keys)
}

/// Re-syncs every team linked to any of the groups.
pub async fn resync_groups(
    forge: &Arc<Forge>,
    cx: &Cx,
    provider: &str,
    group_ids: &BTreeSet<String>,
) -> Result<Vec<(String, SyncReport)>> {
    let teams = Teams::new(forge.clone());
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for g in group_ids {
        for (org_id, team_id) in teams.teams_linked_to(cx, provider, g).await? {
            if seen.insert(team_id.clone()) {
                let r = resync_team(forge, &teams, cx, &org_id, &team_id).await?;
                out.push((team_id, r));
            }
        }
    }
    Ok(out)
}

/// Re-syncs one team from all of its linked groups.
pub async fn resync_team(
    forge: &Arc<Forge>,
    teams: &Teams,
    cx: &Cx,
    org_id: &str,
    team_id: &str,
) -> Result<SyncReport> {
    let org = forge.orgs.org_any(cx, org_id).await?;
    let links = teams.group_links(cx, team_id).await?;
    if links.is_empty() {
        return Ok(SyncReport::default());
    }
    let keys = scope_keys_of_org(forge, cx, org_id).await?;
    let mut desired: BTreeMap<String, String> = BTreeMap::new();
    let mut from_scim: BTreeSet<String> = BTreeSet::new();
    {
        let mut tx = forge.db.begin(SURFACE, cx).await?;
        for (provider, gid) in &links {
            let tag = format!("{provider}:{gid}");
            match provider.as_str() {
                "scim" => {
                    for k in &keys {
                        let Some((g, _)) = tx.get_msg::<GroupRec>(&SCIM_GROUPS, k, gid).await?
                        else {
                            continue;
                        };
                        for uid in &g.members {
                            if let Some((u, _)) =
                                tx.get_msg::<ScimIdentity>(&SCIM_USERS, k, uid).await?
                                && u.active
                                && let Some(p) = u.principal
                            {
                                from_scim.insert(p.id.clone());
                                desired.entry(p.id).or_insert_with(|| tag.clone());
                            }
                        }
                    }
                }
                "saml" => {
                    for p in Sso::members_of_group(&mut tx, &keys, gid).await? {
                        desired.entry(p).or_insert_with(|| tag.clone());
                    }
                }
                _ => {}
            }
        }
    }
    for p in &from_scim {
        if forge.orgs.member(cx, org_id, p).await?.is_none() {
            let principal = principal_of(forge, p).await;
            forge
                .orgs
                .set_member(cx, org_id, principal, Role::Member)
                .await?;
        }
    }
    teams.sync(cx, &org, team_id, &desired).await
}
