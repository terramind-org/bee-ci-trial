//! Job logs: chunked appends into the blob store, masking, tailing, search, download
//! archives and the annotations derived from workflow commands.
//!
//! A runner appends chunks of at most 1 MiB at byte offsets in the stored log. An append at
//! the stored end is written (after masking every secret the job can see); an append at an
//! offset where a chunk already starts is a no-op returning the stored end (retries are
//! idempotent); anything past the end is `INVALID_ARGUMENT`/`offset_gap`. Offsets are in
//! stored (masked) bytes, so a runner continues from the returned `stored_end`.

use bee_proto::v1::{Repo, WorkflowLogChunk, WorkflowStatus};

use super::engine::{Ci, job_of, job_row, status_of};
use super::model::{JobIndex, JobRecord, LogChunk};
use super::util::{mask, zip};
use super::{JOB_INDEX, JOBS, LOGS, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::store::{Row, Scan};

/// Largest chunk one append carries.
pub const MAX_CHUNK: usize = 1024 * 1024;

/// One line matching a log search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMatch {
    /// Job id.
    pub job_id: String,
    /// Job name.
    pub job_name: String,
    /// 1-based line number in the job log.
    pub line: u64,
    /// The line.
    pub text: String,
}

impl Ci {
    /// Appends a chunk to a leased job's log; returns the next offset to write.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_log(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
        offset: u64,
        data: &[u8],
        step: u32,
    ) -> Result<u64> {
        if data.len() > MAX_CHUNK {
            return Err(ForgeError::too_large(
                "log_chunk_bytes",
                MAX_CHUNK as u64,
                data.len() as u64,
            ));
        }
        let lost = || {
            ForgeError::invalid_state("the job lease was lost; stop the job")
                .with_subcode("lease_lost")
        };
        let (idx, j) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((idx, _)) = tx.get_msg::<JobIndex>(&JOB_INDEX, "", job_id).await? else {
                return Err(lost());
            };
            let Some((j, _)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
                return Err(lost());
            };
            (idx, j)
        };
        if j.runner_id != runner_id
            || j.lease_id != lease_id
            || status_of(job_of(&j)) != WorkflowStatus::InProgress
        {
            return Err(lost());
        }
        let end = job_of(&j).log_size_bytes;
        if offset > end {
            return Err(ForgeError::invalid(
                "offset_gap",
                format!("offset {offset} is past the stored end {end}"),
            ));
        }
        if offset < end || data.is_empty() {
            return Ok(end);
        }
        let repo = self.repo_by_id(cx, &idx.repo_id).await?;
        let secrets: Vec<String> = self
            .secrets_for(cx, &repo, &j.environment)
            .await?
            .into_values()
            .collect();
        let text = String::from_utf8_lossy(data);
        let masked = mask(&text, &secrets).into_bytes();
        let len = masked.len() as u64;
        let blob = self.blobs.put(&idx.org_id, masked).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(lost());
        };
        if job_of(&j).log_size_bytes != offset || j.lease_id != lease_id {
            drop(tx);
            self.blobs.delete(&idx.org_id, &blob).await?;
            let cur = self.job_record(cx, &idx.repo_id, job_id).await?;
            return Ok(job_of(&cur).log_size_bytes);
        }
        tx.insert(
            &LOGS,
            &Row::new(job_id, format!("{offset:020}"))
                .n1(offset as i64)
                .msg(&LogChunk {
                    offset,
                    len,
                    step,
                    blob_id: blob,
                    org_id: idx.org_id.clone(),
                }),
        )
        .await?;
        let mut job = job_of(&j).clone();
        job.log_size_bytes = offset + len;
        j.job = Some(job);
        tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
            .await?;
        tx.commit().await?;
        self.wake.notify_waiters();
        Ok(offset + len)
    }

    /// Chunks of a job log from `offset` (0 = start), optionally of one step.
    pub async fn read_log(
        &self,
        cx: &Cx,
        repo_id: &str,
        job_id: &str,
        offset: u64,
        step: u32,
    ) -> Result<Vec<WorkflowLogChunk>> {
        let j = self.job_record(cx, repo_id, job_id).await?;
        if j.logs_deleted {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Redacted,
                "the job's logs were deleted",
            ));
        }
        let chunks = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<LogChunk>(&LOGS, &Scan::scope(job_id, crate::store::MAX_SCAN).by_n1())
                .await?
        };
        let done = status_of(job_of(&j)) == WorkflowStatus::Completed;
        let mut out = Vec::new();
        for (c, _) in chunks {
            if c.offset + c.len <= offset || (step != 0 && c.step != step) {
                continue;
            }
            let bytes = self
                .blobs
                .get(&c.org_id, &c.blob_id)
                .await?
                .ok_or_else(|| {
                    ForgeError::new(bee_proto::v1::ErrorReason::Redacted, "log chunk missing")
                })?;
            let skip = offset.saturating_sub(c.offset) as usize;
            out.push(WorkflowLogChunk {
                offset: c.offset + skip as u64,
                data: bytes.get(skip..).unwrap_or_default().to_vec(),
                step_number: c.step,
                eof: false,
            });
        }
        if done {
            match out.last_mut() {
                Some(l) => l.eof = true,
                None => out.push(WorkflowLogChunk {
                    offset: job_of(&j).log_size_bytes,
                    data: Vec::new(),
                    step_number: 0,
                    eof: true,
                }),
            }
        }
        Ok(out)
    }

    /// A whole job log as bytes.
    pub async fn job_log_bytes(&self, cx: &Cx, repo_id: &str, job_id: &str) -> Result<Vec<u8>> {
        Ok(self
            .read_log(cx, repo_id, job_id, 0, 0)
            .await?
            .into_iter()
            .flat_map(|c| c.data)
            .collect())
    }

    pub(crate) async fn job_log_text(&self, cx: &Cx, j: &JobRecord) -> Result<String> {
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<LogChunk>(
                &LOGS,
                &Scan::scope(job_of(j).id.clone(), crate::store::MAX_SCAN).by_n1(),
            )
            .await?
        };
        let mut out = Vec::new();
        for (c, _) in rows {
            if let Some(b) = self.blobs.get(&c.org_id, &c.blob_id).await? {
                out.extend(b);
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Lines of a run's job logs (latest attempt) containing `query` (case-insensitive).
    pub async fn search_logs(
        &self,
        cx: &Cx,
        repo: &Repo,
        run_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LogMatch>> {
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for job in self.list_jobs(cx, &repo.id, run_id, 0, false).await? {
            let text = String::from_utf8_lossy(&self.job_log_bytes(cx, &repo.id, &job.id).await?)
                .into_owned();
            for (n, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    out.push(LogMatch {
                        job_id: job.id.clone(),
                        job_name: job.name.clone(),
                        line: n as u64 + 1,
                        text: line.to_string(),
                    });
                    if out.len() >= limit.max(1) {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// A zip with one `<n>_<job name>.txt` per job of a run attempt (0 = latest).
    pub async fn run_logs_zip(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        attempt: u32,
    ) -> Result<Vec<u8>> {
        let mut entries = Vec::new();
        for (n, job) in self
            .list_jobs(cx, repo_id, run_id, attempt, false)
            .await?
            .into_iter()
            .enumerate()
        {
            let name: String = job
                .name
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')' | ',') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            entries.push((
                format!("{n}_{name}.txt"),
                self.job_log_bytes(cx, repo_id, &job.id).await?,
            ));
        }
        Ok(zip(&entries))
    }

    /// A signed URL for one job's log.
    pub fn job_logs_url(&self, repo_id: &str, job_id: &str) -> (String, u64) {
        let exp = self.now() + self.config.signed_url_ttl_ms;
        (
            self.urls
                .sign(&format!("/_ci/repos/{repo_id}/jobs/{job_id}/logs"), exp),
            exp,
        )
    }

    /// A signed URL for the zip of a run attempt's logs.
    pub fn run_logs_url(&self, repo_id: &str, run_id: &str, attempt: u32) -> (String, u64) {
        let exp = self.now() + self.config.signed_url_ttl_ms;
        (
            self.urls.sign(
                &format!("/_ci/repos/{repo_id}/runs/{run_id}/attempts/{attempt}/logs.zip"),
                exp,
            ),
            exp,
        )
    }

    /// Deletes every log of a run (all attempts); `INVALID_STATE` while it runs.
    pub async fn delete_run_logs(&self, cx: &Cx, repo: &Repo, run_id: &str) -> Result<()> {
        let run = self.get_run(cx, &repo.id, run_id).await?;
        if run.status != WorkflowStatus::Completed as i32 {
            return Err(ForgeError::invalid_state("the run is still running"));
        }
        let jobs = self.list_jobs(cx, &repo.id, run_id, 0, true).await?;
        let mut blobs = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for job in &jobs {
            for (c, row) in tx
                .scan_msgs::<LogChunk>(&LOGS, &Scan::scope(job.id.clone(), crate::store::MAX_SCAN))
                .await?
            {
                tx.delete(&LOGS, &job.id, &row.id).await?;
                blobs.push((c.org_id, c.blob_id));
            }
            if let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &repo.id, &job.id).await? {
                let mut w = job_of(&j).clone();
                w.log_size_bytes = 0;
                j.job = Some(w);
                j.logs_deleted = true;
                tx.update(&JOBS, &job_row(&repo.id, &j).version(row.version))
                    .await?;
            }
        }
        tx.audit(
            AuditDraft::new("workflow.logs_delete", &repo.org_id)
                .repo_id(&repo.id)
                .target(bee_proto::v1::SubjectKind::WorkflowRun, run_id),
        )
        .await?;
        tx.commit().await?;
        for (org, b) in blobs {
            self.blobs.delete(&org, &b).await?;
        }
        Ok(())
    }
}
