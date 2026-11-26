/** The runner operations used on the repository's work and principal pages. */
import type { AgentService } from "../../gen/bee/v1/agents_pb";
import { BeeError } from "../transport";
import type { AgentRun, AgentRunEvent, PageRequest } from "../types";
import type { Ctx, Impl } from "./router";
import { ulid } from "./seed";
import type { FixtureStore, RepoState } from "./store";
import { invalid } from "./util";

const QUEUED = "AGENT_RUN_STATE_QUEUED";
const RUNNING = "AGENT_RUN_STATE_RUNNING";
const CANCELLED = "AGENT_RUN_STATE_CANCELLED";
const STATE_CHANGED = "AGENT_RUN_EVENT_KIND_STATE_CHANGED";
const STEERING_MESSAGE = "AGENT_RUN_EVENT_KIND_STEERING_MESSAGE";

const terminal = (run: AgentRun) => run.state === "AGENT_RUN_STATE_SUCCEEDED" || run.state === "AGENT_RUN_STATE_FAILED" || run.state === CANCELLED;
const done = (run: AgentRun) => new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "Run " + run.id + " already finished" });
const denied = (message: string) => new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message });

/** Include both the session log and messages delivered by the swarm's lead-only endpoint. */
function events(s: FixtureStore, id: string): AgentRunEvent[] {
  return [...(s.agentRunEvents.get(id) ?? []), ...(s.swarms.messages.get(id) ?? [])]
    .sort((a, b) => Number(BigInt(a.seq!) - BigInt(b.seq!)));
}

function append(s: FixtureStore, run: AgentRun, fields: Pick<AgentRunEvent, "kind" | "state" | "text" | "author">, time = s.iso(s.now())) {
  const id = run.id!;
  const seq = String(BigInt(run.lastEventSeq ?? "0") + 1n);
  const event = { runId: id, seq, time, ...fields };
  const log = s.agentRunEvents.get(id) ?? [];
  log.push(event);
  s.agentRunEvents.set(id, log);
  run.lastEventSeq = seq;
  return event;
}

function emit(s: FixtureStore, repo: RepoState, actor: string, run: AgentRun, kind: string) {
  s.emit(repo, kind, actor, { subject: { kind: "SUBJECT_KIND_AGENT_RUN", repo: { id: repo.repo.id }, id: run.id! } });
}

function lookup(s: FixtureStore, ctx: Ctx, id: string | undefined) {
  const actor = s.caller(ctx.token);
  if (!id) throw invalid("A run ID is required");
  for (const repo of s.readableRepos(actor)) {
    const run = s.swarms.repoRuns(s, repo).find((row) => row.id === id);
    if (run) return { actor, repo, run };
  }
  throw new BeeError("not_found", { reason: "NOT_FOUND", message: "No agent run " + id });
}

function steer(s: FixtureStore, ctx: Ctx, id: string | undefined) {
  const found = lookup(s, ctx, id);
  const { actor, repo, run } = found;
  if (repo.archived) throw new BeeError("failed_precondition", { reason: "ARCHIVED", message: "This repository is archived" });
  const me = s.p(actor).id;
  const drives = me === run.startedBy?.id || me === run.agent?.id || me === run.agent?.ownerId || s.posts.isAdmin(s, actor, repo);
  if (!s.posts.canWrite(s, actor, repo) || !drives) throw denied("You may not change this run");
  return found;
}

/** The wire uses an exclusive cursor: a run ID for descending lists, a sequence for events. */
function pageBy<T>(rows: T[], req: PageRequest | undefined, key: (row: T) => string) {
  const size = Math.min(req?.pageSize || 50, 500);
  const items = rows.slice(0, size);
  return { items, page: { nextPageToken: rows.length > size ? key(items[size - 1]!) : "" } };
}

function checkLength(field: string, value: string, max: number) {
  const actual = new TextEncoder().encode(value).length;
  if (actual > max) throw new BeeError("invalid_argument", { reason: "TOO_LARGE", message: field + " is too large" });
}

/** A live independent run exercises steering and cancellation alongside the adoption history. */
export function seedAgentRuns(s: FixtureStore) {
  const repo = s.repo({ fullName: "acme/api" });
  const started = s.ago(95);
  const created = s.ago(96);
  const id = ulid(created, "agent-run:health-route-logging");
  const agent = s.p("watcher");
  const run: AgentRun = {
    id, repo: { id: repo.repo.id },
    task: { kind: "AGENT_TASK_KIND_FREEFORM", prompt: "Find what makes the health route return a slow response during startup." },
    agent, startedBy: s.p("mara"), model: s.agentMeta.get("watcher")?.model,
    state: RUNNING, baseRef: "main", workRef: "ws/" + agent.id + "/" + id.toLowerCase(),
    scope: { readPaths: [], writePaths: [], refs: [], actions: [] },
    budget: { maxUsdMicros: "1500000" },
    cost: { inputTokens: "1400", outputTokens: "240", usdMicros: "18000" },
    session: "run-" + id.toLowerCase(), createdAt: s.iso(created), startedAt: s.iso(started), trigger: "manual",
  };
  s.swarms.runs.push(run);
  append(s, run, { kind: STATE_CHANGED, state: QUEUED }, s.iso(created));
  append(s, run, { kind: STATE_CHANGED, state: RUNNING }, s.iso(started));
  append(s, run, { kind: "AGENT_RUN_EVENT_KIND_LOG", text: "Read the startup sequence and located the health route checks." }, s.iso(s.ago(90)));
}

export const agentRunImpl: Pick<Impl<typeof AgentService>, "getAgentRun" | "listAgentRuns" | "listAgentRunEvents" | "sendAgentRunMessage" | "cancelAgentRun" | "retryAgentRun"> = {
  getAgentRun: (s, req, ctx) => lookup(s, ctx, req.id).run,
  listAgentRuns: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const hasRepo = req.repo?.id || req.repo?.fullName;
    if (!hasRepo && !req.agentPrincipalId) throw invalid("A repository or agent ID is required");
    const repos = hasRepo ? [s.repo(req.repo)] : s.readableRepos(actor);
    const after = req.page?.pageToken ?? "";
    const rows = repos.flatMap((repo) => s.swarms.repoRuns(s, repo))
      .filter((run) => !req.agentPrincipalId || run.agent?.id === req.agentPrincipalId)
      .filter((run) => !req.startedById || run.startedBy?.id === req.startedById)
      .filter((run) => !req.states?.length || req.states.some((state) => state === run.state))
      .filter((run) => !req.taskKind || req.taskKind === "AGENT_TASK_KIND_UNSPECIFIED" || req.taskKind === run.task?.kind)
      .filter((run) => !after || run.id! < after)
      .sort((a, b) => b.id!.localeCompare(a.id!));
    const result = pageBy(rows, req.page, (run) => run.id!);
    return { runs: result.items, page: result.page };
  },
  listAgentRunEvents: (s, req, ctx) => {
    const { run } = lookup(s, ctx, req.runId);
    const token = req.page?.pageToken ?? "";
    const after = BigInt(/^\d+$/.test(token) ? token : (req.afterSeq ?? "0"));
    const rows = events(s, run.id!).filter((event) => BigInt(event.seq!) > after && (!req.kinds?.length || req.kinds.some((kind) => kind === event.kind)));
    const result = pageBy(rows, req.page, (event) => event.seq!);
    return { events: result.items, page: result.page };
  },
  sendAgentRunMessage: (s, req, ctx) => {
    const { actor, repo, run } = steer(s, ctx, req.runId);
    const swarm = s.swarms.swarms.find((row) => row.memberRunIds.includes(run.id!));
    if (swarm) {
      if (swarm.leadRunId !== run.id || s.kindOf(actor) !== "human") throw denied("Only a person may message the lead of a swarm");
      if (swarm.state === "SWARM_STATE_STOPPED" || swarm.state === "SWARM_STATE_FINISHED") {
        throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "This swarm has ended" });
      }
    }
    if (terminal(run)) throw done(run);
    const body = req.body ?? "";
    if (!body.trim()) throw invalid("A message is required");
    checkLength("Message", body, 65_536);
    const event = append(s, run, { kind: STEERING_MESSAGE, text: body, author: s.p(actor) });
    emit(s, repo, actor, run, "agent_run.message");
    return event;
  },
  cancelAgentRun: (s, req, ctx) => {
    const { actor, repo, run } = steer(s, ctx, req.id);
    const reason = req.reason ?? "";
    checkLength("Reason", reason, 4096);
    if (terminal(run)) throw done(run);
    run.state = CANCELLED;
    run.finishedAt = s.iso(s.now());
    run.failureReason = "CANCELLED";
    if (reason) run.summary = reason;
    append(s, run, { kind: STATE_CHANGED, state: CANCELLED, text: "Stopped by " + s.p(actor).display + (reason ? ": " + reason : "") });
    emit(s, repo, actor, run, "agent_run.completed");
    return run;
  },
  retryAgentRun: (s, req, ctx) => {
    const { actor, repo, run } = steer(s, ctx, req.id);
    if (!terminal(run)) throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "This run has not finished" });
    const id = s.swarms.key(s, "agent-run");
    const retry: AgentRun = {
      id, repo: { id: repo.repo.id }, task: run.task, runtimeId: req.runtimeId || run.runtimeId,
      model: run.model, agent: run.agent, startedBy: s.p(actor), state: QUEUED, baseRef: run.baseRef,
      workRef: "ws/" + run.agent?.id + "/" + id.toLowerCase(), scope: run.scope,
      budget: req.budget ?? run.budget, session: "run-" + id.toLowerCase(), createdAt: s.iso(s.now()),
      trigger: "retry", triggerRef: run.id,
    };
    s.swarms.runs.push(retry);
    append(s, retry, { kind: STATE_CHANGED, state: QUEUED });
    emit(s, repo, actor, retry, "agent_run.queued");
    return retry;
  },
};
