//! Automated security and version updates: the update configuration (the repo's
//! `.beehive/dependabot.yml`, else `.github/dependabot.yml`), update runs and the change
//! requests they open through the changes module.
//!
//! A run plans updates with `bee_deps::update` (security updates for open alerts; version
//! updates from the [`super::ports::Registry`] port), applies the manifest edits, refreshes
//! lockfiles through the [`super::ports::LockRefresher`], commits the result on the default
//! branch tip and opens a change request as the bot principal. Grouping follows the config
//! file's `groups` and the repo's `grouped_security_updates` switch; one fix opens one change
//! request (an update that already has a request, open or not, is not opened again).

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_deps::matching::Finding;
use bee_deps::osv::FileAdvisorySource;
use bee_deps::update::{PlannedUpdate, UpdateGroup, UpdatePlan, apply_plan, plan_security_updates};
use bee_deps::{DependencyGraph, Ecosystem, Relationship, SourceFile};
use bee_proto::v1::{
    ChangeState, DependencyAlertState, DependencyUpdateConfig, DependencyUpdateEntry,
    DependencyUpdateRun, DependencyUpdateRunState, Repo, SetDependencyUpdateConfigRequest,
    SubjectKind, UpdateSecurityConfigurationRequest,
};
use serde_json::Value;

use super::lockfile::TextLockRefresher;
use super::model::{UpdatePrRec, UpdateRunRec, UpdateStateRec};
use super::{
    CONFIG, Change, DEP_ALERTS, Security, UPDATE_PRS, UPDATE_RUNS, page_slice, record, repo_ref_of,
};
use crate::changes::NewChange;
use crate::changes::vcs::NewCommit;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Scan, Surface};

/// Default bot principal that opens update change requests.
pub const DEFAULT_BOT: &str = "bee-update-bot";
/// Default cap on open update change requests per entry.
pub const DEFAULT_OPEN_LIMIT: u32 = 5;

const CONFIG_PATHS: &[&str] = &[
    ".beehive/dependabot.yml",
    ".beehive/dependabot.yaml",
    ".github/dependabot.yml",
    ".github/dependabot.yaml",
];

/// A parsed `groups` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupCfg {
    /// Group name.
    pub name: String,
    /// Package name globs (`*` matches any run of characters); empty matches everything.
    pub patterns: Vec<String>,
    /// `security-updates`, `version-updates` or empty (both).
    pub applies_to: String,
}

/// A parsed update entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryCfg {
    /// Ecosystem (`npm`, `cargo`, `go`, `pip`, ...).
    pub ecosystem: String,
    /// Directory, `/`-rooted.
    pub directory: String,
    /// `daily`, `weekly`, `monthly` or a cron expression.
    pub schedule: String,
    /// Cap on open change requests.
    pub limit: u32,
    /// Groups.
    pub groups: Vec<GroupCfg>,
    /// Target ref (empty = default ref).
    pub target_ref: String,
    /// Dependency name globs never updated.
    pub ignore: Vec<String>,
    /// Labels for change requests.
    pub labels: Vec<String>,
}

fn proto_ecosystem(name: &str) -> Option<&'static str> {
    Some(match name {
        "npm" => "npm",
        "cargo" => "cargo",
        "gomod" | "go" => "go",
        "pip" | "poetry" | "uv" | "pipenv" => "pip",
        "bundler" => "rubygems",
        "maven" | "gradle" => "maven",
        "nuget" => "nuget",
        "composer" => "composer",
        "github-actions" | "actions" => "actions",
        _ => return None,
    })
}

/// Whether `s` is a schedule: daily, weekly, monthly or a five-field cron expression.
pub fn valid_schedule(s: &str) -> bool {
    matches!(s, "daily" | "weekly" | "monthly") || Cron::parse(s).is_some()
}

/// A five-field cron expression (minute, hour, day of month, month, day of week) in UTC.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cron {
    fields: [BTreeSet<u32>; 5],
}

impl Cron {
    fn parse(s: &str) -> Option<Cron> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        let ranges = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)];
        let mut fields: [BTreeSet<u32>; 5] = Default::default();
        for (i, p) in parts.iter().enumerate() {
            let (lo, hi) = ranges[i];
            let mut set = BTreeSet::new();
            for item in p.split(',') {
                let (range, step) = match item.split_once('/') {
                    Some((r, st)) => (r, st.parse::<u32>().ok().filter(|s| *s > 0)?),
                    None => (item, 1),
                };
                let (a, b) = if range == "*" {
                    (lo, hi)
                } else if let Some((a, b)) = range.split_once('-') {
                    (a.parse().ok()?, b.parse().ok()?)
                } else {
                    let v: u32 = range.parse().ok()?;
                    (v, if item.contains('/') { hi } else { v })
                };
                if a < lo || b > hi || a > b {
                    return None;
                }
                let mut v = a;
                while v <= b {
                    set.insert(v);
                    v += step;
                }
            }
            fields[i] = set;
        }
        Some(Cron { fields })
    }

    /// Whether the minute containing `ms` matches.
    fn matches(&self, ms: u64) -> bool {
        let (min, hour, dom, month, dow) = civil(ms);
        self.fields[0].contains(&min)
            && self.fields[1].contains(&hour)
            && self.fields[2].contains(&dom)
            && self.fields[3].contains(&month)
            && self.fields[4].contains(&dow)
    }
}

/// (minute, hour, day of month, month, day of week with Sunday = 0) of a UTC time.
fn civil(ms: u64) -> (u32, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let _ = era;
    let dow = ((days + 4).rem_euclid(7)) as u32;
    ((rem % 3600) / 60, rem / 3600, d, m, dow)
}

/// Whether the UTC minute containing `ms` matches the cron expression `expr`.
pub fn cron_matches(expr: &str, ms: u64) -> bool {
    Cron::parse(expr).is_some_and(|c| c.matches(ms))
}

fn interval_ms(schedule: &str) -> Option<u64> {
    match schedule {
        "daily" => Some(24 * 3600 * 1000),
        "weekly" => Some(7 * 24 * 3600 * 1000),
        "monthly" => Some(30 * 24 * 3600 * 1000),
        _ => None,
    }
}

fn val_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| val_str(Some(x))).collect(),
        Some(x) => val_str(Some(x)).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Parses an update config file (`.beehive/dependabot.yml` and the Dependabot format are the
/// same). Returns the entries and a message per problem; a problem skips only its entry.
pub fn parse_update_config(path: &str, text: &str) -> (Vec<EntryCfg>, Vec<String>) {
    let mut errors = Vec::new();
    let doc = match bee_deps::yaml::parse(path, text) {
        Ok(v) => v,
        Err(e) => return (Vec::new(), vec![format!("{path}: {}", e.message)]),
    };
    if val_str(doc.get("version")).as_deref() != Some("2") {
        errors.push(format!("{path}: `version: 2` is required"));
    }
    let Some(updates) = doc.get("updates").and_then(Value::as_array) else {
        errors.push(format!("{path}: `updates` must be a list"));
        return (Vec::new(), errors);
    };
    let mut out = Vec::new();
    for (i, u) in updates.iter().enumerate() {
        let eco_name = val_str(u.get("package-ecosystem")).unwrap_or_default();
        let Some(eco) = proto_ecosystem(&eco_name) else {
            errors.push(format!(
                "{path}: updates[{i}]: unsupported package-ecosystem {eco_name:?}"
            ));
            continue;
        };
        let schedule = u
            .pointer("/schedule/cronjob")
            .and_then(|c| val_str(Some(c)))
            .or_else(|| {
                u.pointer("/schedule/interval")
                    .and_then(|c| val_str(Some(c)))
            })
            .unwrap_or_default();
        if !valid_schedule(&schedule) {
            errors.push(format!(
                "{path}: updates[{i}]: schedule.interval must be daily, weekly, monthly or a cron expression"
            ));
            continue;
        }
        let mut dirs = string_list(u.get("directories"));
        if dirs.is_empty() {
            dirs.push(val_str(u.get("directory")).unwrap_or_else(|| "/".into()));
        }
        let limit = val_str(u.get("open-pull-requests-limit"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_OPEN_LIMIT);
        let mut groups = Vec::new();
        if let Some(g) = u.get("groups").and_then(Value::as_object) {
            for (name, body) in g {
                groups.push(GroupCfg {
                    name: name.clone(),
                    patterns: string_list(body.get("patterns")),
                    applies_to: val_str(body.get("applies-to")).unwrap_or_default(),
                });
            }
        }
        let ignore = u
            .get("ignore")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| val_str(x.get("dependency-name")))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        for d in dirs {
            let directory = if d.starts_with('/') {
                d
            } else {
                format!("/{d}")
            };
            out.push(EntryCfg {
                ecosystem: eco.to_string(),
                directory,
                schedule: schedule.clone(),
                limit,
                groups: groups.clone(),
                target_ref: val_str(u.get("target-branch")).unwrap_or_default(),
                ignore: ignore.clone(),
                labels: string_list(u.get("labels")),
            });
        }
    }
    (out, errors)
}

fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(pos) => rest = &rest[pos + p.len()..],
                None => return false,
            }
        }
    }
    true
}

fn dir_prefix(directory: &str) -> String {
    directory.trim_matches('/').to_string()
}

fn in_dir(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn run_state(r: &DependencyUpdateRun) -> DependencyUpdateRunState {
    DependencyUpdateRunState::try_from(r.state).unwrap_or(DependencyUpdateRunState::Unspecified)
}

fn run_label(s: DependencyUpdateRunState) -> &'static str {
    match s {
        DependencyUpdateRunState::Queued => "queued",
        DependencyUpdateRunState::Running => "running",
        DependencyUpdateRunState::Succeeded => "succeeded",
        DependencyUpdateRunState::Failed => "failed",
        DependencyUpdateRunState::Unspecified => "unspecified",
    }
}

/// One change request to open: a title and the updates it carries.
struct Partition {
    title: String,
    group_key: String,
    updates: Vec<PlannedUpdate>,
    security: bool,
}

impl Security {
    /// The repo's update configuration: server switches plus the parsed config file of the
    /// default branch.
    pub async fn get_update_config(&self, cx: &Cx, repo: &Repo) -> Result<DependencyUpdateConfig> {
        let cfg = self.config_of(cx, repo).await?;
        let sw = self.update_switches(cx, repo).await?;
        let (entries, errors, path) = self.read_update_entries(repo).await;
        Ok(DependencyUpdateConfig {
            repo: repo_ref_of(repo),
            security_updates_enabled: cfg.security_updates,
            version_updates_enabled: sw.version_updates_enabled,
            grouped_security_updates: sw.grouped_security_updates,
            config_path: path,
            entries: entries
                .iter()
                .map(|e| DependencyUpdateEntry {
                    ecosystem: e.ecosystem.clone(),
                    directory: e.directory.clone(),
                    schedule: e.schedule.clone(),
                    open_change_request_limit: e.limit,
                    groups: e.groups.iter().map(|g| g.name.clone()).collect(),
                    target_ref: e.target_ref.clone(),
                })
                .collect(),
            actor_principal_id: if sw.actor_principal_id.is_empty() {
                DEFAULT_BOT.to_string()
            } else {
                sw.actor_principal_id
            },
            config_errors: errors,
        })
    }

    async fn update_switches(&self, cx: &Cx, repo: &Repo) -> Result<DependencyUpdateConfig> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx
            .get_msg::<DependencyUpdateConfig>(&CONFIG, &repo.id, "update_cfg")
            .await?
            .map(|(c, _)| c)
            .unwrap_or_default())
    }

    /// Reads the config file from the default branch; problems are returned, never raised.
    pub(crate) async fn read_update_entries(
        &self,
        repo: &Repo,
    ) -> (Vec<EntryCfg>, Vec<String>, String) {
        let tip = match self.ports.vcs.resolve(repo, "").await {
            Ok(t) => t,
            Err(e) => {
                return (
                    Vec::new(),
                    vec![format!("default branch is not readable: {}", e.message)],
                    String::new(),
                );
            }
        };
        for p in CONFIG_PATHS {
            if let Ok(Some(bytes)) = self.ports.vcs.read_file(repo, &tip, p).await {
                let (entries, errors) = parse_update_config(p, &String::from_utf8_lossy(&bytes));
                return (entries, errors, (*p).to_string());
            }
        }
        (Vec::new(), Vec::new(), String::new())
    }

    /// `SetDependencyUpdateConfig`: switches only; the schedule and groups stay in the file.
    pub async fn set_update_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        req: &SetDependencyUpdateConfigRequest,
    ) -> Result<DependencyUpdateConfig> {
        if let Some(s) = req.security_updates_enabled {
            self.update_config(
                cx,
                repo,
                &UpdateSecurityConfigurationRequest {
                    security_updates: Some(s),
                    ..Default::default()
                },
            )
            .await?;
        }
        let mut sw = self.update_switches(cx, repo).await?;
        let before = serde_json::json!({
            "version_updates": sw.version_updates_enabled,
            "grouped": sw.grouped_security_updates,
            "actor": sw.actor_principal_id,
        })
        .to_string();
        if let Some(v) = req.version_updates_enabled {
            sw.version_updates_enabled = v;
        }
        if let Some(v) = req.grouped_security_updates {
            sw.grouped_security_updates = v;
        }
        if let Some(a) = &req.actor_principal_id {
            if !a.is_empty() && self.directory.get(a).await.is_none() && a != DEFAULT_BOT {
                return Err(ForgeError::invalid(
                    "unknown_principal",
                    format!("principal {a} does not exist"),
                ));
            }
            sw.actor_principal_id = a.clone();
        }
        sw.repo = repo_ref_of(repo);
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.put(
            &CONFIG,
            &Row::new(&repo.id, "update_cfg").k1("update_cfg").msg(&sw),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "dependency_update.config_updated",
                action: "security.dependency_update.config",
                subject: SubjectKind::Repo,
                id: &repo.id,
                payload: serde_json::json!({
                    "version_updates": sw.version_updates_enabled,
                    "grouped": sw.grouped_security_updates,
                }),
                before,
                after: serde_json::json!({
                    "version_updates": sw.version_updates_enabled,
                    "grouped": sw.grouped_security_updates,
                    "actor": sw.actor_principal_id,
                })
                .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        self.get_update_config(cx, repo).await
    }

    // ---- runs ---------------------------------------------------------------------------

    /// `TriggerDependencyUpdateRun`: runs now (the returned run is finished). A run for the
    /// same entry that is queued or running is `RESOURCE_EXHAUSTED`.
    pub async fn trigger_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        ecosystem: &str,
        directory: &str,
        security_only: bool,
    ) -> Result<DependencyUpdateRun> {
        if self.changes.is_none() {
            return Err(crate::service::unserved(
                "TriggerDependencyUpdateRun",
                "the changes module (update runs open change requests)",
            ));
        }
        if !ecosystem.is_empty() && Ecosystem::from_proto_name(ecosystem).is_none() {
            return Err(ForgeError::invalid(
                "unknown_ecosystem",
                "unknown ecosystem",
            ));
        }
        let cfg = self.config_of(cx, repo).await?;
        let sw = self.update_switches(cx, repo).await?;
        if !cfg.security_updates && !sw.version_updates_enabled {
            return Err(ForgeError::invalid_state(
                "automated updates are not enabled for this repo",
            ));
        }
        self.ensure_index(cx).await?;
        let run = self
            .queue_run(cx, repo, ecosystem, directory, security_only)
            .await?;
        self.execute_run(cx, repo, &run.id).await
    }

    fn entry_key(ecosystem: &str, directory: &str) -> String {
        if ecosystem.is_empty() {
            "*".to_string()
        } else {
            format!(
                "{ecosystem}|{}",
                if directory.is_empty() { "/" } else { directory }
            )
        }
    }

    /// Queues a run without executing it (the server's worker, or [`Security::execute_run`],
    /// runs it). A run for the same entry that is queued or running is `RESOURCE_EXHAUSTED`.
    pub async fn queue_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        ecosystem: &str,
        directory: &str,
        security_only: bool,
    ) -> Result<DependencyUpdateRun> {
        let key = Self::entry_key(ecosystem, directory);
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let others = tx
            .scan_msgs::<UpdateRunRec>(
                &UPDATE_RUNS,
                &Scan::scope(&repo.id, 1000).key(Key::K1(key.clone())),
            )
            .await?;
        if others.iter().any(|(r, _)| {
            r.run.as_ref().is_some_and(|x| {
                matches!(
                    run_state(x),
                    DependencyUpdateRunState::Queued | DependencyUpdateRunState::Running
                )
            })
        }) {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::ResourceExhausted,
                format!("an update run for {key} is already queued"),
            )
            .with_subcode("update_run_queued")
            .with_retry_after(5000));
        }
        let run = DependencyUpdateRun {
            id: tx.new_id()?,
            repo: repo_ref_of(repo),
            ecosystem: ecosystem.to_string(),
            directory: directory.to_string(),
            security_only,
            state: DependencyUpdateRunState::Queued as i32,
            change_requests: Vec::new(),
            error: String::new(),
            agent_run_id: String::new(),
            created_at: Some(ts(now)),
            finished_at: None,
        };
        let rec = UpdateRunRec {
            run: Some(run.clone()),
            starter: cx.actor_id.clone(),
            entry: key.clone(),
        };
        tx.insert(
            &UPDATE_RUNS,
            &Row::new(&repo.id, &run.id)
                .k1(&key)
                .k2("queued")
                .n1(now as i64)
                .msg(&rec),
        )
        .await
        .map_err(|e| {
            // The unique `(scope, k1)` index is the cross-replica queue lease. A competing
            // transaction may have committed after the scan above; it is the same observable
            // condition as an already queued run for an explicit caller.
            if e.reason == bee_proto::v1::ErrorReason::AlreadyExists {
                ForgeError::new(
                    bee_proto::v1::ErrorReason::ResourceExhausted,
                    format!("an update run for {key} is already queued"),
                )
                .with_subcode("update_run_queued")
                .with_retry_after(5000)
            } else {
                e
            }
        })?;
        record(
            &mut tx,
            Change {
                repo,
                event: "dependency_update.run_started",
                action: "security.dependency_update.run",
                subject: SubjectKind::Repo,
                id: &run.id,
                payload: serde_json::json!({"run_id": run.id, "entry": key, "security_only": security_only}),
                before: String::new(),
                after: serde_json::json!({"run_id": run.id, "entry": key}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    async fn set_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        f: impl FnOnce(&mut DependencyUpdateRun),
        event: bool,
    ) -> Result<DependencyUpdateRun> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let Some((mut rec, row)) = tx
            .get_msg::<UpdateRunRec>(&UPDATE_RUNS, &repo.id, id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("update run {id}")));
        };
        let mut run = rec.run.clone().unwrap_or_default();
        f(&mut run);
        rec.run = Some(run.clone());
        let row = if matches!(
            run_state(&run),
            DependencyUpdateRunState::Queued | DependencyUpdateRunState::Running
        ) {
            row.clone().k2(run_label(run_state(&run))).msg(&rec)
        } else {
            // A terminal run releases the durable queue lease, permitting a later run for
            // this entry while retaining the historical record.
            let mut finished = row.clone().k2(run_label(run_state(&run))).msg(&rec);
            finished.k1 = None;
            finished
        };
        tx.update(&UPDATE_RUNS, &row).await?;
        if event {
            record(
                &mut tx,
                Change {
                    repo,
                    event: "dependency_update.run_finished",
                    action: "security.dependency_update.finish",
                    subject: SubjectKind::Repo,
                    id,
                    payload: serde_json::to_value(&run).unwrap_or_default(),
                    before: String::new(),
                    after: serde_json::json!({
                        "run_id": id, "state": run_label(run_state(&run)),
                        "change_requests": run.change_requests, "error": run.error,
                    })
                    .to_string(),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(run)
    }

    /// Executes a queued run and records its outcome.
    pub async fn execute_run(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<DependencyUpdateRun> {
        let (rec, _) = self.run_rec(cx, repo, id).await?;
        let run = rec.run.clone().unwrap_or_default();
        if run_state(&run) != DependencyUpdateRunState::Queued {
            return Ok(run);
        }
        self.set_run(
            cx,
            repo,
            id,
            |r| r.state = DependencyUpdateRunState::Running as i32,
            false,
        )
        .await?;
        let outcome = self.plan_and_open(cx, repo, &run).await;
        let now = self.now();
        match outcome {
            Ok(crs) => {
                self.set_run(
                    cx,
                    repo,
                    id,
                    |r| {
                        r.state = DependencyUpdateRunState::Succeeded as i32;
                        r.change_requests = crs.iter().map(u64::to_string).collect();
                        r.finished_at = Some(ts(now));
                    },
                    true,
                )
                .await
            }
            Err(e) => {
                self.set_run(
                    cx,
                    repo,
                    id,
                    |r| {
                        r.state = DependencyUpdateRunState::Failed as i32;
                        r.error = e.to_string();
                        r.finished_at = Some(ts(now));
                    },
                    true,
                )
                .await
            }
        }
    }

    async fn run_rec(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<(UpdateRunRec, Row)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.get_msg::<UpdateRunRec>(&UPDATE_RUNS, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("update run {id}")))
    }

    /// `GetDependencyUpdateRun`.
    pub async fn get_run(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<DependencyUpdateRun> {
        Ok(self.run_rec(cx, repo, id).await?.0.run.unwrap_or_default())
    }

    /// `ListDependencyUpdateRuns`, newest first.
    pub async fn list_runs(
        &self,
        cx: &Cx,
        repo: &Repo,
        state: i32,
        token: &str,
        size: usize,
    ) -> Result<(Vec<DependencyUpdateRun>, String)> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<UpdateRunRec>(
                &UPDATE_RUNS,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN).by_n1().desc(),
            )
            .await?;
        let all: Vec<DependencyUpdateRun> = rows
            .into_iter()
            .filter_map(|(r, _)| r.run)
            .filter(|r| state == 0 || r.state == state)
            .collect();
        page_slice(&all, token, size)
    }

    /// Runs the security updates a repo with `security_updates` on is owed after new alerts.
    /// Failures are recorded on the run, never raised.
    pub(crate) async fn auto_security_updates(&self, cx: &Cx, repo: &Repo) {
        if self.changes.is_none() {
            return;
        }
        let Ok(cfg) = self.config_of(cx, repo).await else {
            return;
        };
        if !cfg.security_updates {
            return;
        }
        // Both alert creation and enabling this switch call this method. Only an open alert
        // without its durable CR link is owed work; once linked, repeated triggers are no-ops.
        let Ok(alerts) = self.dependency_alert_recs(cx, repo).await else {
            return;
        };
        if !alerts.iter().any(|(rec, _)| {
            rec.alert.as_ref().is_some_and(|alert| {
                alert.state == DependencyAlertState::Open as i32
                    && alert.update_change_request.is_empty()
                    && rec.update_cr_id.is_empty()
            })
        }) {
            return;
        }
        if let Ok(run) = self.queue_run(cx, repo, "", "", true).await {
            let _ = self.execute_run(cx, repo, &run.id).await;
        }
    }

    /// Runs scheduled entries that are due (version updates; one run per due entry).
    pub async fn run_due_updates(&self, cx: &Cx) -> Result<usize> {
        if self.changes.is_none() {
            return Ok(0);
        }
        self.ensure_index(cx).await?;
        let now = self.now();
        let repo_ids: Vec<String> = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<DependencyUpdateConfig>(
                &CONFIG,
                &Scan {
                    key: Some(Key::K1("update_cfg".into())),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .filter(|(c, _)| c.version_updates_enabled)
            .map(|(_, row)| row.scope)
            .collect()
        };
        let mut n = 0;
        for id in repo_ids {
            let Ok(repo) = self.orgs.repo_any(cx, &id).await else {
                continue;
            };
            if repo.archived {
                continue;
            }
            let (entries, _, _) = self.read_update_entries(&repo).await;
            let mut state = {
                let mut tx = self.db.begin(Surface("security"), cx).await?;
                tx.get_msg::<UpdateStateRec>(&CONFIG, &repo.id, "update_state")
                    .await?
                    .map(|(s, _)| s)
                    .unwrap_or_default()
            };
            let mut changed = false;
            for e in entries {
                let key = Self::entry_key(&e.ecosystem, &e.directory);
                let idx = state.entries.iter().position(|k| *k == key);
                let last = idx.map(|i| state.last_ms[i]).unwrap_or(0);
                let due = match interval_ms(&e.schedule) {
                    Some(iv) => now >= last + iv,
                    None => Cron::parse(&e.schedule)
                        .is_some_and(|c| c.matches(now) && now / 60_000 != last / 60_000),
                };
                if !due {
                    continue;
                }
                match idx {
                    Some(i) => state.last_ms[i] = now,
                    None => {
                        state.entries.push(key.clone());
                        state.last_ms.push(now);
                    }
                }
                changed = true;
                if let Ok(run) = self
                    .queue_run(cx, &repo, &e.ecosystem, &e.directory, false)
                    .await
                {
                    let _ = self.execute_run(cx, &repo, &run.id).await;
                    n += 1;
                }
            }
            if changed {
                let mut tx = self.db.begin(Surface("security"), cx).await?;
                tx.put(
                    &CONFIG,
                    &Row::new(&repo.id, "update_state")
                        .k1("update_state")
                        .msg(&state),
                )
                .await?;
                tx.commit().await?;
            }
        }
        // Runs a crash left queued.
        let stuck: Vec<(String, String)> = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<UpdateRunRec>(
                &UPDATE_RUNS,
                &Scan {
                    key: Some(Key::K2("queued".into())),
                    limit: 100,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .map(|(_, row)| (row.scope, row.id))
            .collect()
        };
        for (repo_id, id) in stuck {
            if let Ok(repo) = self.orgs.repo_any(cx, &repo_id).await {
                let _ = self.execute_run(cx, &repo, &id).await;
                n += 1;
            }
        }
        Ok(n)
    }

    // ---- planning and opening ---------------------------------------------------------------

    async fn cr_open(&self, cx: &Cx, repo: &Repo, id: &str) -> bool {
        let Some(ch) = &self.changes else {
            return false;
        };
        ch.get(cx, repo, id)
            .await
            .map(|c| c.state == ChangeState::Open as i32)
            .unwrap_or(false)
    }

    async fn plan_and_open(
        &self,
        cx: &Cx,
        repo: &Repo,
        run: &DependencyUpdateRun,
    ) -> Result<Vec<u64>> {
        let changes = self
            .changes
            .clone()
            .ok_or_else(|| ForgeError::unimplemented("the changes module is not attached"))?;
        let sw = self.update_switches(cx, repo).await?;
        let cfg = self.config_of(cx, repo).await?;
        let tip = self.ports.vcs.resolve(repo, "").await?;
        let (files, _) = self.dependency_files(repo, &tip).await?;
        let (file_entries, _, _) = self.read_update_entries(repo).await;
        let graph = DependencyGraph::from_files(&files);
        let selected: Vec<EntryCfg> = if run.ecosystem.is_empty() {
            let mut v = file_entries.clone();
            // With security updates on and nothing configured, every ecosystem present is
            // updated at the repo root.
            let present: BTreeSet<&str> = graph
                .manifests()
                .iter()
                .map(|m| m.ecosystem.proto_name())
                .collect();
            for e in present {
                if !v.iter().any(|x| x.ecosystem == e) {
                    v.push(EntryCfg {
                        ecosystem: e.to_string(),
                        directory: "/".into(),
                        limit: DEFAULT_OPEN_LIMIT,
                        ..EntryCfg::default()
                    });
                }
            }
            v
        } else {
            let dir = if run.directory.is_empty() {
                "/"
            } else {
                &run.directory
            };
            let found: Vec<EntryCfg> = file_entries
                .iter()
                .filter(|e| e.ecosystem == run.ecosystem && e.directory == dir)
                .cloned()
                .collect();
            if found.is_empty() {
                vec![EntryCfg {
                    ecosystem: run.ecosystem.clone(),
                    directory: dir.to_string(),
                    limit: DEFAULT_OPEN_LIMIT,
                    ..EntryCfg::default()
                }]
            } else {
                found
            }
        };
        let alerts = self.dependency_alert_recs(cx, repo).await?;
        let dismissed: BTreeSet<String> = alerts
            .iter()
            .filter(|(r, _)| {
                r.alert
                    .as_ref()
                    .is_some_and(|a| a.state != bee_proto::v1::DependencyAlertState::Open as i32)
            })
            .map(|(r, _)| r.key.clone())
            .collect();
        let bot = if sw.actor_principal_id.is_empty() {
            DEFAULT_BOT.to_string()
        } else {
            sw.actor_principal_id.clone()
        };
        let mut bot_cx = cx.clone();
        bot_cx.actor_id = bot.clone();
        bot_cx.actor_kind = bee_proto::v1::PrincipalKind::Agent;
        let mut chain = vec![bot.clone()];
        chain.extend(cx.chain.iter().filter(|c| **c != bot).cloned());
        bot_cx.chain = chain.clone();
        let bot_principal = self.principal(&bot).await;
        let locks: Arc<dyn super::ports::LockRefresher> = match &self.ports.locks {
            Some(l) => l.clone(),
            None => Arc::new(TextLockRefresher::new(self.ports.registry.clone())),
        };
        let mut opened: Vec<u64> = Vec::new();
        for entry in selected {
            let Some(eco) = Ecosystem::from_proto_name(&entry.ecosystem) else {
                continue;
            };
            let prefix = dir_prefix(&entry.directory);
            let efiles: Vec<SourceFile> = files
                .iter()
                .filter(|f| in_dir(&f.path, &prefix))
                .cloned()
                .collect();
            let sub = DependencyGraph::from_files(&efiles);
            let mut plan_groups: Vec<UpdateGroup> = Vec::new();
            let mut security_pkgs: BTreeSet<String> = BTreeSet::new();
            if cfg.security_updates {
                let findings: Vec<Finding> = self
                    .findings_of(&sub)?
                    .into_iter()
                    .filter(|f| f.ecosystem == eco)
                    .filter(|f| {
                        !dismissed
                            .contains(&format!("{}|{}|{}", f.purl, f.manifest_path, f.advisory_id))
                    })
                    .filter(|f| !entry.ignore.iter().any(|g| glob(g, &f.package)))
                    .collect();
                let plan: UpdatePlan = {
                    let idx = self.index.read().map_err(super::advisories::poisoned)?;
                    plan_security_updates(&efiles, &sub, &findings, &idx.source)
                };
                for g in plan.groups {
                    for u in &g.updates {
                        security_pkgs.insert(u.package.clone());
                    }
                    plan_groups.push(g);
                }
            }
            let version_cfg = !run.security_only
                && sw.version_updates_enabled
                && file_entries
                    .iter()
                    .any(|e| e.ecosystem == entry.ecosystem && e.directory == entry.directory);
            let mut version_groups: Vec<UpdateGroup> = Vec::new();
            if version_cfg {
                let mut synthetic: Vec<Finding> = Vec::new();
                for p in sub.packages() {
                    if p.ecosystem != eco
                        || p.relationship != Relationship::Direct
                        || security_pkgs.contains(&p.name)
                        || entry.ignore.iter().any(|g| glob(g, &p.name))
                    {
                        continue;
                    }
                    let Some(latest) = self.ports.registry.latest_version(eco, &p.name).await
                    else {
                        continue;
                    };
                    if bee_deps::version::compare(eco, &latest, &p.version)
                        != std::cmp::Ordering::Greater
                    {
                        continue;
                    }
                    synthetic.push(Finding {
                        advisory_id: "version-update".into(),
                        aliases: Vec::new(),
                        summary: format!("{} {latest} is available", p.name),
                        severity: bee_deps::cvss::Severity::None,
                        cvss_score: None,
                        cvss_vector: None,
                        ecosystem: eco,
                        package: p.name.clone(),
                        purl: p.purl.clone(),
                        installed_version: p.version.clone(),
                        fixed_versions: vec![latest],
                        scope: p.scope,
                        relationship: p.relationship,
                        manifest_path: p.source_path.clone(),
                        dependency_path: vec![p.purl.clone()],
                    });
                }
                let none = FileAdvisorySource::new();
                version_groups = plan_security_updates(&efiles, &sub, &synthetic, &none).groups;
            }
            let mut partitions = partition(&plan_groups, &entry, sw.grouped_security_updates, true);
            partitions.extend(partition(&version_groups, &entry, false, false));
            let mut open_now = self.open_pr_count(cx, repo, &entry).await?;
            for part in partitions {
                if open_now >= entry.limit.max(1) as usize {
                    break;
                }
                let key = format!(
                    "{}|{}|{}|{}",
                    entry.ecosystem,
                    entry.directory,
                    part.group_key,
                    part.updates
                        .iter()
                        .map(|u| format!("{}@{}", u.package, u.to))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                if self.pr_exists(cx, repo, &key).await? {
                    continue;
                }
                let group = UpdateGroup {
                    ecosystem: eco,
                    title: part.title.clone(),
                    updates: part.updates.clone(),
                };
                let manifest_changes = apply_plan(
                    &efiles,
                    &UpdatePlan {
                        groups: vec![group.clone()],
                        unfixable: Vec::new(),
                    },
                )
                .map_err(|e| ForgeError::invalid("update_failed", e.message))?;
                let mut after: Vec<SourceFile> = efiles.clone();
                for c in &manifest_changes {
                    if let Some(f) = after.iter_mut().find(|f| f.path == c.path) {
                        f.content = c.content.clone();
                    }
                }
                let lock_changes = locks.refresh(&after, &part.updates).await?;
                let mut edits: Vec<(String, Option<Vec<u8>>)> = Vec::new();
                let mut seen = BTreeSet::new();
                for c in manifest_changes.iter().chain(lock_changes.iter()) {
                    if seen.insert(c.path.clone()) {
                        let content = lock_changes
                            .iter()
                            .find(|l| l.path == c.path)
                            .unwrap_or(c)
                            .content
                            .clone();
                        edits.push((c.path.clone(), Some(content.into_bytes())));
                    }
                }
                if edits.is_empty() {
                    continue;
                }
                let message = part.title.clone();
                let commit = changes
                    .vcs()
                    .create_commit(
                        repo,
                        NewCommit {
                            parent: Some(tip.clone()),
                            edits,
                            message: message.clone(),
                            author: bot_principal.display.clone(),
                            creator: bot.clone(),
                            chain: chain.clone(),
                            change_id: None,
                        },
                    )
                    .await?;
                let target = if entry.target_ref.is_empty() {
                    String::new()
                } else {
                    entry.target_ref.clone()
                };
                let mut labels = entry.labels.clone();
                labels.push("dependencies".into());
                if part.security {
                    labels.push("security".into());
                }
                labels.sort();
                labels.dedup();
                let cr = changes
                    .open(
                        &bot_cx,
                        repo,
                        NewChange {
                            target_ref: target,
                            head: Some(commit),
                            title: part.title.clone(),
                            body: body_of(&part, &entry, &group),
                            labels,
                            ..NewChange::default()
                        },
                    )
                    .await?;
                self.remember_pr(cx, repo, &key, &cr.id, cr.number, part.security)
                    .await?;
                if part.security {
                    self.link_alerts_to_cr(cx, repo, &part.updates, &cr.id, cr.number)
                        .await?;
                }
                open_now += 1;
                opened.push(cr.number);
            }
        }
        Ok(opened)
    }

    async fn open_pr_count(&self, cx: &Cx, repo: &Repo, entry: &EntryCfg) -> Result<usize> {
        let rows = {
            let mut tx = self.db.begin(Surface("security"), cx).await?;
            tx.scan_msgs::<UpdatePrRec>(&UPDATE_PRS, &Scan::scope(&repo.id, crate::store::MAX_SCAN))
                .await?
        };
        let prefix = format!("{}|{}|", entry.ecosystem, entry.directory);
        let mut n = 0;
        for (rec, row) in rows {
            if row.id.starts_with(&prefix) && self.cr_open(cx, repo, &rec.cr_id).await {
                n += 1;
            }
        }
        Ok(n)
    }

    async fn pr_exists(&self, cx: &Cx, repo: &Repo, key: &str) -> Result<bool> {
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        Ok(tx.get(&UPDATE_PRS, &repo.id, key).await?.is_some())
    }

    async fn remember_pr(
        &self,
        cx: &Cx,
        repo: &Repo,
        key: &str,
        cr_id: &str,
        number: u64,
        security: bool,
    ) -> Result<()> {
        let now = self.now();
        let rec = UpdatePrRec {
            number,
            cr_id: cr_id.to_string(),
            created_ms: now,
            security,
        };
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        tx.put(
            &UPDATE_PRS,
            &Row::new(&repo.id, key).n1(now as i64).msg(&rec),
        )
        .await?;
        record(
            &mut tx,
            Change {
                repo,
                event: "dependency_update.change_opened",
                action: "security.dependency_update.open_change",
                subject: SubjectKind::ChangeRequest,
                id: &number.to_string(),
                payload: serde_json::json!({"change_request": number, "key": key, "security": security}),
                before: String::new(),
                after: serde_json::json!({"change_request": number, "key": key}).to_string(),
            },
        )
        .await?;
        tx.commit().await
    }

    /// Points the alerts a security update fixes at its change request.
    async fn link_alerts_to_cr(
        &self,
        cx: &Cx,
        repo: &Repo,
        updates: &[PlannedUpdate],
        cr_id: &str,
        number: u64,
    ) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(Surface("security"), cx).await?;
        let rows = tx
            .scan_msgs::<super::model::DepAlertRec>(
                &DEP_ALERTS,
                &Scan::scope(&repo.id, crate::store::MAX_SCAN),
            )
            .await?;
        for (mut rec, row) in rows {
            let Some(mut alert) = rec.alert.clone() else {
                continue;
            };
            let hit = updates.iter().any(|u| {
                alert.dependency.as_ref().is_some_and(|d| {
                    d.name == u.package && d.manifest_path == u.source_path || d.name == u.package
                }) && (u.advisories.contains(&rec.advisory_id)
                    || u.advisories.iter().any(|a| rec.aliases.contains(a)))
            });
            if hit && alert.state == bee_proto::v1::DependencyAlertState::Open as i32 {
                alert.update_change_request = number.to_string();
                alert.updated_at = Some(ts(now));
                rec.update_cr_id = cr_id.to_string();
                rec.alert = Some(alert);
                tx.update(&DEP_ALERTS, &row.clone().msg(&rec)).await?;
            }
        }
        tx.commit().await
    }
}

/// Splits planned groups into change requests: updates matching a configured group share one;
/// with `all_grouped` every ecosystem group is one; the rest are one per package.
fn partition(
    groups: &[UpdateGroup],
    entry: &EntryCfg,
    all_grouped: bool,
    security: bool,
) -> Vec<Partition> {
    let mut out = Vec::new();
    for g in groups {
        if all_grouped {
            out.push(Partition {
                title: g.title.clone(),
                group_key: format!("all-{}", g.ecosystem.proto_name()),
                updates: g.updates.clone(),
                security,
            });
            continue;
        }
        let mut rest: Vec<PlannedUpdate> = g.updates.clone();
        for cfg in &entry.groups {
            let applies = match cfg.applies_to.as_str() {
                "security-updates" => security,
                "version-updates" => !security,
                _ => true,
            };
            if !applies {
                continue;
            }
            let (matched, others): (Vec<PlannedUpdate>, Vec<PlannedUpdate>) =
                rest.into_iter().partition(|u| {
                    cfg.patterns.is_empty() || cfg.patterns.iter().any(|p| glob(p, &u.package))
                });
            rest = others;
            if !matched.is_empty() {
                out.push(Partition {
                    title: format!(
                        "Bump the {} group with {} update{}",
                        cfg.name,
                        matched.len(),
                        if matched.len() == 1 { "" } else { "s" }
                    ),
                    group_key: cfg.name.clone(),
                    updates: matched,
                    security,
                });
            }
        }
        for u in rest {
            out.push(Partition {
                title: format!("Bump {} from {} to {}", u.package, u.from, u.to),
                group_key: format!("pkg-{}", u.package),
                updates: vec![u],
                security,
            });
        }
    }
    out
}

fn body_of(part: &Partition, entry: &EntryCfg, group: &UpdateGroup) -> String {
    let mut s = String::new();
    if part.security {
        s.push_str("Fixes security advisories in the dependencies below.\n\n");
    } else {
        s.push_str("Updates dependencies to their newest versions.\n\n");
    }
    s.push_str("| Package | From | To | Advisories | Manifest |\n|---|---|---|---|---|\n");
    for u in &part.updates {
        s.push_str(&format!(
            "| {} | {} | {} | {} | `{}` |\n",
            u.package,
            u.from,
            u.to,
            if u.advisories.is_empty() {
                "-".to_string()
            } else {
                u.advisories.join(", ")
            },
            u.source_path
        ));
    }
    let commands = group.commands();
    if !commands.is_empty() {
        s.push_str("\nCommands a runner uses to finish lockfiles:\n\n");
        for c in commands {
            s.push_str(&format!("- `{c}`\n"));
        }
    }
    let notes: Vec<&String> = part.updates.iter().flat_map(|u| u.notes.iter()).collect();
    if !notes.is_empty() {
        s.push_str("\nNotes:\n\n");
        for n in notes {
            s.push_str(&format!("- {n}\n"));
        }
    }
    s.push_str(&format!(
        "\nOpened by the update bot for `{}` in `{}`.\n",
        entry.ecosystem, entry.directory
    ));
    s
}
