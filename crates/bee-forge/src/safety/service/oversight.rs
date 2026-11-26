use std::collections::{BTreeMap, BTreeSet};

use prost::Message as _;

use super::*;
use crate::store::{Cursor, Scan};

struct Period {
    start: u64,
    end: u64,
}

impl Period {
    fn parse(value: Option<SafetyPeriod>) -> Result<Self> {
        let value = value.ok_or_else(|| ForgeError::missing("period"))?;
        let timestamp = |value: Option<pbjson_types::Timestamp>| {
            value
                .filter(|t| {
                    (0..=253_402_300_799).contains(&t.seconds)
                        && (0..1_000_000_000).contains(&t.nanos)
                        && t.nanos % 1_000_000 == 0
                })
                .map(|t| ts_ms(&t))
                .ok_or_else(|| {
                    ForgeError::invalid(
                        "safety_period",
                        "Expected UTC timestamps at millisecond precision",
                    )
                })
        };
        let start = timestamp(value.start)?;
        let end = timestamp(value.end)?;
        if start >= end {
            return Err(ForgeError::invalid(
                "safety_period",
                "The start must precede the end",
            ));
        }
        Ok(Self { start, end })
    }

    fn contains(&self, time: u64) -> bool {
        (self.start..self.end).contains(&time)
    }

    fn sample_key(&self, seed: &str, repo: &str, work: &SafetyRef) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("Beehive safety spot check");
        hash.update(&self.start.to_be_bytes());
        hash.update(&self.end.to_be_bytes());
        for value in [seed, repo, &store::key(work)] {
            hash.update(&(value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        *hash.finalize().as_bytes()
    }
}

#[derive(Default)]
struct Summary {
    by_kind: BTreeMap<i32, u64>,
    by_work: BTreeMap<(i32, String), BTreeMap<i32, u64>>,
    tripwires: Vec<SafetyAttention>,
    waits: Vec<u64>,
}

fn counts(values: BTreeMap<i32, u64>) -> Vec<SafetySignalCount> {
    values
        .into_iter()
        .map(|(kind, count)| SafetySignalCount { kind, count })
        .collect()
}

impl Summary {
    fn record(&mut self, period: &Period, record: SafetyRecord) {
        let Some(work) = record.work else { return };
        let mut groups: BTreeSet<_> = record
            .agents
            .into_iter()
            .map(|agent| (SafetyWorkKind::Run as i32, agent.id))
            .collect();
        if work.kind() == SafetyWorkKind::Swarm {
            groups.insert((work.kind, work.id));
        }
        for finding in record.findings {
            if finding.state() == SafetyFindingState::Dismissed {
                continue;
            }
            let Some(created) = finding.created_at.as_ref().map(ts_ms) else {
                continue;
            };
            if !period.contains(created) {
                continue;
            }
            let Some(report) = finding.report else {
                continue;
            };
            *self.by_kind.entry(report.kind).or_default() += 1;
            for key in &groups {
                *self
                    .by_work
                    .entry(key.clone())
                    .or_default()
                    .entry(report.kind)
                    .or_default() += 1;
            }
            if let Some(resolved) = finding.resolved_at.as_ref().map(ts_ms) {
                self.waits.push(resolved.saturating_sub(created));
            }
        }
    }

    fn finish(mut self) -> SafetySummary {
        self.waits.sort_unstable();
        let middle = self.waits.len() / 2;
        let median_wait_ms = match self.waits.get(middle).copied() {
            Some(high) if self.waits.len().is_multiple_of(2) => {
                let low = self.waits[middle - 1];
                Some(low + (high - low) / 2)
            }
            high => high,
        };
        self.tripwires
            .sort_by_key(|item| (item.created_at.as_ref().map(ts_ms), item.id.clone()));
        SafetySummary {
            signals: counts(self.by_kind),
            work: self
                .by_work
                .into_iter()
                .map(|((kind, id), values)| SafetyWorkCount {
                    id,
                    kind,
                    signals: counts(values),
                })
                .collect(),
            tripwires: self.tripwires,
            median_wait_ms,
        }
    }
}

impl SafetyApi {
    pub(super) async fn spot_check(
        &self,
        caller: &Caller,
        request: SpotCheckRequest,
    ) -> Result<SpotCheckResponse> {
        require_person(caller)?;
        let (repo, _) = self.repo(caller, &request.repo).await?;
        let cx = self.writer(caller, &repo).await?;
        let period = Period::parse(request.period)?;
        required_text("seed", &request.seed, 256)?;
        let limit = if request.limit == 0 {
            5
        } else {
            request.limit.min(7) as usize
        };
        let mut best = BTreeMap::new();
        let mut after = String::new();
        loop {
            let (refs, token) = evidence::list_refs(&self.agents, &cx, &repo, &after, 40).await?;
            for work in refs {
                let snapshot = evidence::collect(&self.agents, &cx, &repo, work.clone()).await?;
                if snapshot
                    .agent_merge_times
                    .iter()
                    .any(|time| period.contains(*time))
                {
                    best.insert(
                        (
                            period.sample_key(&request.seed, &repo.id, &work),
                            store::key(&work),
                        ),
                        work,
                    );
                    if best.len() > limit {
                        best.pop_last();
                    }
                }
            }
            if token.is_empty() {
                break;
            }
            after = token;
        }
        let access = self.evidence_access(caller, &repo).await;
        let mut records = Vec::with_capacity(best.len());
        for work in best.into_values() {
            records.push(access.record(self.refresh(&repo, &cx, work).await?.0));
        }
        Ok(SpotCheckResponse { records })
    }

    pub(super) async fn summary(
        &self,
        caller: &Caller,
        request: GetSafetySummaryRequest,
    ) -> Result<SafetySummary> {
        require_person(caller)?;
        let period = Period::parse(request.period)?;
        let org = self.base.org_id(caller, &request.org).await?;
        let cx = self
            .agents
            .forge
            .authz
            .org(caller, &org, OrgPerm::ReadAudit)
            .await?;
        let mut summary = Summary::default();
        let mut after = String::new();
        let mut attention_bytes = 0;
        loop {
            let (repos, token) = self
                .agents
                .forge
                .orgs
                .list_repos(&cx, &org, &after, 64)
                .await?;
            for repo in repos {
                match self
                    .agents
                    .forge
                    .authz
                    .repo_every_path(caller, &repo, Action::Read)
                    .await
                {
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.reason,
                            ErrorReason::PermissionDenied
                                | ErrorReason::PolicyDenied
                                | ErrorReason::NotFound
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
                let mut cursor = None;
                loop {
                    let records = tx
                        .scan_msgs::<SafetyRecord>(
                            &crate::safety::RECORDS,
                            &Scan::scope(&repo.id, 128).after(cursor),
                        )
                        .await?;
                    cursor = records.last().map(|(_, row)| Cursor::after(row));
                    let done = records.len() < 128;
                    for (mut record, _) in records {
                        store::load_findings(&mut tx, &repo, &mut record).await?;
                        summary.record(&period, record);
                    }
                    if done {
                        break;
                    }
                }
                let mut cursor = None;
                loop {
                    let items = tx
                        .scan_msgs::<SafetyAttention>(
                            &crate::safety::ATTENTION,
                            &Scan::scope(&repo.id, 128)
                                .n1_min(period.start as i64)
                                .n1_max((period.end - 1) as i64)
                                .after(cursor),
                        )
                        .await?;
                    cursor = items.last().map(|(_, row)| Cursor::after(row));
                    let done = items.len() < 128;
                    for (item, _) in items {
                        if item.tripwire.is_some() {
                            attention_bytes += item.encoded_len() + 8;
                            if attention_bytes > store::MAX_PAGE_BYTES {
                                return Err(ForgeError::too_large(
                                    "safety tripwire summary",
                                    store::MAX_PAGE_BYTES as u64,
                                    attention_bytes as u64,
                                ));
                            }
                            summary.tripwires.push(item);
                        }
                    }
                    if done {
                        break;
                    }
                }
            }
            if token.is_empty() {
                break;
            }
            after = token;
        }
        let response = summary.finish();
        if response.encoded_len() > store::MAX_PAGE_BYTES {
            return Err(ForgeError::too_large(
                "safety summary",
                store::MAX_PAGE_BYTES as u64,
                response.encoded_len() as u64,
            ));
        }
        Ok(response)
    }
}
