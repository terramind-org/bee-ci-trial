/** Swarm coordination in the deterministic demo repository. */
import { SwarmService, type SwarmRefJson } from "../../gen/bee/v1/swarms_pb";
import { base64ToBytes, pathBytes } from "../ids";
import { BeeError, type ConnectCode } from "../transport";
import type {
  AgentRun,
  AgentRunBudget,
  AgentRunEvent,
  Cost,
  Principal,
  RepoRef,
  RepoWorkItem,
  Swarm,
  SwarmAttempt,
  SwarmAttemptOutcome,
  SwarmHandBack,
  SwarmJoin,
  SwarmState,
  SwarmTask,
  SwarmTaskState,
} from "../types";
import { mayJoinSwarm } from "./policy";
import { feedAgentRun } from "./posts";
import { defineService, type Ctx } from "./router";
import { ulid } from "./seed";
import type { FixtureStore, RepoState } from "./store";

const PLANNING = "SWARM_STATE_PLANNING";
const WORKING = "SWARM_STATE_WORKING";
const PAUSED = "SWARM_STATE_PAUSED";
const STOPPED = "SWARM_STATE_STOPPED";
const FINISHED = "SWARM_STATE_FINISHED";

const TASK_OPEN = "SWARM_TASK_STATE_OPEN";
const TASK_WORKING = "SWARM_TASK_STATE_WORKING";
const TASK_FINISHED = "SWARM_TASK_STATE_FINISHED";
const TASK_EMPTY = "SWARM_TASK_STATE_EMPTY";

const DONE = "SWARM_ATTEMPT_OUTCOME_FINISHED";
const EMPTY = "SWARM_ATTEMPT_OUTCOME_EMPTY";
const REFUSED = "SWARM_ATTEMPT_OUTCOME_REFUSED";
const IN_PROGRESS = "SWARM_ATTEMPT_OUTCOME_UNSPECIFIED";

const PENDING = "SWARM_JOIN_STATE_PENDING";
const CANCELLED = "AGENT_RUN_STATE_CANCELLED";

const terminal = (run: AgentRun): boolean => run.state === "AGENT_RUN_STATE_SUCCEEDED" || run.state === "AGENT_RUN_STATE_FAILED" || run.state === CANCELLED;
// Run details share this feed, so a persisted log or steering event counts as activity.
const runActivity = (s: FixtureStore, run: AgentRun): string =>
  [run.finishedAt, run.startedAt, run.createdAt, s.agentRunEvents.get(run.id!)?.at(-1)?.time, s.swarms.messages.get(run.id!)?.at(-1)?.time]
    .filter((at): at is string => !!at)
    .sort()
    .at(-1) ?? "";
const zeroCost: Cost = { inputTokens: "0", outputTokens: "0", usdMicros: "0" };

const budgetKeys = ["maxInputTokens", "maxOutputTokens", "maxUsdMicros", "maxDurationSeconds", "maxChangesets", "maxLanded"] as const;
const maxU64 = 2n ** 64n - 1n;

function sum(values: string[]): string {
  const total = values.reduce((n, value) => n + BigInt(value), 0n);
  return String(total > maxU64 ? maxU64 : total);
}

/** A zero limit is unlimited, so a single unlimited member keeps that field unlimited. */
function workBudget(runs: AgentRun[]): AgentRunBudget {
  return Object.fromEntries(budgetKeys.map((key) => {
    const limits = runs.map((r) => r.budget?.[key] ?? "0");
    return [key, limits.includes("0") ? "0" : sum(limits)];
  }));
}

function workCost(runs: AgentRun[]): Cost {
  const total = (key: keyof Cost) => sum(runs.map((r) => r.cost?.[key] ?? "0"));
  return { inputTokens: total("inputTokens"), outputTokens: total("outputTokens"), usdMicros: total("usdMicros") };
}

function fail(code: ConnectCode, reason: string, subcode: string, message: string): never {
  throw new BeeError(code, { reason, subcode, message });
}

const denied = (message: string): never => fail("permission_denied", "PERMISSION_DENIED", "swarm_permission", message);
const invalidState = (message: string): never => fail("failed_precondition", "INVALID_STATE", "swarm_state", message);
const invalid = (message: string): never => fail("invalid_argument", "INVALID_ARGUMENT", "invalid_swarm", message);
const notFound = (what: string): never => fail("not_found", "NOT_FOUND", "swarm_not_found", "No " + what);

function finishAttempt(attempt: SwarmAttempt, time: string, outcome: SwarmAttemptOutcome, reason: string, handBack: SwarmHandBack): void {
  Object.assign(attempt, { finishedAt: time, outcome, reason, handBack });
}

function handBack(done: string[] = [], deviations: string[] = [], concerns: string[] = [], followUps: string[] = []): SwarmHandBack {
  return { done, deviations, concerns, followUps };
}

function repoRef(repo: RepoState): RepoRef {
  return { id: repo.repo.id };
}

function publicPrincipal(principal: Principal): Principal {
  return { ...principal, externalIds: [] };
}

function text(value: string | undefined, label: string, max: number, multiline = false): string {
  if (!value?.trim()) return fail("invalid_argument", "INVALID_ARGUMENT", "missing_field", label + " is required");
  if (Array.from(value).length > max || (!multiline && /\p{Cc}/u.test(value))) {
    return fail("invalid_argument", "INVALID_ARGUMENT", "swarm_text", "Invalid " + label.toLowerCase());
  }
  return value;
}

function taskState(outcome: SwarmAttemptOutcome): SwarmTaskState {
  return outcome === DONE ? TASK_FINISHED : TASK_EMPTY;
}

export class SwarmWorld {
  readonly swarms: Swarm[] = [];
  readonly runs: AgentRun[] = [];
  readonly messages = new Map<string, AgentRunEvent[]>();
  private nextId = 0;

  key(s: FixtureStore, kind: string): string {
    return ulid(s.now(), kind + ":" + ++this.nextId);
  }

  repoRuns(s: FixtureStore, repo: RepoState): AgentRun[] {
    return [
      ...this.runs.filter((r) => r.repo?.id === repo.repo.id),
      ...s.posts.runs.filter((r) => r.repo === repo.name).map((r) => feedAgentRun(s, r)),
    ];
  }

  get(repo: RepoState, id: string | undefined): Swarm {
    return this.swarms.find((row) => row.id === id && row.repo?.id === repo.repo.id) ?? notFound("swarm " + (id ?? ""));
  }

  run(repo: RepoState, id: string | undefined): AgentRun {
    return this.runs.find((row) => row.id === id && row.repo?.id === repo.repo.id) ?? notFound("agent run " + (id ?? ""));
  }

  task(row: Swarm, id: string | undefined): SwarmTask {
    return row.tasks.find((task) => task.id === id) ?? notFound("swarm task " + (id ?? ""));
  }

  emit(s: FixtureStore, repo: RepoState, actor: string, row: Swarm, kind: string, details: Record<string, string> = {}): void {
    row.updatedAt = s.iso(s.now());
    s.emit(repo, kind, actor, {
      subject: { kind: "SUBJECT_KIND_SWARM", repo: repoRef(repo), id: row.id },
      payloadJson: JSON.stringify(details),
    });
  }

  view(s: FixtureStore, repo: RepoState, row: Swarm, actor: string, ctx: Ctx): Swarm {
    const person = s.p(actor).id;
    const lead = isLead(s, row, actor, ctx);
    const finances = lead || s.posts.isAdmin(s, actor, repo);
    const members = row.memberRunIds.map((id) => this.run(repo, id));
    return {
      ...row,
      updatedAt: [row.updatedAt ?? "", ...members.map((run) => runActivity(s, run))].sort().at(-1),
      joins: row.joins.filter((join) => lead || join.requester?.id === person),
      budget: finances ? workBudget(members) : undefined,
      cost: finances ? workCost(members) : undefined,
      changeRequestIds: [...new Set(members.flatMap((r) => this.outcomes(s, repo, r)))].sort(),
    };
  }

  private outcomes(s: FixtureStore, repo: RepoState, run: AgentRun): string[] {
    if (!terminal(run)) return [];
    return (run.changeRequests ?? []).flatMap((number) => {
      const cr = s.crs.find((c) => c.repo === repo.name && String(c.number) === number);
      return cr ? [cr.id] : [];
    });
  }

  list(s: FixtureStore, repo: RepoState, actor: string, ctx: Ctx): RepoWorkItem[] {
    const rows = this.swarms.filter((row) => row.repo?.id === repo.repo.id);
    const attached = new Set(rows.flatMap((row) => row.memberRunIds));
    const items = rows.map((row): RepoWorkItem => {
      const view = this.view(s, repo, row, actor, ctx);
      return { swarm: view, activityAt: view.updatedAt, changeRequestIds: view.changeRequestIds };
    });
    for (const run of this.repoRuns(s, repo)) {
      if (attached.has(run.id ?? "")) continue;
      const person = s.p(actor).id;
      const owns = run.agent?.ownerId === person || run.agent?.id === person || run.startedBy?.id === person;
      const visible = owns || s.posts.isAdmin(s, actor, repo) ? run : { ...run, budget: undefined, cost: undefined };
      items.push({ run: visible, activityAt: runActivity(s, run), changeRequestIds: this.outcomes(s, repo, run) });
    }
    return items.sort((a, b) => {
      const left = a.swarm?.id ?? a.run?.id ?? "";
      const right = b.swarm?.id ?? b.run?.id ?? "";
      return (b.activityAt ?? "").localeCompare(a.activityAt ?? "") || right.localeCompare(left);
    });
  }

  seed(s: FixtureStore): void {
    const repo = s.repo({ fullName: "acme/api" });
    const at = (ago: number) => s.iso(s.ago(ago));
    const id = (ago: number, name: string) => ulid(s.ago(ago), "swarm:" + name);
    const rotate = s.crByKey("rotate");
    const histogram = s.crByKey("histogram");

    const run = (agent: string, branch: string, prompt: string, ago: number, finished?: number, change?: typeof rotate): AgentRun => {
      const principal = s.p(agent);
      const runId = id(ago, agent + ":" + prompt);
      const provenance = change ? s.current(change).head.provenance : undefined;
      const value: AgentRun = {
        id: runId,
        repo: repoRef(repo),
        task: { kind: "AGENT_TASK_KIND_FREEFORM", prompt },
        agent: publicPrincipal(principal),
        startedBy: publicPrincipal(s.p(s.handleOf(principal.ownerId))),
        model: s.agentMeta.get(agent)?.model,
        state: finished === undefined ? "AGENT_RUN_STATE_RUNNING" : "AGENT_RUN_STATE_SUCCEEDED",
        baseRef: branch,
        workRef: change?.ws ?? "ws/" + principal.id + "/" + runId.toLowerCase(),
        budget: { maxUsdMicros: "3000000", maxInputTokens: "500000", maxOutputTokens: "35000", maxDurationSeconds: "3600", maxChangesets: "10", maxLanded: "2" },
        cost: provenance?.cost ?? zeroCost,
        changeRequests: change ? [String(change.number)] : [],
        session: provenance?.session ?? "run-" + runId.toLowerCase(),
        createdAt: at(ago),
        startedAt: at(ago),
        finishedAt: finished === undefined ? undefined : at(finished),
      };
      this.runs.push(value);
      return value;
    };

    const attempt = (worker: AgentRun, ago: number, outcome: SwarmAttemptOutcome, reason: string, report?: SwarmHandBack, finished = ago - 2): SwarmAttempt => ({
      runId: worker.id!,
      agent: worker.agent as Principal,
      startedAt: at(ago),
      finishedAt: outcome === IN_PROGRESS ? undefined : at(finished),
      outcome,
      reason,
      handBack: report,
    });
    const task = (title: string, paths: string[], state: SwarmTaskState, attempts: SwarmAttempt[] = []): SwarmTask => ({
      id: id(100, title),
      title,
      paths: paths.map(pathBytes),
      state,
      runId: state === TASK_WORKING ? (attempts.at(-1)?.runId ?? "") : "",
      attempts,
    });

    const lead = run("triage", "main", "Coordinate session rotation", 3_040, undefined, rotate);
    const docs = run("docs", "main", "Check the session guide for old role claims", 170, 166);
    const reviewer = run("reviewer", "main", "Review rotation regressions", 110);
    this.swarms.push({
      id: id(3_030, "session-rotation"),
      repo: repoRef(repo),
      name: "Session rotation",
      goal: "When we demote an admin, their old session must lose admin access immediately. Keep other sessions signed in.",
      issue: "12",
      branch: "main",
      leadRunId: lead.id!,
      lead: lead.agent as Principal,
      owner: publicPrincipal(s.p("mara")),
      state: WORKING,
      status: "The rotation fix is in review. Next, add an audit entry for role changes.",
      memberRunIds: [lead.id!, docs.id!, reviewer.id!],
      tasks: [
        task("Map the role downgrade path", ["src/auth/tokens.ts"], TASK_FINISHED, [
          attempt(lead, 3_020, DONE, "Found where the role service changes a session.", handBack(["Traced the call from the role service to session rotation."], [], [], ["Cover the downgraded session in the regression test."])),
        ]),
        task("Review the rotation regression tests", ["tests/auth.test.ts"], TASK_WORKING, [
          attempt(reviewer, 100, IN_PROGRESS, ""),
        ]),
        task("Check the API guide for stale claims", ["docs/api.md"], TASK_EMPTY, [
          attempt(docs, 169, EMPTY, "The API guide has no statement about retaining role claims.", handBack([], [], [], ["Recheck the guide when session rotation has shipped."])),
        ]),
        task("Check the protected operations config", ["ops/secrets.enc.yaml"], TASK_EMPTY, [
          attempt(lead, 100, REFUSED, "Repository rules deny access to the encrypted operations file.", handBack([], ["Left the protected file untouched."], ["An operator can verify the config in their environment."])),
        ]),
        task("Record session rotation in the audit log", ["src/auth/session.ts"], TASK_OPEN),
        task("Verify account switching", ["src/server.ts"], TASK_OPEN),
      ],
      joins: [
        { id: id(30, "billing-join"), requester: publicPrincipal(s.p("dev")), agent: publicPrincipal(s.p("billing")), budget: { maxUsdMicros: "1000000" }, state: PENDING, runId: "", reason: "", requestedAt: at(30) },
      ],
      changeRequestIds: [],
      createdAt: at(3_030),
      updatedAt: at(30),
    });

    const watch = run("watcher", "main", "Find the source of the health route latency", 1_700, 1_430);
    const route = run("triage", "main", "Add a health route latency histogram", 1_620, 1_430, histogram);
    const support = run("billing", "main", "Prepare a redacted incident summary from the health route access logs.", 1_410, 1_400);
    const concern = run("docs", "main", "Check which health route diagnostics are available to a signed-in customer.", 1_408, 1_402);
    for (const interrupted of [support, concern]) {
      interrupted.startedBy = publicPrincipal(s.p("mara"));
      interrupted.state = CANCELLED;
      interrupted.failureReason = "CANCELLED";
    }
    this.swarms.push({
      id: id(1_690, "health-route"),
      repo: repoRef(repo),
      name: "Health route latency",
      goal: "Tell slow health route requests apart from a slow host so we can diagnose the p99 spike.",
      issue: "18",
      branch: "main",
      leadRunId: watch.id!,
      lead: watch.agent as Principal,
      owner: publicPrincipal(s.p("mara")),
      state: FINISHED,
      status: "The latency histogram landed. Mara closed the swarm after the safety pause. The incident summary stayed private.",
      memberRunIds: [watch.id!, route.id!, support.id!, concern.id!],
      tasks: [
        task("Compare telemetry around the dependency bump", ["src/metrics.ts"], TASK_FINISHED, [
          attempt(watch, 1_680, DONE, "Isolated time spent in the route.", handBack(["Compared route p99 before and after the dependency bump."])),
        ]),
        task(histogram.title, ["src/routes/health.ts"], TASK_FINISHED, [
          attempt(route, 1_605, DONE, "Added the route latency histogram.", handBack(["Opened the histogram pull request and saw it land."], [], [], ["Watch the new metric through the next release."]), 1_430),
        ]),
        task("Prepare a redacted incident summary", [], TASK_EMPTY, [
          attempt(support, 1_409, EMPTY, "A safety tripwire interrupted the work.", undefined, 1_400),
        ]),
        task("Check customer access to health diagnostics", [], TASK_EMPTY, [
          attempt(concern, 1_407, EMPTY, "The agent asked for a redacted session.", undefined, 1_402),
        ]),
      ],
      joins: [],
      changeRequestIds: [histogram.id],
      createdAt: at(1_690),
      updatedAt: at(1_390),
    });
  }
}

function scope(s: FixtureStore, ref: SwarmRefJson | undefined, ctx: Ctx, write = false) {
  const actor = s.caller(ctx.token);
  const repo = s.repo(ref?.repo);
  if (write && repo.archived) fail("failed_precondition", "ARCHIVED", "swarm_archived", "This repository is archived");
  return { actor, repo, row: s.swarms.get(repo, ref?.id) };
}

function isLead(s: FixtureStore, row: Swarm, actor: string, ctx: Ctx): boolean {
  const repo = s.repo(row.repo);
  return row.owner?.id === s.p(actor).id || inRun(s, ctx, actor, s.swarms.run(repo, row.leadRunId));
}

function needLead(s: FixtureStore, row: Swarm, actor: string, ctx: Ctx): void {
  if (!isLead(s, row, actor, ctx)) denied("Only the swarm lead or the person who answers for it can do this");
}

function tokenScope(s: FixtureStore, ctx: Ctx) {
  const tokenId = ctx.token && s.tokenSecrets.get(ctx.token);
  return s.tokens.find((t) => t.tokenId === tokenId)?.scope;
}

function inRun(s: FixtureStore, ctx: Ctx, actor: string, run: AgentRun): boolean {
  return !!run.session && tokenScope(s, ctx)?.session === run.session && run.agent?.id === s.p(actor).id;
}

function active(row: Swarm): void {
  if (row.state === STOPPED || row.state === FINISHED) invalidState("This swarm has ended");
}

function working(row: Swarm): void {
  if (row.state !== WORKING) invalidState("The swarm must be working");
}

function owner(s: FixtureStore, row: Swarm, repo: RepoState, actor: string): boolean {
  return s.kindOf(actor) === "human" && (row.owner?.id === s.p(actor).id || s.posts.isAdmin(s, actor, repo));
}

function pathsOf(paths: string[]): string[] {
  if (paths.length > 128) invalid("At most 128 paths may be claimed");
  const claims = [...new Set(paths)];
  const bytes = (path: string) => Buffer.from(base64ToBytes(path));
  for (const claim of claims) {
    const raw = bytes(claim);
    if (raw.length > 1024 || raw.includes(0) || raw.toString("latin1").split("/").some((p) => p === "" || p === "." || p === "..")) {
      invalid("Task paths must be relative repository paths");
    }
  }
  return claims.sort((a, b) => bytes(a).compare(bytes(b)));
}

function overlaps(a: string, b: string): boolean {
  const prefix = (short: Uint8Array, long: Uint8Array) =>
    (long.length === short.length || long[short.length] === 0x2f) && short.every((byte, index) => byte === long[index]);
  const left = base64ToBytes(a);
  const right = base64ToBytes(b);
  return prefix(left, right) || prefix(right, left);
}

const transitions: Partial<Record<SwarmState, readonly SwarmState[]>> = {
  [PLANNING]: [WORKING, STOPPED, FINISHED],
  [WORKING]: [PAUSED, STOPPED, FINISHED],
  [PAUSED]: [WORKING, STOPPED, FINISHED],
};

export const swarmService = defineService(SwarmService, {
  createSwarm: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    if (repo.archived) fail("failed_precondition", "ARCHIVED", "swarm_archived", "This repository is archived");
    const run = s.swarms.run(repo, req.leadRunId);
    const lead = run.agent as Principal;
    const person = s.p(s.handleOf(lead.ownerId));
    if (actor !== s.handleOf(person.id) || s.kindOf(actor) !== "human" || !s.posts.canWrite(s, actor, repo)) denied("Only the lead's person can create a swarm in this repository");
    if (terminal(run) || s.swarms.swarms.some((w) => w.memberRunIds.includes(run.id!))) invalidState("The lead must be a live run available to join a swarm");
    const branch = text(req.branch, "Branch", 1024);
    if (!repo.refs.has(branch)) notFound("branch " + branch);
    if (run.baseRef !== branch) invalid("The lead run must use the swarm branch");
    if (req.issue && !s.issues.some((i) => i.repoId === repo.repo.id && String(i.number) === req.issue)) notFound("issue " + req.issue);
    const row: Swarm = {
      id: s.swarms.key(s, "swarm"),
      repo: repoRef(repo),
      name: text(req.name, "Name", 200),
      goal: text(req.goal, "Goal", 16_384, true),
      issue: req.issue ?? "",
      branch,
      leadRunId: run.id!,
      lead: publicPrincipal(lead),
      owner: publicPrincipal(person),
      state: PLANNING,
      status: "",
      memberRunIds: [run.id!],
      tasks: [],
      joins: [],
      changeRequestIds: [],
      createdAt: s.iso(s.now()),
    };
    s.swarms.swarms.push(row);
    s.swarms.emit(s, repo, actor, row, "swarm.created");
    return s.swarms.view(s, repo, row, actor, ctx);
  },
  getSwarm: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req, ctx);
    return s.swarms.view(s, repo, row, actor, ctx);
  },
  listRepoWork: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const cursor = req.page?.pageToken ?? "";
    const [afterTime, afterId] = cursor.split("/");
    if (cursor && (!/^\d+$/.test(afterTime ?? "") || !afterId || cursor.split("/").length !== 2)) invalid("Invalid repo work page token");
    const key = (item: RepoWorkItem) => ({ time: Date.parse(item.activityAt ?? ""), id: item.swarm?.id ?? item.run?.id ?? "" });
    const items = s.swarms.list(s, repo, actor, ctx).filter((item) => {
      if (!cursor) return true;
      const { time, id } = key(item);
      return time < Number(afterTime) || (time === Number(afterTime) && id < afterId!);
    });
    const limit = Math.max(1, Math.min(500, req.page?.pageSize || 50));
    const result = items.slice(0, limit);
    const last = result.at(-1);
    const next = last && items.length > limit ? key(last) : undefined;
    return { items: result, page: { nextPageToken: next ? next.time + "/" + next.id : "" } };
  },
  setSwarmState: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    const next = req.state ?? "SWARM_STATE_UNSPECIFIED";
    if (!owner(s, row, repo, actor)) denied("Only the person who answers for this swarm or a repository admin can change its state");
    if (row.state === next) return s.swarms.view(s, repo, row, actor, ctx);
    if (!transitions[row.state]?.includes(next)) invalidState("The swarm cannot move to that state");
    if (next === FINISHED && (row.tasks.some((task) => task.state === TASK_OPEN || task.state === TASK_WORKING) || row.memberRunIds.some((id) => !terminal(s.swarms.run(repo, id))))) {
      invalidState("Finish the tasks and member runs before finishing the swarm");
    }
    if (next === STOPPED) {
      const time = s.iso(s.now());
      for (const id of row.memberRunIds) {
        const run = s.swarms.run(repo, id);
        if (terminal(run)) continue;
        run.state = CANCELLED;
        run.finishedAt = time;
        run.failureReason = "CANCELLED";
        s.emit(repo, "agent_run.completed", actor, { subject: { kind: "SUBJECT_KIND_AGENT_RUN", repo: repoRef(repo), id } });
      }
      for (const task of row.tasks) {
        const attempt = task.attempts.at(-1);
        if (task.state !== TASK_WORKING || !attempt) continue;
        finishAttempt(attempt, time, EMPTY, "Swarm stopped", handBack());
        task.state = TASK_EMPTY;
        task.runId = "";
        s.swarms.emit(s, repo, actor, row, "swarm.task_finished", { task_id: task.id, run_id: attempt.runId, outcome: EMPTY });
      }
    }
    if (next === STOPPED || next === FINISHED) {
      const time = s.iso(s.now());
      for (const join of row.joins) {
        if (join.state !== PENDING) continue;
        join.state = "SWARM_JOIN_STATE_DECLINED";
        join.reason = "Swarm ended";
        join.decidedAt = time;
        s.swarms.emit(s, repo, actor, row, "swarm.join_decided", { join_id: join.id, state: join.state });
      }
    }
    row.state = next;
    s.swarms.emit(s, repo, actor, row, "swarm.state_changed", { state: next });
    return s.swarms.view(s, repo, row, actor, ctx);
  },
  setSwarmStatus: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    if (!inRun(s, ctx, actor, s.swarms.run(repo, row.leadRunId))) denied("Only the swarm lead run can update status");
    active(row);
    row.status = text(req.status, "Status", 500);
    s.swarms.emit(s, repo, actor, row, "swarm.status_changed");
    return s.swarms.view(s, repo, row, actor, ctx);
  },
  addSwarmTask: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    needLead(s, row, actor, ctx);
    active(row);
    if (row.tasks.length >= 200) fail("resource_exhausted", "TOO_LARGE", "swarm_tasks", "At most 200 tasks may be added");
    const task: SwarmTask = {
      id: s.swarms.key(s, "task"),
      title: text(req.title, "Task title", 240),
      paths: pathsOf(req.paths ?? []),
      state: TASK_OPEN,
      runId: "",
      attempts: [],
    };
    row.tasks.push(task);
    s.swarms.emit(s, repo, actor, row, "swarm.task_added", { task_id: task.id });
    return task;
  },
  startSwarmTask: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    working(row);
    const task = s.swarms.task(row, req.taskId);
    const run = s.swarms.run(repo, req.runId);
    if (!isLead(s, row, actor, ctx) && !inRun(s, ctx, actor, run)) denied("Only the swarm lead or the worker run can start the task");
    if (!row.memberRunIds.includes(run.id!) || terminal(run)) invalidState("The worker must be a live member of this swarm");
    if (task.state !== TASK_OPEN && task.state !== TASK_EMPTY) invalidState("Only open or empty tasks can be started");
    const locked = row.tasks.some((other) => other.state === TASK_WORKING && (
      other.runId === run.id || other.paths.some((a) => task.paths.some((b) => overlaps(a, b)))
    ));
    if (task.attempts.length >= 50) fail("resource_exhausted", "TOO_LARGE", "swarm_attempts", "At most 50 attempts may be recorded");
    if (locked) fail("failed_precondition", "INVALID_STATE", "swarm_task_claim", "The worker or task paths are already in use");
    const time = s.iso(s.now());
    const attempt: SwarmAttempt = { runId: run.id!, agent: run.agent as Principal, startedAt: time, outcome: IN_PROGRESS, reason: "" };
    task.state = TASK_WORKING;
    task.runId = run.id!;
    task.attempts.push(attempt);
    s.swarms.emit(s, repo, actor, row, "swarm.task_started", { task_id: task.id, run_id: run.id! });
    return task;
  },
  finishSwarmTask: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    active(row);
    const task = s.swarms.task(row, req.taskId);
    const run = s.swarms.run(repo, req.runId);
    if (!isLead(s, row, actor, ctx) && !inRun(s, ctx, actor, run)) denied("Only the assigned worker or the swarm lead can return the task");
    const attempt = task.attempts.at(-1);
    if (!req.attemptNumber) return fail("invalid_argument", "INVALID_ARGUMENT", "missing_field", "An attempt number is required");
    if (req.attemptNumber !== task.attempts.length) return fail("aborted", "VERSION_MISMATCH", "swarm_attempt_changed", "This is not the current task attempt");
    if (task.state !== TASK_WORKING || task.runId !== run.id || !attempt) return invalidState("This run is not working on the task");
    if (req.outcome !== DONE && req.outcome !== EMPTY && req.outcome !== REFUSED) return invalid("Choose how the task ended");
    if (!req.handBack) return invalid("A hand-back is required");
    const reason = text(req.reason, "Reason", 500);
    const report = handBack(req.handBack.done, req.handBack.deviations, req.handBack.concerns, req.handBack.followUps);
    for (const entries of Object.values(report)) {
      if (entries.length > 32) fail("resource_exhausted", "TOO_LARGE", "swarm_hand_back", "At most 32 entries may be recorded in each hand-back list");
      for (const entry of entries) text(entry, "Hand-back entry", 1000);
    }
    finishAttempt(attempt, s.iso(s.now()), req.outcome, reason, report);
    task.state = taskState(req.outcome);
    task.runId = "";
    s.swarms.emit(s, repo, actor, row, "swarm.task_finished", { task_id: task.id, run_id: attempt.runId, outcome: req.outcome });
    return task;
  },
  sendSwarmMessage: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    if (s.kindOf(actor) !== "human" || !s.posts.canWrite(s, actor, repo)) denied("Only a person with write access can send a message to the swarm lead");
    active(row);
    const run = s.swarms.run(repo, row.leadRunId);
    if (terminal(run)) invalidState("The lead run has finished");
    const body = text(req.body, "Message", 65_536, true);
    const time = s.iso(s.now());
    const seq = String(BigInt(run.lastEventSeq ?? "0") + 1n);
    const event: AgentRunEvent = {
      runId: row.leadRunId,
      seq,
      kind: "AGENT_RUN_EVENT_KIND_STEERING_MESSAGE",
      time,
      text: body,
      author: publicPrincipal(s.p(actor)),
    };
    run.lastEventSeq = seq;
    const messages = s.swarms.messages.get(row.leadRunId) ?? [];
    messages.push(event);
    s.swarms.messages.set(row.leadRunId, messages);
    row.updatedAt = time;
    s.emit(repo, "agent_run.message", actor, { subject: { kind: "SUBJECT_KIND_AGENT_RUN", repo: repoRef(repo), id: row.leadRunId } });
    return event;
  },
  requestSwarmJoin: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    active(row);
    const requester = s.p(actor);
    const agent = s.principals.get(s.handleOf(req.agentPrincipalId ?? ""));
    if (!agent || agent.kind !== "PRINCIPAL_KIND_AGENT") return notFound("agent " + (req.agentPrincipalId ?? ""));
    if (requester.id !== agent.ownerId && requester.id !== agent.id) denied("Ask with an agent you own");
    if (!req.budget || BigInt(req.budget.maxUsdMicros ?? 0) === 0n) {
      return fail("invalid_argument", "INVALID_ARGUMENT", "swarm_join_budget", "A positive spending limit is required");
    }
    const token = tokenScope(s, ctx);
    if (token && token.actions.length && !token.actions.includes("swarm_join")) denied("The token does not allow swarm join requests");
    if (row.joins.length >= 200) fail("resource_exhausted", "TOO_LARGE", "swarm_joins", "At most 200 join requests may be recorded");
    if (row.joins.some((j) => (j.state === PENDING || j.state === "SWARM_JOIN_STATE_ACCEPTED") && j.agent?.id === agent.id)) {
      return fail("already_exists", "ALREADY_EXISTS", "swarm_join_duplicate", "This agent already has a pending or accepted join request");
    }
    const join: SwarmJoin = {
      id: s.swarms.key(s, "join"),
      requester: publicPrincipal(requester),
      agent: publicPrincipal(agent),
      budget: req.budget,
      state: PENDING,
      runId: "",
      reason: "",
      requestedAt: s.iso(s.now()),
    };
    row.joins.push(join);
    s.swarms.emit(s, repo, actor, row, "swarm.join_requested", { join_id: join.id });
    return join;
  },
  decideSwarmJoin: (s, req, ctx) => {
    const { actor, repo, row } = scope(s, req.swarm, ctx, true);
    needLead(s, row, actor, ctx);
    active(row);
    const join = row.joins.find((j) => j.id === req.joinId) ?? notFound("join request");
    if (join.state !== PENDING) invalidState("This request has already been decided");
    const time = s.iso(s.now());
    const agent = join.agent as Principal;
    const reason = req.reason ? text(req.reason, "Reason", 500) : "";
    const worker = s.handleOf(agent.id);
    const requester = join.requester as Principal;
    const primary = s.handleOf(agent.ownerId);
    const rules = s.rules(repo);
    const participants = [actor, s.handleOf(requester.id), primary, worker];
    const admittedByRules = (handle: string) => {
      const role = s.posts.isAdmin(s, handle, repo) ? "admin" : s.inOrg(handle, repo.owner) ? "write" : "read";
      return mayJoinSwarm(rules, [handle, s.p(handle).id, "@everyone", "@" + s.kindOf(handle) + "s", "@repo-role/" + role]);
    };
    if (!req.accept) {
      join.state = "SWARM_JOIN_STATE_DECLINED";
      join.reason = reason;
    } else if (!s.inOrg(worker, repo.owner) || !s.agentMeta.has(worker)) {
      return notFound("enabled agent in this organization");
    } else if (requester.id !== agent.ownerId && requester.ownerId !== agent.ownerId) {
      return denied("The agent's current owner must authorize this run");
    } else if (!participants.every(admittedByRules)) {
      join.state = "SWARM_JOIN_STATE_REFUSED";
      join.reason = "Repository rules do not grant swarm admission to every participant.";
    } else {
      const runId = s.swarms.key(s, "run");
      s.swarms.runs.push({
        id: runId,
        repo: repoRef(repo),
        agent,
        startedBy: publicPrincipal(s.p(primary)),
        task: { kind: "AGENT_TASK_KIND_FREEFORM", prompt: row.goal },
        baseRef: row.branch,
        workRef: "ws/" + agent.id + "/" + runId.toLowerCase(),
        state: "AGENT_RUN_STATE_QUEUED",
        session: "run-" + runId.toLowerCase(),
        budget: join.budget,
        cost: zeroCost,
        createdAt: time,
      });
      join.state = "SWARM_JOIN_STATE_ACCEPTED";
      join.reason = reason;
      join.runId = runId;
      row.memberRunIds.push(runId);
    }
    join.decidedAt = time;
    s.swarms.emit(s, repo, actor, row, "swarm.join_decided", { join_id: join.id, state: join.state });
    return join;
  },
});
