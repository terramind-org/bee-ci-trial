//! First-time contributor handling.
//!
//! A first-time contributor is a principal without collaborator standing and without a prior
//! contribution in the repo. For their issues and CRs the write path asks
//! [`Moderation::check_write`](super::Moderation::check_write), which answers with a
//! [`WriteDecision`](super::WriteDecision) instead of an error: a label to attach, whether the
//! CR waits for maintainer approval before review starts, and whether workflow runs need
//! approval.
//!
//! Settings exist per org and per repo. A repo's settings override the org's; the first time a
//! repo sets anything it starts from the org's effective settings, so unset fields keep their
//! inherited values. With nothing set anywhere the defaults apply: label first contributions
//! with `first-time-contributor`, approve runs of first-time contributors, hold nothing.
//!
//! Agents count as first-time unless their owner (or, with `treat_new_agents_as_first_time`,
//! the agent itself) has a prior contribution.

use bee_proto::v1::{
    ModerationFirstTimeContributorSettings, ModerationRunApproval, RepoRef, repo_ref,
};
use serde_json::json;

use super::tables::FIRST_TIME;
use super::{Change, Moderation, SURFACE, record};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::store::{Row, Tx};

/// The label first contributions get unless the settings name another.
pub const DEFAULT_LABEL: &str = "first-time-contributor";

/// Whose settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsTarget {
    /// One repo.
    Repo {
        /// Repo id.
        repo_id: String,
        /// Its org id.
        org_id: String,
    },
    /// An org (the org id).
    Org(String),
}

impl SettingsTarget {
    fn key(&self) -> String {
        match self {
            SettingsTarget::Repo { repo_id, .. } => format!("repo:{repo_id}"),
            SettingsTarget::Org(o) => format!("org:{o}"),
        }
    }
}

/// A partial update; `None` fields keep their value.
#[derive(Clone, Debug, Default)]
pub struct SettingsPatch {
    /// When workflow runs of first-time contributors need approval.
    pub run_approval: Option<ModerationRunApproval>,
    /// Whether to label first contributions.
    pub label_first_contributions: Option<bool>,
    /// The label.
    pub label: Option<String>,
    /// Whether CRs wait for maintainer approval.
    pub hold_change_requests: Option<bool>,
    /// Whether an agent with no contribution of its own counts as first-time.
    pub treat_new_agents_as_first_time: Option<bool>,
}

/// The settings that apply with nothing configured.
pub fn default_settings() -> ModerationFirstTimeContributorSettings {
    ModerationFirstTimeContributorSettings {
        repo: None,
        org: String::new(),
        run_approval: ModerationRunApproval::FirstTime as i32,
        label_first_contributions: true,
        label: DEFAULT_LABEL.to_string(),
        hold_change_requests: false,
        treat_new_agents_as_first_time: false,
    }
}

/// Effective settings of a repo read inside a transaction: the repo's row, else the org's,
/// else the defaults (two point reads).
pub(crate) async fn effective(
    tx: &mut Tx,
    repo_id: &str,
    org_id: &str,
) -> Result<ModerationFirstTimeContributorSettings> {
    if let Some((s, _)) = tx
        .get_msg::<ModerationFirstTimeContributorSettings>(
            &FIRST_TIME,
            &format!("repo:{repo_id}"),
            "s",
        )
        .await?
    {
        return Ok(s);
    }
    if let Some((s, _)) = tx
        .get_msg::<ModerationFirstTimeContributorSettings>(
            &FIRST_TIME,
            &format!("org:{org_id}"),
            "s",
        )
        .await?
    {
        return Ok(s);
    }
    Ok(default_settings())
}

impl Moderation {
    /// The settings in force for a repo (its own, else its org's, else the defaults).
    pub async fn first_time_settings(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
    ) -> Result<ModerationFirstTimeContributorSettings> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut s = effective(&mut tx, repo_id, org_id).await?;
        s.repo = Some(RepoRef {
            repo: Some(repo_ref::Repo::Id(repo_id.to_string())),
        });
        Ok(s)
    }

    /// The settings of an org (the defaults when it set none).
    pub async fn org_first_time_settings(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<ModerationFirstTimeContributorSettings> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut s = tx
            .get_msg::<ModerationFirstTimeContributorSettings>(
                &FIRST_TIME,
                &format!("org:{org_id}"),
                "s",
            )
            .await?
            .map(|(s, _)| s)
            .unwrap_or_else(default_settings);
        s.repo = None;
        s.org = org_id.to_string();
        Ok(s)
    }

    /// Applies a patch to a repo's or org's settings and returns the result.
    pub async fn set_first_time_settings(
        &self,
        cx: &Cx,
        target: &SettingsTarget,
        patch: SettingsPatch,
    ) -> Result<ModerationFirstTimeContributorSettings> {
        if let Some(l) = &patch.label
            && (l.is_empty() || l.len() > 50)
        {
            return Err(ForgeError::invalid(
                "bad_label",
                "label must be 1-50 characters",
            ));
        }
        if patch.run_approval == Some(ModerationRunApproval::Unspecified) {
            return Err(ForgeError::invalid(
                "bad_run_approval",
                "run_approval must name a policy",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (org_id, repo_id) = match target {
            SettingsTarget::Repo { repo_id, org_id } => (org_id.clone(), repo_id.clone()),
            SettingsTarget::Org(o) => (o.clone(), String::new()),
        };
        let own = tx
            .get_msg::<ModerationFirstTimeContributorSettings>(&FIRST_TIME, &target.key(), "s")
            .await?;
        let mut s = match &own {
            Some((s, _)) => s.clone(),
            None => effective(&mut tx, &repo_id, &org_id).await?,
        };
        let before = json!({
            "run_approval": s.run_approval,
            "hold_change_requests": s.hold_change_requests,
        });
        if let Some(v) = patch.run_approval {
            s.run_approval = v as i32;
        }
        if let Some(v) = patch.label_first_contributions {
            s.label_first_contributions = v;
        }
        if let Some(v) = patch.label {
            s.label = v;
        }
        if let Some(v) = patch.hold_change_requests {
            s.hold_change_requests = v;
        }
        if let Some(v) = patch.treat_new_agents_as_first_time {
            s.treat_new_agents_as_first_time = v;
        }
        match target {
            SettingsTarget::Repo { repo_id, .. } => {
                s.repo = Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo_id.clone())),
                });
                s.org = String::new();
            }
            SettingsTarget::Org(o) => {
                s.repo = None;
                s.org = o.clone();
            }
        }
        let mut row = Row::new(target.key(), "s").msg(&s);
        if let Some((_, old)) = &own {
            row.version = old.version;
            tx.update(&FIRST_TIME, &row).await?;
        } else {
            tx.insert(&FIRST_TIME, &row).await?;
        }
        record(
            &mut tx,
            Change {
                kind: "moderation.first_time_settings_set",
                action: "moderation.first_time_settings_set",
                org: org_id,
                repo: repo_id,
                subject: None,
                before: Some(before),
                after: json!({
                    "scope": target.key(),
                    "run_approval": s.run_approval,
                    "label_first_contributions": s.label_first_contributions,
                    "label": s.label,
                    "hold_change_requests": s.hold_change_requests,
                    "treat_new_agents_as_first_time": s.treat_new_agents_as_first_time,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }
}
