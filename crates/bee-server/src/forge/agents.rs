//! The ports of the agents module (`docs/spec/agents.md` §8) backed by this server:
//!
//! - [`ServerRunIssuer`]: run tokens signed with the server's key ring; revocations go to the
//!   server's persistent revocation list (so the server and the forge refuse them at once).
//! - [`ServerAgentBlobs`]: logs and transcripts as blobs of the internal `-agents` org.
//! - [`ServerAgentRepo`]: reads through the VCS; workspace changesets through the server's
//!   `WriteService.CreateChangeset` (push protection, provenance, budgets, audit) with a token
//!   for the run's principal chain.
//! - Runners: [`CommandRunner`] runs a configured shell command per run with only the run
//!   token (`forge.agent_runner = "command"`); `forge.agent_runner = "fake"` uses the forge's
//!   deterministic `FakeRunner` (development and demos); otherwise [`NoRunner`] fails runs
//!   with `no_runner_configured`.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_forge::ForgeError;
use bee_forge::agents::ports::{RunStatus, RunTokenIssuer};
use bee_forge::agents::{
    AgentBlobs, AgentRepo, AgentRunner, RepoFile, RunGrant, RunJob, RunOutcome, RunReporter,
    RunnerEvent,
};
use bee_forge::changes::vcs::HistoryVcs;
use bee_proto::v1::{self, Repo, write_service_server::WriteService};
use bee_store::{OrgId, OrgRow, UploadRecord};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{api_to_forge, status_to_forge};
use crate::services::write::Writes;
use crate::state::{AppState, RequestMeta};

type FResult<T> = bee_forge::Result<T>;

const RUN_CANCEL_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RUN_CANCEL_POLL_TIMEOUT: Duration = Duration::from_millis(500);

/// The internal VCS org that holds agent logs and transcripts.
pub const AGENT_ORG: &str = "-agents";

fn stopping() -> ForgeError {
    ForgeError::unavailable("server stopping")
}

// ----- tokens ------------------------------------------------------------------------------

/// Run tokens from the server's signing keys, revoked in its persistent revocation list.
pub struct ServerRunIssuer {
    state: Weak<AppState>,
}

impl ServerRunIssuer {
    /// An issuer over `state`'s signing keys.
    pub fn new(state: &Arc<AppState>) -> ServerRunIssuer {
        ServerRunIssuer {
            state: Arc::downgrade(state),
        }
    }
}

#[async_trait]
impl RunTokenIssuer for ServerRunIssuer {
    async fn issue(&self, grant: &RunGrant) -> FResult<bee_forge::agents::ports::IssuedToken> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        let token = match &grant.parent_token {
            Some(parent) => bee_auth::attenuate(
                parent,
                &bee_auth::Attenuation {
                    scope: grant.scope.clone(),
                    act_as: (grant.parent_principal != grant.agent.id).then(|| {
                        bee_auth::PrincipalRef::new(
                            grant.agent.id.clone(),
                            bee_policy::PrincipalKind::Agent,
                        )
                    }),
                },
            )
            .map_err(ForgeError::from)?,
            None => state
                .ring
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mint(
                    &bee_auth::MintRequest {
                        principal: bee_auth::Principal::agent(
                            grant.agent.id.clone(),
                            if grant.agent.display.is_empty() {
                                grant.agent.id.clone()
                            } else {
                                grant.agent.display.clone()
                            },
                            bee_auth::AgentInfo {
                                owner: bee_policy::PrincipalId::new(grant.owner_id.clone()),
                                runtime: grant.runtime.clone(),
                                purpose: grant.purpose.clone(),
                            },
                        ),
                        on_behalf_of: grant.on_behalf_of.clone(),
                        scope: grant.scope.clone(),
                    },
                    state.now_ms() / 1000,
                )
                .map_err(ForgeError::from)?,
        };
        Ok(bee_forge::agents::ports::IssuedToken {
            token: token.text,
            token_id: token.id.0,
        })
    }

    async fn revoke(&self, token_id: &str) -> FResult<()> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        state
            .revocations
            .revoke(
                state.kv.as_ref(),
                &bee_auth::TokenId(token_id.to_string()),
                state.now_ms(),
            )
            .await
            .map(|_| ())
            .map_err(api_to_forge)
    }
}

// ----- blobs -------------------------------------------------------------------------------

/// Logs and transcripts in the blob store, under the internal [`AGENT_ORG`] org; a reference
/// is the blob id text.
pub struct ServerAgentBlobs {
    state: Weak<AppState>,
}

impl ServerAgentBlobs {
    /// Blobs over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerAgentBlobs {
        ServerAgentBlobs {
            state: Arc::downgrade(state),
        }
    }

    async fn org(&self, state: &AppState) -> FResult<OrgId> {
        let vcs = |e: bee_core::BeeError| bee_forge::changes::vcs::vcs_err(e);
        if let Some(o) = state.meta.org_by_name(AGENT_ORG).await.map_err(vcs)? {
            return Ok(o.id);
        }
        let id = OrgId(crate::util::new_ulid(state.now_ms()).map_err(super::fe)?);
        state
            .meta
            .create_org(OrgRow {
                id,
                name: AGENT_ORG.into(),
                created_at_ms: state.now_ms(),
            })
            .await
            .map_err(vcs)?;
        Ok(state
            .meta
            .org_by_name(AGENT_ORG)
            .await
            .map_err(vcs)?
            .map(|o| o.id)
            .unwrap_or(id))
    }
}

#[async_trait]
impl AgentBlobs for ServerAgentBlobs {
    async fn put(&self, bytes: &[u8]) -> FResult<String> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        let org = self.org(&state).await?;
        let id = bee_object::blob_id(bytes);
        state
            .writer
            .upload(org, vec![(id, bytes.to_vec())], None, state.now_ms())
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?;
        Ok(id.to_text())
    }

    async fn get(&self, reference: &str) -> FResult<Option<Vec<u8>>> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        let Ok(id) = bee_core::ObjectId::parse(reference) else {
            return Ok(None);
        };
        let org = self.org(&state).await?;
        Ok(state
            .blobs
            .get(org, &id)
            .await
            .map_err(bee_forge::changes::vcs::vcs_err)?
            .map(|b| b.to_vec()))
    }
}

// ----- repo --------------------------------------------------------------------------------

/// Reads through the VCS; changesets through `CreateChangeset` as the run's chain.
pub struct ServerAgentRepo {
    state: Weak<AppState>,
    vcs: Arc<HistoryVcs>,
}

impl ServerAgentRepo {
    /// A repo port over `state`.
    pub fn new(state: &Arc<AppState>, vcs: Arc<HistoryVcs>) -> ServerAgentRepo {
        ServerAgentRepo {
            state: Arc::downgrade(state),
            vcs,
        }
    }
}

#[async_trait]
impl AgentRepo for ServerAgentRepo {
    async fn resolve(&self, repo: &Repo, revision: &str) -> FResult<v1::ObjectId> {
        AgentRepo::resolve(self.vcs.as_ref(), repo, revision).await
    }

    async fn read_file(
        &self,
        repo: &Repo,
        at: &v1::ObjectId,
        path: &str,
    ) -> FResult<Option<(v1::ObjectId, Vec<u8>)>> {
        AgentRepo::read_file(self.vcs.as_ref(), repo, at, path).await
    }

    async fn list_files(
        &self,
        repo: &Repo,
        at: &v1::ObjectId,
        prefix: &str,
        limit: usize,
    ) -> FResult<(Vec<RepoFile>, bool)> {
        AgentRepo::list_files(self.vcs.as_ref(), repo, at, prefix, limit).await
    }

    async fn changed_paths(
        &self,
        repo: &Repo,
        from: &v1::ObjectId,
        to: &v1::ObjectId,
    ) -> FResult<Vec<String>> {
        AgentRepo::changed_paths(self.vcs.as_ref(), repo, from, to).await
    }

    async fn commit(
        &self,
        repo: &Repo,
        parent: &v1::ObjectId,
        edits: Vec<(String, Option<Vec<u8>>)>,
        message: &str,
        chain: &[String],
    ) -> FResult<v1::ObjectId> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        let ctx = super::adapters::repo_ctx(&state, &repo.id).await?;
        let actor = chain.first().cloned().unwrap_or_default();
        let mut blobs = Vec::new();
        let mut delta = Vec::new();
        for (path, content) in edits {
            match content {
                Some(bytes) => {
                    let id = bee_object::blob_id(&bytes);
                    delta.push(v1::DeltaEntry {
                        path: path.into_bytes(),
                        op: Some(v1::delta_entry::Op::Put(v1::Put {
                            mode: v1::Mode::File as i32,
                            blob: Some(crate::convert::id_to(&id)),
                            size: bytes.len() as u64,
                        })),
                    });
                    blobs.push((id, bytes));
                }
                None => delta.push(v1::DeltaEntry {
                    path: path.into_bytes(),
                    op: Some(v1::delta_entry::Op::Delete(true)),
                }),
            }
        }
        if !blobs.is_empty() {
            state
                .writer
                .upload(
                    ctx.org.id,
                    blobs,
                    Some(UploadRecord {
                        repo: Some(ctx.repo.id),
                        principal: actor.clone(),
                    }),
                    state.now_ms(),
                )
                .await
                .map_err(bee_forge::changes::vcs::vcs_err)?;
        }
        let token = super::landing::internal_token(&state, chain)
            .await
            .map_err(api_to_forge)?;
        let display = state
            .principal(&actor)
            .await
            .ok()
            .flatten()
            .map(|r| r.display)
            .unwrap_or_else(|| actor.clone());
        let mut change_id = [0u8; 16];
        if let Ok(b) = crate::util::random_bytes(16) {
            change_id.copy_from_slice(&b[..16]);
        }
        let mut req = tonic::Request::new(v1::CreateChangesetRequest {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::Id(repo.id.clone())),
            }),
            parents: vec![parent.clone()],
            delta,
            change_id: Some(v1::ChangeId {
                raw: change_id.to_vec(),
            }),
            author: display.clone(),
            author_time: Some(v1::Signature {
                name: display,
                time: Some(crate::util::timestamp_ms(state.now_ms())),
                tz_offset_minutes: 0,
            }),
            message: message.to_string(),
            ..Default::default()
        });
        if let Ok(v) = format!("Bearer {token}").parse() {
            req.metadata_mut().insert("authorization", v);
        }
        req.extensions_mut().insert(RequestMeta {
            request_id: crate::util::random_hex(8),
            deadline: Instant::now() + Duration::from_millis(state.cfg.limits.max_deadline_ms),
            method: "/bee.v1.AgentService/commit".into(),
        });
        let resp = Writes::new(state.clone())
            .create_changeset(req)
            .await
            .map_err(|s| status_to_forge(&s))?
            .into_inner();
        resp.changeset
            .and_then(|c| c.id)
            .ok_or_else(|| ForgeError::internal("CreateChangeset returned no changeset"))
    }

    async fn set_ref(&self, repo: &Repo, name: &str, target: &v1::ObjectId) -> FResult<u64> {
        AgentRepo::set_ref(self.vcs.as_ref(), repo, name, target).await
    }
}

// ----- runners -----------------------------------------------------------------------------

/// No runner is configured: runs fail at once with `no_runner_configured`.
pub struct NoRunner;

#[async_trait]
impl AgentRunner for NoRunner {
    async fn execute(&self, _job: RunJob, reporter: Arc<dyn RunReporter>) -> FResult<RunOutcome> {
        let _ = reporter
            .report(RunnerEvent::Error(
                "this server has no agent runner (forge.agent_runner)".into(),
            ))
            .await;
        Ok(RunOutcome {
            status: RunStatus::Failed("no_runner_configured".into()),
            ..RunOutcome::default()
        })
    }

    async fn cancel(&self, _run_id: &str) -> FResult<()> {
        Ok(())
    }

    async fn steer(&self, _run_id: &str, _message: &str) -> FResult<()> {
        Err(ForgeError::invalid_state("no agent runner is configured"))
    }
}

/// Runs `sh -c <command>` per run in `<data>/agent-runs/<run id>`, with the prompt on stdin
/// and only these credentials in the environment: `BEE_ENDPOINT`, `BEE_TOKEN` (the run
/// token), `BEE_REPO`, `BEE_RUN_ID`, `BEE_BASE_REF`, `BEE_WORK_REF`,
/// `BEE_RUN_CHAIN` (comma-separated, agent first) and `BEE_STEER_FILE` (steering messages
/// are appended to it, one per line). Each output line becomes a log event; exit status 0
/// succeeds, anything else fails with `exit_<code>`; the run's deadline and `cancel` kill the
/// process.
pub struct CommandRunner {
    command: String,
    endpoint: String,
    root: std::path::PathBuf,
    state: Weak<AppState>,
    kills: Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<()>>>,
}

impl CommandRunner {
    /// A runner for `command`, pointing runs at `endpoint`, working under `root`.
    pub fn new(
        command: &str,
        endpoint: &str,
        root: std::path::PathBuf,
        state: &Arc<AppState>,
    ) -> CommandRunner {
        CommandRunner {
            command: command.to_string(),
            endpoint: endpoint.to_string(),
            root,
            state: Arc::downgrade(state),
            kills: Mutex::new(BTreeMap::new()),
        }
    }

    fn dir(&self, run_id: &str) -> std::path::PathBuf {
        let safe: String = run_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        self.root.join(safe)
    }

    async fn is_cancelled(&self, run_id: &str) -> FResult<bool> {
        let state = self.state.upgrade().ok_or_else(stopping)?;
        let wiring = state
            .hub
            .get()
            .ok_or_else(|| ForgeError::unavailable("forge is not ready"))?;
        let run = wiring
            .agents
            .get_run(
                &bee_forge::Cx::system().with_timeout(RUN_CANCEL_POLL_TIMEOUT),
                run_id,
            )
            .await?;
        Ok(run.state == v1::AgentRunState::Cancelled as i32)
    }
}

async fn kill_command(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let process_group = format!("-{pid}");
        let _ = tokio::process::Command::new("/bin/kill")
            .args(["-KILL", "--", process_group.as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[async_trait]
impl AgentRunner for CommandRunner {
    async fn execute(&self, job: RunJob, reporter: Arc<dyn RunReporter>) -> FResult<RunOutcome> {
        let io = |e: std::io::Error| ForgeError::unavailable(format!("agent runner: {e}"));
        let dir = self.dir(&job.run.id);
        std::fs::create_dir_all(&dir).map_err(io)?;
        let steer = dir.join("steering.txt");
        let mut command = tokio::process::Command::new("sh");
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .arg("-c")
            .arg(&self.command)
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .env("BEE_ENDPOINT", &self.endpoint)
            .env("BEE_TOKEN", &job.token)
            .env("BEE_REPO", &job.repo.full_name)
            .env("BEE_RUN_ID", &job.run.id)
            .env("BEE_BASE_REF", &job.run.base_ref)
            .env("BEE_WORK_REF", &job.run.work_ref)
            .env("BEE_RUN_CHAIN", job.chain.join(","))
            .env("BEE_STEER_FILE", &steer)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(io)?;
        if let Some(mut stdin) = child.stdin.take() {
            let mut input = job.prompt.clone();
            if !job.instructions.is_empty() {
                input = format!("{}\n\n{input}", job.instructions);
            }
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
        self.kills
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job.run.id.clone(), kill_tx);
        let mut log = Vec::new();
        let mut last = String::new();
        let mut out = child.stdout.take().map(|s| BufReader::new(s).lines());
        let mut err = child.stderr.take().map(|s| BufReader::new(s).lines());
        let mut cancel_poll = tokio::time::interval(RUN_CANCEL_POLL_INTERVAL);
        cancel_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        cancel_poll.tick().await;
        let deadline = (job.deadline_ms > 0).then(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            tokio::time::Instant::now() + Duration::from_millis(job.deadline_ms.saturating_sub(now))
        });
        let mut kill_rx = kill_rx;
        let status = loop {
            let timeout = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = async { match out.as_mut() { Some(o) => o.next_line().await, None => std::future::pending().await } } => {
                    match line {
                        Ok(Some(l)) => {
                            log.extend_from_slice(l.as_bytes());
                            log.push(b'\n');
                            last.clone_from(&l);
                            let _ = reporter.report(RunnerEvent::Log(l)).await;
                        }
                        _ => out = None,
                    }
                }
                line = async { match err.as_mut() { Some(e) => e.next_line().await, None => std::future::pending().await } } => {
                    match line {
                        Ok(Some(l)) => {
                            log.extend_from_slice(l.as_bytes());
                            log.push(b'\n');
                            let _ = reporter.report(RunnerEvent::Log(l)).await;
                        }
                        _ => err = None,
                    }
                }
                s = child.wait(), if out.is_none() && err.is_none() => {
                    break match s {
                        Ok(s) if s.success() => RunStatus::Succeeded,
                        Ok(s) => RunStatus::Failed(format!("exit_{}", s.code().unwrap_or(-1))),
                        Err(e) => RunStatus::Failed(format!("runner_error: {e}")),
                    };
                }
                _ = &mut kill_rx => {
                    kill_command(&mut child).await;
                    break RunStatus::Cancelled;
                }
                _ = cancel_poll.tick() => {
                    match self.is_cancelled(&job.run.id).await {
                        Ok(true) => {
                            kill_command(&mut child).await;
                            break RunStatus::Cancelled;
                        }
                        Ok(false) => {}
                        Err(error) => crate::logging::log(
                            crate::logging::Level::Debug,
                            "forge.agent_runner",
                            "failed to poll agent run cancellation status",
                            &[
                                ("run_id", serde_json::Value::from(job.run.id.as_str())),
                                ("reason", serde_json::Value::from(error.reason_name())),
                            ],
                        ),
                    }
                }
                _ = timeout => {
                    kill_command(&mut child).await;
                    break RunStatus::Failed("DEADLINE_EXCEEDED".into());
                }
            }
        };
        self.kills
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&job.run.id);
        Ok(RunOutcome {
            status,
            summary: last,
            log,
            ..RunOutcome::default()
        })
    }

    async fn cancel(&self, run_id: &str) -> FResult<()> {
        if let Some(k) = self
            .kills
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run_id)
        {
            let _ = k.send(());
        }
        Ok(())
    }

    async fn steer(&self, run_id: &str, message: &str) -> FResult<()> {
        use std::io::Write;
        let path = self.dir(run_id).join("steering.txt");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| ForgeError::unavailable(format!("agent runner: {e}")))?;
        writeln!(f, "{}", message.replace('\n', " "))
            .map_err(|e| ForgeError::unavailable(format!("agent runner: {e}")))
    }
}
