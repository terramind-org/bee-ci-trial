//! What the agents module needs from the rest of the system, with the implementations that
//! live here.
//!
//! | port | does | here |
//! |---|---|---|
//! | [`AgentRunner`] | executes a run in a sandbox with its runtime (Claude Code, Codex, ...) | [`crate::agents::fake::FakeRunner`] (deterministic, tests and `--dev`) |
//! | [`RunTokenIssuer`] | mints or attenuates the run's delegated token and revokes it | [`KeyRingIssuer`] over a `bee_auth::KeyRing` |
//! | [`AgentBlobs`] | stores run logs and transcripts | [`MemoryBlobs`] |
//! | [`AgentRepo`] | reads repo files, diffs, writes workspace changesets and refs | `impl AgentRepo for HistoryVcs` |
//! | [`WorkspaceProvider`] | provisions, starts, stops and snapshots workspaces | [`crate::agents::local::LocalProvider`] |
//!
//! The server supplies production implementations (a sandboxed runner fleet, the signing key
//! ring and revocation store, the blob store, its VCS, a container or VM fleet).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_auth::{
    AgentInfo, Attenuation, InMemoryRevocations, KeyRing, MintRequest, PrincipalRef, Scope, TokenId,
};
use bee_core::{Mode, ObjectId as CoreId, RepoPath};
use bee_proto::v1::{
    AgentRun, AgentRuntime, ChangeRequest, Cost, Issue, ObjectId, Principal, Repo, Workspace,
};

use super::registry::AgentDefinition;
use crate::changes::vcs::{ChangeVcs, HistoryVcs, NewCommit, vcs_err};
use crate::error::{ForgeError, Result};
use crate::ids::{Clock, hex};

// ---- runner ----

/// One thing a runner reports while a run executes.
#[derive(Clone, Debug, PartialEq)]
pub enum RunnerEvent {
    /// A log line.
    Log(String),
    /// A reasoning summary the runtime chose to share.
    Thought(String),
    /// A tool call with JSON arguments.
    ToolCall {
        /// Tool name.
        name: String,
        /// JSON arguments (truncated and secret-scrubbed by the runner).
        payload: String,
    },
    /// A tool result.
    ToolResult {
        /// Tool name.
        name: String,
        /// JSON result.
        payload: String,
    },
    /// A changeset was created.
    ChangesetCreated(ObjectId),
    /// A change request was opened (its number).
    ChangeRequestOpened(u64),
    /// A lease was acquired (its id).
    LeaseAcquired(String),
    /// A check ran.
    CheckResult {
        /// Check name.
        name: String,
        /// Result text.
        result: String,
    },
    /// Cumulative cost so far.
    Cost(Cost),
    /// An error the run recovered from or ended with.
    Error(String),
}

/// Receives a run's events as they happen (they become `AgentRunEvent`s).
#[async_trait]
pub trait RunReporter: Send + Sync {
    /// Records one event.
    async fn report(&self, event: RunnerEvent) -> Result<()>;
}

/// Everything a runner needs to execute one run.
#[derive(Clone, Debug, Default)]
pub struct RunJob {
    /// The run (state `RUNNING`).
    pub run: AgentRun,
    /// The repo.
    pub repo: Repo,
    /// The delegated token: the only credential the runtime gets.
    pub token: String,
    /// Delegation chain of the token, agent first.
    pub chain: Vec<String>,
    /// The agent definition.
    pub definition: AgentDefinition,
    /// The registered runtime, when the definition names one.
    pub runtime: Option<AgentRuntime>,
    /// Resolved repo instructions (AGENTS.md and friends plus the definition's file).
    pub instructions: String,
    /// The task prompt as the runtime receives it.
    pub prompt: String,
    /// The issue, for issue runs.
    pub issue: Option<Issue>,
    /// The change request, for review and revise runs.
    pub change_request: Option<ChangeRequest>,
    /// The workspace the run works in, when one was provisioned.
    pub workspace: Option<Workspace>,
    /// Steering messages sent before the run started.
    pub messages: Vec<String>,
    /// Wall-clock limit (ms since the epoch); 0 = none.
    pub deadline_ms: u64,
}

/// How a run ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RunStatus {
    /// Done; with open change requests the run waits for review.
    #[default]
    Succeeded,
    /// Failed with a reason (an `ErrorReason` name or a runner code).
    Failed(String),
    /// Stopped because it was cancelled.
    Cancelled,
}

/// What a runner returns when a run stops.
#[derive(Clone, Debug, Default)]
pub struct RunOutcome {
    /// End state.
    pub status: RunStatus,
    /// Final summary, Markdown.
    pub summary: String,
    /// Model actually used (empty = the requested one).
    pub model: String,
    /// Total cost.
    pub cost: Cost,
    /// Changesets created.
    pub changesets: Vec<ObjectId>,
    /// Change requests opened (numbers).
    pub change_requests: Vec<u64>,
    /// Leases acquired.
    pub lease_ids: Vec<String>,
    /// Raw log.
    pub log: Vec<u8>,
    /// Full transcript.
    pub transcript: Vec<u8>,
}

/// Executes runs. The server's implementation runs the runtime's container in a sandbox
/// under the workspace egress policy, passing only the delegated token.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Executes `job` until it stops, reporting events as they happen.
    async fn execute(&self, job: RunJob, reporter: Arc<dyn RunReporter>) -> Result<RunOutcome>;
    /// Asks a running run to stop.
    async fn cancel(&self, run_id: &str) -> Result<()>;
    /// Delivers a steering message to a running run.
    async fn steer(&self, run_id: &str, message: &str) -> Result<()>;
}

// ---- tokens ----

/// What a run's token may do and on whose behalf.
#[derive(Clone, Debug)]
pub struct RunGrant {
    /// The run.
    pub run_id: String,
    /// The agent principal the token acts as.
    pub agent: Principal,
    /// The agent's primary owner.
    pub owner_id: String,
    /// Runtime name.
    pub runtime: String,
    /// What the agent is for.
    pub purpose: String,
    /// Delegators, nearest first: the owner first, the human or service root last.
    pub on_behalf_of: Vec<PrincipalRef>,
    /// The starter's own token, when the run was started through the API: the run token is
    /// derived from it, so it can never be broader.
    pub parent_token: Option<String>,
    /// The principal of `parent_token`.
    pub parent_principal: String,
    /// The attenuated scope (repo, paths, refs, actions, budget, session, expiry).
    pub scope: Scope,
}

/// An issued run token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedToken {
    /// Token text (a bearer token).
    pub token: String,
    /// Token id (revocation id of its last block).
    pub token_id: String,
}

/// Issues and revokes delegated run tokens.
#[async_trait]
pub trait RunTokenIssuer: Send + Sync {
    /// Issues a token for `grant`.
    async fn issue(&self, grant: &RunGrant) -> Result<IssuedToken>;
    /// Revokes a token (and everything derived from it).
    async fn revoke(&self, token_id: &str) -> Result<()>;
}

/// Issues run tokens with a `bee_auth` key ring: attenuates the starter's token when there
/// is one (adding the agent with `act_as`), otherwise mints a token for the agent on behalf of
/// its owner and the delegation chain.
pub struct KeyRingIssuer {
    ring: KeyRing,
    revocations: Option<Arc<InMemoryRevocations>>,
    clock: Arc<dyn Clock>,
}

impl KeyRingIssuer {
    /// An issuer signing with `ring`; `revocations` receives revoked run tokens.
    pub fn new(
        ring: KeyRing,
        revocations: Option<Arc<InMemoryRevocations>>,
        clock: Arc<dyn Clock>,
    ) -> KeyRingIssuer {
        KeyRingIssuer {
            ring,
            revocations,
            clock,
        }
    }
}

#[async_trait]
impl RunTokenIssuer for KeyRingIssuer {
    async fn issue(&self, g: &RunGrant) -> Result<IssuedToken> {
        let token = match &g.parent_token {
            Some(parent) => {
                let act_as = (g.parent_principal != g.agent.id).then(|| {
                    PrincipalRef::new(g.agent.id.clone(), bee_policy::PrincipalKind::Agent)
                });
                bee_auth::attenuate(
                    parent,
                    &Attenuation {
                        scope: g.scope.clone(),
                        act_as,
                    },
                )?
            }
            None => {
                let principal = bee_auth::Principal::agent(
                    g.agent.id.clone(),
                    if g.agent.display.is_empty() {
                        g.agent.id.clone()
                    } else {
                        g.agent.display.clone()
                    },
                    AgentInfo {
                        owner: bee_policy::PrincipalId::new(g.owner_id.clone()),
                        runtime: g.runtime.clone(),
                        purpose: g.purpose.clone(),
                    },
                );
                self.ring.mint(
                    &MintRequest {
                        principal,
                        on_behalf_of: g.on_behalf_of.clone(),
                        scope: g.scope.clone(),
                    },
                    self.clock.now_ms() / 1000,
                )?
            }
        };
        Ok(IssuedToken {
            token: token.text,
            token_id: token.id.0,
        })
    }

    async fn revoke(&self, token_id: &str) -> Result<()> {
        if let Some(r) = &self.revocations {
            r.revoke(&TokenId(token_id.to_string()));
        }
        Ok(())
    }
}

// ---- blobs ----

/// Stores run logs and transcripts by content.
#[async_trait]
pub trait AgentBlobs: Send + Sync {
    /// Stores `bytes` and returns a reference (`blake3:<hex>`).
    async fn put(&self, bytes: &[u8]) -> Result<String>;
    /// The bytes of a reference, `None` when absent.
    async fn get(&self, reference: &str) -> Result<Option<Vec<u8>>>;
}

/// The content reference of `bytes`.
pub fn blob_ref(bytes: &[u8]) -> String {
    format!("blake3:{}", hex(blake3::hash(bytes).as_bytes()))
}

/// Blobs in memory.
#[derive(Default)]
pub struct MemoryBlobs {
    map: RwLock<BTreeMap<String, Vec<u8>>>,
}

impl MemoryBlobs {
    /// An empty store.
    pub fn new() -> MemoryBlobs {
        MemoryBlobs::default()
    }
}

#[async_trait]
impl AgentBlobs for MemoryBlobs {
    async fn put(&self, bytes: &[u8]) -> Result<String> {
        let r = blob_ref(bytes);
        self.map
            .write()
            .map_err(|_| ForgeError::internal("blob map poisoned"))?
            .insert(r.clone(), bytes.to_vec());
        Ok(r)
    }

    async fn get(&self, reference: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .map
            .read()
            .map_err(|_| ForgeError::internal("blob map poisoned"))?
            .get(reference)
            .cloned())
    }
}

// ---- repo content ----

/// A file in a repo at a changeset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoFile {
    /// Path from the repo root, `/`-separated, no leading slash.
    pub path: String,
    /// Blob id.
    pub blob: ObjectId,
    /// Size in bytes (when known).
    pub size: u64,
}

/// Repo reads and the writes workspaces need.
#[async_trait]
pub trait AgentRepo: Send + Sync {
    /// The changeset a revision names: empty = the default ref, a ref name, or a full hex id.
    async fn resolve(&self, repo: &Repo, revision: &str) -> Result<ObjectId>;
    /// A file's blob id and bytes, `None` when absent or not a file.
    async fn read_file(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
    ) -> Result<Option<(ObjectId, Vec<u8>)>>;
    /// Files under `prefix` (`""` = everything), at most `limit`, sorted by path; the flag
    /// says whether the list was cut.
    async fn list_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
        prefix: &str,
        limit: usize,
    ) -> Result<(Vec<RepoFile>, bool)>;
    /// Paths that differ between two changesets.
    async fn changed_paths(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
    ) -> Result<Vec<String>>;
    /// Creates a changeset on `parent` with `edits` (write `Some`, delete `None`).
    async fn commit(
        &self,
        repo: &Repo,
        parent: &ObjectId,
        edits: Vec<(String, Option<Vec<u8>>)>,
        message: &str,
        chain: &[String],
    ) -> Result<ObjectId>;
    /// Points `name` at `target` (workspace refs); returns the new ref version.
    async fn set_ref(&self, repo: &Repo, name: &str, target: &ObjectId) -> Result<u64>;
}

fn core_id(id: &ObjectId) -> Result<CoreId> {
    CoreId::from_bytes(&id.raw).map_err(|e| ForgeError::invalid("invalid_object_id", e.message))
}

fn proto_id(id: &CoreId) -> ObjectId {
    ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

/// Parses a full hex object id.
pub fn parse_hex_id(s: &str) -> Option<ObjectId> {
    let raw = crate::ids::unhex(s)?;
    CoreId::from_bytes(&raw).ok().map(|id| proto_id(&id))
}

#[async_trait]
impl AgentRepo for HistoryVcs {
    async fn resolve(&self, repo: &Repo, revision: &str) -> Result<ObjectId> {
        let rev = revision.trim();
        let name = if rev.is_empty() {
            repo.default_ref.as_str()
        } else {
            rev
        };
        if let Some(r) = self.ref_tip(repo, name).await?
            && let Some(t) = r.target
        {
            return Ok(t);
        }
        if let Some(id) = parse_hex_id(name) {
            return Ok(id);
        }
        Err(ForgeError::not_found(format!("revision {name}")))
    }

    async fn read_file(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
    ) -> Result<Option<(ObjectId, Vec<u8>)>> {
        let Some(id) = self.file_id(repo, at, path).await? else {
            return Ok(None);
        };
        Ok(ChangeVcs::read_file(self, repo, at, path)
            .await?
            .map(|b| (id, b)))
    }

    async fn list_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
        prefix: &str,
        limit: usize,
    ) -> Result<(Vec<RepoFile>, bool)> {
        let h = self.history(repo).await?;
        let root = *h.changeset(&core_id(at)?).await.map_err(vcs_err)?.root();
        let prefix = prefix.trim_matches('/');
        let start = if prefix.is_empty() {
            root
        } else {
            let rp = RepoPath::new(prefix.as_bytes().to_vec())
                .map_err(|e| ForgeError::invalid("invalid_path", e.message))?;
            match h.entry_at(&root, &rp).await.map_err(vcs_err)? {
                Some(e) if e.mode == Mode::Tree => e.id,
                Some(e) if e.is_leaf() && e.mode != Mode::Gitlink => {
                    return Ok((
                        vec![RepoFile {
                            path: prefix.to_string(),
                            blob: proto_id(&e.id),
                            size: 0,
                        }],
                        false,
                    ));
                }
                _ => return Ok((Vec::new(), false)),
            }
        };
        let mut out = Vec::new();
        let mut stack = vec![(prefix.to_string(), start)];
        let mut truncated = false;
        while let Some((dir, tree)) = stack.pop() {
            let t = h.tree(&tree).await.map_err(vcs_err)?;
            for e in t.entries() {
                let name = String::from_utf8_lossy(&e.name).into_owned();
                let path = if dir.is_empty() {
                    name
                } else {
                    format!("{dir}/{name}")
                };
                if e.mode == Mode::Tree {
                    stack.push((path, e.id));
                } else if e.mode != Mode::Gitlink {
                    if out.len() >= limit {
                        truncated = true;
                        break;
                    }
                    out.push(RepoFile {
                        path,
                        blob: proto_id(&e.id),
                        size: 0,
                    });
                }
            }
            if truncated {
                break;
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((out, truncated))
    }

    async fn changed_paths(
        &self,
        repo: &Repo,
        from: &ObjectId,
        to: &ObjectId,
    ) -> Result<Vec<String>> {
        let items = self.diff(repo, from, to, &[], false, 0).await?;
        let mut out: Vec<String> = Vec::new();
        for i in items {
            for p in [&i.path, &i.old_path] {
                if !p.is_empty() {
                    out.push(String::from_utf8_lossy(p).into_owned());
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn commit(
        &self,
        repo: &Repo,
        parent: &ObjectId,
        edits: Vec<(String, Option<Vec<u8>>)>,
        message: &str,
        chain: &[String],
    ) -> Result<ObjectId> {
        let actor = chain.first().cloned().unwrap_or_default();
        self.create_commit(
            repo,
            NewCommit {
                parent: Some(parent.clone()),
                edits,
                message: message.to_string(),
                author: actor.clone(),
                creator: actor,
                chain: chain.to_vec(),
                change_id: None,
            },
        )
        .await
    }

    async fn set_ref(&self, repo: &Repo, name: &str, target: &ObjectId) -> Result<u64> {
        HistoryVcs::set_ref(self, repo, name, target).await
    }
}

// ---- workspaces ----

/// What a provider reports after provisioning or starting a workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provisioned {
    /// Provider handle (container id, VM id, directory).
    pub handle: String,
    /// Ports the container listens on, with labels and protocols.
    pub ports: Vec<(u32, String, String)>,
}

/// Connection endpoints of a running workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoints {
    /// Web IDE URL.
    pub web_ide_url: String,
    /// SSH host.
    pub ssh_host: String,
    /// SSH port.
    pub ssh_port: u32,
    /// SSH user.
    pub ssh_user: String,
}

/// The files a workspace changed relative to its base changeset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceEdits {
    /// Writes (`Some`) and deletions (`None`), sorted by path.
    pub edits: Vec<(String, Option<Vec<u8>>)>,
}

/// What a provider needs to know to build a workspace.
#[derive(Clone, Debug, Default)]
pub struct BuildRequest {
    /// The workspace.
    pub workspace: Workspace,
    /// The repo.
    pub repo: Repo,
    /// The parsed devcontainer spec.
    pub spec: super::devcontainer::DevcontainerSpec,
    /// The machine type.
    pub machine: super::workspaces::MachineType,
    /// The prebuild handle to start from, if any.
    pub prebuild_handle: String,
    /// Dotfiles repo and install command, if enabled.
    pub dotfiles: Option<(String, String)>,
    /// Effective egress policy (allowed domains, CIDRs, default deny).
    pub egress: bee_proto::v1::WorkspaceEgressPolicy,
}

/// Provisions and runs workspaces. The server's implementation drives a container or VM
/// fleet; [`crate::agents::local::LocalProvider`] materializes working copies on disk.
#[async_trait]
pub trait WorkspaceProvider: Send + Sync {
    /// Creates and starts a workspace.
    async fn provision(&self, req: &BuildRequest) -> Result<Provisioned>;
    /// Starts a stopped workspace (`handle` from [`WorkspaceProvider::provision`]).
    async fn start(&self, handle: &str, req: &BuildRequest) -> Result<Provisioned>;
    /// Stops a running workspace, keeping its disk.
    async fn stop(&self, handle: &str) -> Result<()>;
    /// Rebuilds the container, keeping the working copy.
    async fn rebuild(&self, handle: &str, req: &BuildRequest, full: bool) -> Result<Provisioned>;
    /// Deletes the workspace and its disk.
    async fn destroy(&self, handle: &str) -> Result<()>;
    /// The working copy's edits relative to the base changeset.
    async fn edits(&self, handle: &str) -> Result<WorkspaceEdits>;
    /// Marks the current working copy as the new base (after an export).
    async fn rebase(&self, handle: &str) -> Result<()>;
    /// Connection endpoints.
    async fn endpoints(&self, handle: &str, workspace: &Workspace) -> Result<Endpoints>;
    /// Builds a prebuild for a changeset; returns its handle.
    async fn prebuild(&self, req: &BuildRequest, changeset: &ObjectId) -> Result<String>;
    /// Deletes a prebuild's image.
    async fn delete_prebuild(&self, handle: &str) -> Result<()>;
}
