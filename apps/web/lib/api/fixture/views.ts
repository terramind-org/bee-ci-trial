/**
 * bee.v1 handlers for the areas the UI composes: agents, workflows, issues, projects,
 * releases, security, policy and audit.
 */
import { AgentService } from "../../gen/bee/v1/agents_pb";
import { AuditService } from "../../gen/bee/v1/audit_pb";
import type { RepoRefJson } from "../../gen/bee/v1/common_pb";
import { DeploymentService } from "../../gen/bee/v1/deployments_pb";
import { IssueService } from "../../gen/bee/v1/issues_pb";
import { PolicyService } from "../../gen/bee/v1/policy_pb";
import { ProjectService, type ProjectItemJson } from "../../gen/bee/v1/projects_pb";
import { ReleaseService } from "../../gen/bee/v1/releases_pb";
import { SecurityService } from "../../gen/bee/v1/security_pb";
import { WorkflowService, type WorkflowJson } from "../../gen/bee/v1/workflows_pb";
import { bytesToBase64, pathBytes } from "../ids";
import { runStatus } from "../ops/work";
import { BeeError } from "../transport";
import type * as T from "../types";
import { alertNumber, auditEntry, codeAlert, dependencyAlert, issueJson, logBytes, releaseJson, secretAlert, secretParts, workflowJob, workflowRun } from "./convert";
import { describeRules, normalizedPolicy, parsePolicy, POLICY_PATH } from "./policy";
import { defineService, type Ctx, type ServiceDef } from "./router";
import { sessionsFor } from "./briefs";
import { agentBudgetImpl } from "./agent-budgets";
import { agentRunImpl } from "./agent-runs";
import { feedTriggerImpl } from "./posts";
import { ulid } from "./seed";
import { instructionImpl } from "./settings";
import type { FixtureStore, IssueRow } from "./store";
import { invalid, page, waitLoop } from "./util";

const WEEK = 7 * 86_400_000;

// ---------------------------------------------------------------- agents

function agentHandles(s: FixtureStore): string[] {
  return [...s.principals.entries()].filter(([, p]) => p.kind === "PRINCIPAL_KIND_AGENT").map(([h]) => h);
}

/** The agent's commits, newest first. */
function commitsBy(s: FixtureStore, h: string) {
  return [...s.commits.values()].filter((c) => c.author === h).sort((a, b) => b.time - a.time);
}

const agents = defineService(AgentService, {
  ...instructionImpl,
  ...feedTriggerImpl,
  ...agentBudgetImpl,
  ...agentRunImpl,
  getSessionsForChange: (s, req) => sessionsFor(s, req as Parameters<typeof sessionsFor>[1]),
  // The fixture always reports a 7-day window ending at its own clock.
  listAgentPrincipals: (s, req) => {
    const org = s.orgRecord(req.org || s.org.name).org.name;
    const weekAgo = s.now() - WEEK;
    const only = new Set(req.principalIds ?? []);
    return {
      agents: agentHandles(s)
        .filter((h) => s.inOrg(h, org))
        .filter((h) => !only.size || only.has(s.p(h).id))
        .map((h) => {
        const pr = s.p(h);
        const mine = commitsBy(s, h);
        const recent = mine.filter((c) => c.time > weekAgo);
        const sum = (f: (c: (typeof mine)[number]) => string | undefined) => String(recent.reduce((n, c) => n + Number(f(c) ?? 0), 0));
        const leases = s.leases.filter((l) => l.holder?.id === pr.id).length;
        const last = mine[0]?.time ?? 0;
        const landed = [...s.repos.values()].flatMap((r) => r.main).filter((c) => c.author === h && c.time > weekAgo).length;
        const latest = mine.find((c) => c.provenance);
        return {
          principal: pr,
          lastModel: s.agentMeta.get(h)?.model ?? latest?.provenance?.model ?? "",
          lastSession: latest?.provenance?.session ?? "",
          openChangeRequests: String(s.crs.filter((c) => c.author === h && c.state === "CHANGE_STATE_OPEN").length),
          purpose: s.agentMeta.get(h)?.purpose ?? "",
          stats: {
            changesets: String(recent.length),
            workItems: String(recent.length),
            cost: { inputTokens: sum((c) => c.provenance?.cost?.inputTokens), outputTokens: sum((c) => c.provenance?.cost?.outputTokens), usdMicros: sum((c) => c.provenance?.cost?.usdMicros) },
            outcomes: {
              changeRequestsOpened: String(s.crs.filter((c) => c.author === h && c.createdAt > weekAgo).length),
              changeRequestsLanded: String(landed),
            },
            activeLeases: String(leases),
          },
          lastActiveAt: last ? s.iso(Math.max(last, leases ? s.now() - 4 * 60_000 : 0)) : undefined,
        };
      }),
      page: { nextPageToken: "" },
    };
  },
  listAgentWorkItems: (s, req) => {
    const h = s.handleOf(req.principalId ?? "");
    const items = commitsBy(s, h)
      .filter((c) => c.provenance)
      .map((c) => {
        const cr = s.crs.find((x) => x.revisions.some((r) => r.head === c));
        const repo = s.repos.get(c.repo);
        return {
          subject: { kind: "SUBJECT_KIND_CHANGESET" as const, repo: { fullName: repo?.repo.fullName ?? "" }, id: c.idText },
          title: c.message,
          state: !cr && (repo?.main.includes(c) ?? false) ? "landed" : "open",
          cost: c.provenance?.cost,
          session: c.provenance?.session ?? "",
          createdAt: s.iso(c.time),
        };
      });
    const pg = page(items, req.page);
    return { items: pg.items, page: { nextPageToken: pg.next } };
  },
});

// ---------------------------------------------------------------- workflows

function runOf(s: FixtureStore, id: string | undefined): T.Run {
  const run = s.runs.find((r) => r.id === id);
  if (!run) throw new BeeError("not_found", { reason: "RUN_NOT_FOUND", message: "no run " + id });
  return run;
}

// The fixture repository declares one workflow, in its checked-in demo tree.
const CI_PATH = ".github/workflows/ci.yml";

function fixtureWorkflow(s: FixtureStore, ref: RepoRefJson | undefined): WorkflowJson | undefined {
  const repo = s.repo(ref);
  const tip = repo.refs.get(repo.repo.defaultRef)?.target;
  if (!tip?.files.has(CI_PATH)) return undefined;
  return {
    id: "wf_ci",
    repo: { id: repo.repo.id },
    name: "ci",
    path: CI_PATH,
    state: s.disabledCi.has(repo.repo.id) ? "WORKFLOW_STATE_DISABLED_MANUALLY" : "WORKFLOW_STATE_ACTIVE",
  };
}

function namedWorkflow(s: FixtureStore, repo: RepoRefJson | undefined, name: string | undefined): WorkflowJson {
  const workflow = fixtureWorkflow(s, repo);
  if (!workflow || ![workflow.id, workflow.path, "ci.yml"].includes(name)) {
    throw new BeeError("not_found", { reason: "NOT_FOUND", message: "Workflow not found." });
  }
  return workflow;
}

const workflows = defineService(WorkflowService, {
  listWorkflows: (s, req) => {
    const workflow = fixtureWorkflow(s, req.repo);
    const pg = page(workflow ? [workflow] : [], req.page);
    return { workflows: pg.items, page: { nextPageToken: pg.next } };
  },
  enableWorkflow: (s, req) => {
    const workflow = namedWorkflow(s, req.repo, req.workflow);
    s.disabledCi.delete(s.repo(req.repo).repo.id);
    return { ...workflow, state: "WORKFLOW_STATE_ACTIVE" };
  },
  disableWorkflow: (s, req) => {
    const workflow = namedWorkflow(s, req.repo, req.workflow);
    s.disabledCi.add(s.repo(req.repo).repo.id);
    return { ...workflow, state: "WORKFLOW_STATE_DISABLED_MANUALLY" };
  },
  getWorkflowBadge: (s, req) => {
    const workflow = namedWorkflow(s, req.repo, req.workflow);
    const repo = s.repo(req.repo);
    const last = s.runs.filter((r) => r.repoId === repo.repo.id && r.ref === (req.branch || repo.repo.defaultRef) && r.finishedAt)
      .sort((a, b) => Date.parse(b.finishedAt!) - Date.parse(a.finishedAt!))[0];
    const message = last?.status === "passed" ? "passing" : last?.status === "failed" ? "failing" : last?.status === "cancelled" ? "cancelled" : "no status";
    return {
      label: workflow.name,
      message,
      svg: '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="20"><rect width="120" height="20" fill="#333"/><text x="8" y="14" fill="white" font-size="12">ci: ' + message + '</text></svg>',
    };
  },
  cancelWorkflowRun: (s, req, ctx) => {
    const run = runOf(s, req.runId);
    if (run.repoId !== s.repo(req.repo).repo.id) throw new BeeError("not_found", { reason: "RUN_NOT_FOUND", message: "Run not found." });
    s.cancelRun(run, s.caller(ctx.token));
    return {};
  },
  listWorkflowRuns: (s, req) => {
    s.refreshLive();
    const repo = s.repo(req.repo);
    const want = req.status && req.status !== "WORKFLOW_STATUS_UNSPECIFIED" ? runStatus(req.status, req.conclusion) : null;
    const runs = s.runs
      .filter((r) => r.repoId === repo.repo.id)
      .filter((r) => !want || r.status === want)
      .filter((r) => !req.changeRequestNumber || req.changeRequestNumber === "0" || String(s.crs.find((c) => c.id === r.changeRequestId)?.number) === req.changeRequestNumber)
      .sort((a, b) => Date.parse(b.startedAt ?? "") - Date.parse(a.startedAt ?? ""));
    const pg = page(runs, req.page);
    return { runs: pg.items.map((r) => ({ ...workflowRun(s, r), jobs: req.includeJobs ? r.jobs.map((j) => workflowJob(s, r, j)) : [] })), page: { nextPageToken: pg.next }, totalCount: String(runs.length) };
  },
  getWorkflowRun: (s, req) => {
    s.refreshLive();
    return workflowRun(s, runOf(s, req.runId));
  },
  listWorkflowJobs: (s, req) => {
    s.refreshLive();
    const run = runOf(s, req.runId);
    return { jobs: run.jobs.map((j) => workflowJob(s, run, j)), page: { nextPageToken: "" } };
  },
  rerunWorkflowRun: (s, req, ctx) => workflowRun(s, s.rerun(runOf(s, req.runId), false, s.caller(ctx.token))),
  rerunFailedWorkflowJobs: (s, req, ctx) => workflowRun(s, s.rerun(runOf(s, req.runId), true, s.caller(ctx.token))),
  tailWorkflowJobLogs: (s, req, ctx: Ctx) => ({
    async *[Symbol.asyncIterator]() {
      const run = s.runs.find((r) => r.jobs.some((j) => j.id === req.jobId));
      if (!run) throw new BeeError("not_found", { reason: "RUN_NOT_FOUND", message: "no job " + req.jobId });
      let sent = Number(req.offset ?? 0);
      const flush = function* () {
        const bytes = logBytes(s.jobLines(run, req.jobId ?? ""));
        if (bytes.length > sent) {
          yield { offset: String(sent), data: bytesToBase64(bytes.subarray(sent)), stepNumber: 0, eof: false };
          sent = bytes.length;
        }
      };
      yield* flush();
      if (!req.follow) return;
      for await (const _ of waitLoop(ctx.signal, Math.min(250, s.logMs))) {
        void _;
        s.refreshLive();
        yield* flush();
        const job = run.jobs.find((j) => j.id === req.jobId);
        if (!job || job.status !== "running") {
          yield* flush();
          return;
        }
      }
    },
  }),
});

// A deployment of the first main commit in the fixture API repository.
const deployments = defineService(DeploymentService, {
  listDeployments: (s, req) => {
    const repo = s.repo(req.repo);
    const commit = repo.name === "api" ? repo.main[0] : undefined;
    const entries = commit ? [{
      id: ulid(commit.time, "deployment:api:staging"),
      environment: "staging",
      ref: "main",
      changeset: commit.id,
      state: "DEPLOYMENT_STATE_SUCCESS" as const,
    }] : [];
    const pg = page(entries, req.page);
    return { deployments: pg.items, page: { nextPageToken: pg.next } };
  },
});

// ---------------------------------------------------------------- issues

function issueOf(s: FixtureStore, ref: { id?: string; fullName?: string } | undefined, number: string | undefined): IssueRow {
  const repo = s.repo(ref);
  const i = s.issues.find((x) => x.repoId === repo.repo.id && String(x.number) === number);
  if (!i) throw new BeeError("not_found", { reason: "ISSUE_NOT_FOUND", message: "no issue #" + number + " in " + repo.repo.fullName });
  return i;
}

const issues = defineService(IssueService, {
  createIssue: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const title = (req.title ?? "").trim();
    if (!title) throw invalid("an issue needs a title");
    let inspiredBy: string | undefined;
    if (req.inspiredByPostId) {
      const source = s.posts.byId(req.inspiredByPostId);
      if (!source || source.state !== "POST_STATE_PUBLISHED" || !s.posts.visible(s, source, actor)) throw invalid("the post is not published or you cannot read it", "post_not_readable");
      inspiredBy = source.id;
    }
    const now = s.now();
    const number = Math.max(0, ...s.issues.filter((x) => x.repoId === repo.repo.id).map((x) => x.number)) + 1;
    const assignees = (req.assignees ?? []).map((id) => {
      const a = s.principals.get(s.handleOf(id));
      if (!a) throw new BeeError("not_found", { reason: "PRINCIPAL_NOT_FOUND", message: "no principal " + id });
      return a;
    });
    const issue: IssueRow = {
      id: ulid(now, "issue:new:" + repo.name + ":" + number),
      repoId: repo.repo.id,
      number,
      title,
      body: req.body ?? "",
      state: "open",
      type: (req.issueType ?? "task").toLowerCase(),
      labels: req.labels ?? [],
      milestone: "",
      assignees,
      author: s.p(actor),
      parentNumber: 0,
      subIssues: [],
      linkedChanges: [],
      comments: [],
      createdAt: s.iso(now),
      inspiredByPostId: inspiredBy,
    };
    s.issues.push(issue);
    s.emit(repo, "issue.opened", actor, { note: { title: "#" + number + " " + title, target: "issue:" + number } });
    if (inspiredBy) {
      s.posts.adoptions.push({ post: inspiredBy, repo: repo.name, kind: "issue", ref: String(number), title, by: actor, at: now });
      s.posts.adopted(s, inspiredBy, actor);
    }
    return issueJson(s, issue, actor);
  },
  listIssues: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    const state = req.allStates || !req.state || req.state === "ISSUE_STATE_UNSPECIFIED" ? "" : req.state === "ISSUE_STATE_CLOSED" ? "closed" : "open";
    const labels = req.labels ?? [];
    const list = s.issues
      .filter((i) => i.repoId === repo.repo.id)
      .filter((i) => !state || i.state === state)
      .filter((i) => labels.every((l) => i.labels.includes(l)))
      .filter((i) => !req.assignee || i.assignees.some((a) => a.id === req.assignee))
      .sort((a, b) => b.number - a.number);
    const pg = page(list, req.page, 100);
    const viewer = s.viewer(ctx.token);
    return { issues: pg.items.map((i) => issueJson(s, i, viewer)), totalCount: String(list.length), page: { nextPageToken: pg.next } };
  },
  getIssueByNumber: (s, req, ctx) => issueJson(s, issueOf(s, req.repo, req.number), s.viewer(ctx.token)),
  listIssueComments: (s, req) => {
    const i = issueOf(s, req.repo, req.number);
    const repo = s.repo(req.repo);
    return {
      comments: i.comments.map((c) => ({ id: c.id, subject: { kind: "SUBJECT_KIND_ISSUE" as const, repo: { fullName: repo.repo.fullName }, id: String(i.number) }, author: c.author, body: c.body, createdAt: c.createdAt })),
      page: { nextPageToken: "" },
    };
  },
  listIssueSubIssues: (s, req) => {
    const i = issueOf(s, req.repo, req.number);
    const subs = s.issues.filter((x) => x.repoId === i.repoId && x.parentNumber === i.number);
    const done = subs.filter((x) => x.state === "closed").length;
    return {
      subIssues: subs.map((x) => ({ id: x.id, number: String(x.number), title: x.title, state: x.state === "closed" ? ("ISSUE_STATE_CLOSED" as const) : ("ISSUE_STATE_OPEN" as const), assignees: x.assignees })),
      progress: { total: subs.length, completed: done, percentCompleted: subs.length ? Math.round((done * 100) / subs.length) : 0 },
      page: { nextPageToken: "" },
    };
  },
  addIssueAssignees: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const i = issueOf(s, req.repo, req.number);
    for (const id of req.assignees ?? []) {
      const a = s.principals.get(s.handleOf(id));
      if (!a) throw new BeeError("not_found", { reason: "PRINCIPAL_NOT_FOUND", message: "no principal " + id });
      if (i.assignees.some((x) => x.id === a.id)) continue;
      i.assignees.push(a);
      s.emit(repo, "issue.assigned", actor, { note: { title: "#" + i.number + " → " + a.display, target: "issue:" + i.number } });
      if (a.kind === "PRINCIPAL_KIND_AGENT" && i.state === "open") {
        // Assigning an agent starts its work (PRODUCT §4.15): it comments with a plan and moves the card.
        i.comments.push({ id: ulid(s.now(), "ic:auto:" + i.number + ":" + a.id), author: a, body: "Picked this up. I'll take a lease on the paths I touch and open a change request.", createdAt: s.iso(s.now()) });
        const proj = s.projects.get(repo.name);
        if (proj) {
          for (const col of proj.columns) col.issueNumbers = col.issueNumbers.filter((n) => n !== i.number);
          proj.columns.find((c) => c.id === "doing")?.issueNumbers.unshift(i.number);
        }
        s.emit(repo, "agent.started", s.handleOf(a.id), { note: { title: a.display + " started #" + i.number, target: "issue:" + i.number } });
      }
    }
    s.appendAudit(actor, "issue.assign", "#" + i.number + " → " + (i.assignees.map((a) => a.display).join(", ") || "nobody"), repo.name, true, "");
    return { issue: issueJson(s, i), agentRuns: [] };
  },
  removeIssueAssignees: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const i = issueOf(s, req.repo, req.number);
    const drop = new Set(req.assignees ?? []);
    i.assignees = i.assignees.filter((a) => !drop.has(a.id));
    s.appendAudit(actor, "issue.unassign", "#" + i.number, repo.name, true, "");
    return issueJson(s, i);
  },
  closeIssue: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const i = issueOf(s, req.repo, req.number);
    i.state = "closed";
    s.emit(repo, "issue.closed", actor, { note: { title: "#" + i.number + " " + i.title, target: "issue:" + i.number } });
    return issueJson(s, i);
  },
  reopenIssue: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const i = issueOf(s, req.repo, req.number);
    i.state = "open";
    s.emit(repo, "issue.reopened", actor, { note: { title: "#" + i.number + " " + i.title, target: "issue:" + i.number } });
    return issueJson(s, i);
  },
  createIssueComment: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const i = issueOf(s, req.repo, req.number);
    const body = req.body ?? "";
    if (!body.trim()) throw invalid("a comment needs a body");
    const c = { id: ulid(s.now(), "ic:" + i.number + ":" + i.comments.length), author: s.p(actor), body, createdAt: s.iso(s.now()) };
    i.comments.push(c);
    return { ...c, subject: { kind: "SUBJECT_KIND_ISSUE" as const, repo: { fullName: repo.repo.fullName }, id: String(i.number) } };
  },
});

// ---------------------------------------------------------------- projects

const STATUS_FIELD = "status";

function projectById(s: FixtureStore, id: string | undefined) {
  for (const [name, p] of s.projects) if (p.id === id) return { name, p };
  throw new BeeError("not_found", { reason: "PROJECT_NOT_FOUND", message: "no project " + id });
}

const itemId = (projectId: string, number: number) => projectId + ":" + number;

function projectItems(s: FixtureStore, name: string, p: T.Project): ProjectItemJson[] {
  const repo = s.repos.get(name);
  return p.columns.flatMap((col, ci) =>
    col.issueNumbers.map((n, i) => {
      const issue = s.issues.find((x) => x.repoId === p.repoId && x.number === n);
      return {
        id: itemId(p.id, n),
        projectId: p.id,
        kind: "PROJECT_ITEM_KIND_ISSUE" as const,
        content: { kind: "SUBJECT_KIND_ISSUE" as const, repo: { fullName: repo?.repo.fullName ?? "" }, id: issue?.id ?? "" },
        title: issue?.title ?? "",
        repoFullName: repo?.repo.fullName ?? "",
        number: String(n),
        contentState: issue?.state ?? "",
        assignees: issue?.assignees ?? [],
        fieldValues: [{ fieldId: STATUS_FIELD, fieldName: "Status", kind: "PROJECT_FIELD_KIND_SINGLE_SELECT" as const, optionId: col.id, displayValue: col.name }],
        // Lexicographic positions keep each column's order.
        position: String(ci).padStart(3, "0") + ":" + String(i).padStart(5, "0"),
      };
    }),
  );
}

function itemOf(s: FixtureStore, projectId: string | undefined, id: string | undefined) {
  const { name, p } = projectById(s, projectId);
  const n = Number(String(id ?? "").split(":").pop());
  const col = p.columns.find((c) => c.issueNumbers.includes(n));
  if (!col) throw new BeeError("not_found", { reason: "NOT_FOUND", message: "no project item " + id });
  return { name, p, n, col };
}

const projects = defineService(ProjectService, {
  listProjects: (s, req) => {
    const repo = req.repo ? s.repo(req.repo) : undefined;
    const list = [...s.projects.entries()].filter(([name]) => !repo || name === repo.name);
    return {
      projects: list.map(([name, p], i) => ({ id: p.id, ownerLogin: s.org.name, ownerKind: "PROJECT_OWNER_KIND_ORG" as const, number: String(i + 1), title: p.name, itemCount: projectItems(s, name, p).length, linkedRepoIds: [p.repoId] })),
      page: { nextPageToken: "" },
    };
  },
  listProjectFields: (s, req) => {
    const { p } = projectById(s, req.projectId);
    return {
      fields: [
        { id: "title", projectId: p.id, name: "Title", kind: "PROJECT_FIELD_KIND_TITLE" as const, builtIn: true },
        { id: STATUS_FIELD, projectId: p.id, name: "Status", kind: "PROJECT_FIELD_KIND_SINGLE_SELECT" as const, builtIn: true, options: p.columns.map((c) => ({ id: c.id, name: c.name })) },
      ],
      page: { nextPageToken: "" },
    };
  },
  listProjectItems: (s, req) => {
    const { name, p } = projectById(s, req.projectId);
    const items = projectItems(s, name, p);
    return { items, totalCount: String(items.length), page: { nextPageToken: "" } };
  },
  setProjectItemFieldValue: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const { name, p, n } = itemOf(s, req.projectId, req.itemId);
    if (req.value?.fieldId !== STATUS_FIELD) throw invalid("the fixture board only has a Status field");
    const to = p.columns.find((c) => c.id === req.value?.optionId);
    if (!to) throw invalid("no column " + req.value?.optionId);
    for (const c of p.columns) c.issueNumbers = c.issueNumbers.filter((x) => x !== n);
    to.issueNumbers.push(n);
    s.appendAudit(actor, "project.move", "#" + n + " → " + to.name, name, true, "");
    return projectItems(s, name, p).find((i) => i.id === itemId(p.id, n)) as ProjectItemJson;
  },
  moveProjectItem: (s, req) => {
    const { name, p, n, col } = itemOf(s, req.projectId, req.itemId);
    col.issueNumbers = col.issueNumbers.filter((x) => x !== n);
    const after = req.afterItemId ? Number(req.afterItemId.split(":").pop()) : NaN;
    const at = Number.isFinite(after) ? col.issueNumbers.indexOf(after) + 1 : 0;
    col.issueNumbers.splice(at, 0, n);
    return projectItems(s, name, p).find((i) => i.id === itemId(p.id, n)) as ProjectItemJson;
  },
});

// ---------------------------------------------------------------- releases

const releases = defineService(ReleaseService, {
  listReleases: (s, req) => {
    const repo = s.repo(req.repo);
    const list = s.releases
      .filter((r) => r.repoId === repo.repo.id && (req.includePrereleases || !r.prerelease))
      .sort((a, b) => Date.parse(b.createdAt ?? "") - Date.parse(a.createdAt ?? ""));
    return { releases: list.map(releaseJson), page: { nextPageToken: "" } };
  },
  generateReleaseNotes: (s, req) => {
    const repo = s.repo(req.repo);
    const prev = req.previousTagName
      ? s.releases.find((r) => r.tag === req.previousTagName)
      : s.releases.filter((r) => r.repoId === repo.repo.id && !r.prerelease).sort((a, b) => Date.parse(b.createdAt ?? "") - Date.parse(a.createdAt ?? ""))[0];
    const since = prev ? Date.parse(prev.createdAt ?? "") : 0;
    const landed = s.crs.filter((c) => c.repo === repo.name && c.state === "CHANGE_STATE_LANDED");
    const commits = repo.main.filter((c) => c.time > since);
    const lines = commits.map((c) => {
      const cr = landed.find((x) => x.changeId.raw === c.changeId.raw || x.title === c.message);
      return "- " + c.message + " (" + s.p(c.author).display + (cr ? ", " + cr.id.slice(-6).toLowerCase() : "") + ")";
    });
    return {
      name: req.tagName ?? "",
      body: "## Changes since " + (prev?.tag ?? "the first commit") + "\n\n" + (lines.reverse().join("\n") || "- No landed changes") + "\n",
      previousTagName: prev?.tag ?? "",
    };
  },
  createRelease: (s, req, ctx) => {
    const actor = s.caller(ctx.token);
    const repo = s.repo(req.repo);
    const tag = req.tagName ?? "";
    if (!/^v\d+\.\d+\.\d+(-[\w.]+)?$/.test(tag)) throw invalid("tags look like v1.4.0 or v1.4.0-rc.2", "INVALID_TAG");
    if (s.releases.some((r) => r.repoId === repo.repo.id && r.tag === tag)) throw new BeeError("already_exists", { reason: "TAG_EXISTS", message: tag + " already exists and releases are immutable" });
    const rel: T.Release = {
      id: ulid(s.now(), "release:" + tag),
      repoId: repo.repo.id,
      tag,
      name: req.name || tag,
      notes: req.body ?? "",
      target: s.tip(repo).id,
      author: s.p(actor),
      prerelease: req.prerelease ?? false,
      immutable: req.immutable ?? false,
      assets: [],
      createdAt: s.iso(s.now()),
    };
    s.releases.push(rel);
    s.emit(repo, "release.published", actor, { note: { title: rel.tag, target: "release:" + rel.tag } });
    s.appendAudit(actor, "release.create", rel.tag, repo.name, true, "");
    return releaseJson(rel);
  },
});

// ---------------------------------------------------------------- security

function alertsOf(s: FixtureStore, ref: { id?: string; fullName?: string } | undefined, kind: T.SecurityAlert["kind"]) {
  const repo = s.repo(ref);
  return s.alerts.filter((a) => a.repoId === repo.repo.id && a.kind === kind);
}

function alertOf(s: FixtureStore, ref: { id?: string; fullName?: string } | undefined, kind: T.SecurityAlert["kind"], number: string | undefined) {
  const a = alertsOf(s, ref, kind).find((x) => String(alertNumber(s, x)) === number);
  if (!a) throw new BeeError("not_found", { reason: "ALERT_NOT_FOUND", message: "no " + kind.replace("_", " ") + " alert #" + number });
  return a;
}

function dismissAlert(s: FixtureStore, ctx: Ctx, ref: { id?: string; fullName?: string } | undefined, a: T.SecurityAlert, state: T.SecurityAlert["state"], comment: string) {
  const actor = s.caller(ctx.token);
  const repo = s.repo(ref);
  if (state === "dismissed" && !comment.trim()) throw invalid("say why you are dismissing this alert", "REASON_REQUIRED");
  a.state = state;
  a.dismissedReason = state === "dismissed" ? comment : "";
  if (state === "dismissed") {
    s.emit(repo, "security.dismissed", actor, { note: { title: a.title, target: "alert:" + a.id } });
    s.appendAudit(actor, "security.dismiss", a.title + " (" + comment + ")", repo.name, true, "");
  }
}

const security = defineService(SecurityService, {
  listDependencyAlerts: (s, req) => {
    const states = req.states ?? [];
    const list = alertsOf(s, req.repo, "dependency").map((a) => dependencyAlert(s, a)).filter((a) => !states.length || states.includes(a.state ?? "DEPENDENCY_ALERT_STATE_UNSPECIFIED"));
    return { alerts: list, page: { nextPageToken: "" } };
  },
  listSecretScanningAlerts: (s, req) => {
    const want = req.state && req.state !== "SECRET_SCANNING_ALERT_STATE_UNSPECIFIED" ? req.state : null;
    return { alerts: alertsOf(s, req.repo, "secret").map((a) => secretAlert(s, a)).filter((a) => !want || a.state === want), page: { nextPageToken: "" } };
  },
  listSecretScanningLocations: (s, req) => {
    const a = alertOf(s, req.repo, "secret", req.number);
    const { path, line } = secretParts(a);
    return { locations: path ? [{ blob: { path: pathBytes(path), line, secretType: a.advisory.replace(/^secret:/, "") } }] : [], page: { nextPageToken: "" } };
  },
  listCodeScanningAlerts: (s, req) => {
    const states = req.states ?? [];
    return { alerts: alertsOf(s, req.repo, "code_scanning").map((a) => codeAlert(s, a)).filter((a) => !states.length || states.includes(a.state ?? "CODE_SCANNING_ALERT_STATE_UNSPECIFIED")), page: { nextPageToken: "" } };
  },
  updateDependencyAlert: (s, req, ctx) => {
    const a = alertOf(s, req.repo, "dependency", req.number);
    dismissAlert(s, ctx, req.repo, a, req.state === "DEPENDENCY_ALERT_STATE_OPEN" ? "open" : req.state === "DEPENDENCY_ALERT_STATE_FIXED" ? "fixed" : "dismissed", req.dismissedComment ?? "");
    return dependencyAlert(s, a);
  },
  updateSecretScanningAlert: (s, req, ctx) => {
    const a = alertOf(s, req.repo, "secret", req.number);
    const state = req.state === "SECRET_SCANNING_ALERT_STATE_OPEN" ? "open" : req.resolution === "SECRET_SCANNING_RESOLUTION_REVOKED" ? "fixed" : "dismissed";
    dismissAlert(s, ctx, req.repo, a, state, req.resolutionComment ?? "");
    return secretAlert(s, a);
  },
  updateCodeScanningAlert: (s, req, ctx) => {
    const a = alertOf(s, req.repo, "code_scanning", req.number);
    dismissAlert(s, ctx, req.repo, a, req.state === "CODE_SCANNING_ALERT_STATE_OPEN" ? "open" : req.state === "CODE_SCANNING_ALERT_STATE_FIXED" ? "fixed" : "dismissed", req.dismissedComment ?? "");
    return codeAlert(s, a);
  },
});

// ---------------------------------------------------------------- policy

/** How recent change requests would be decided under proposed rules (ValidatePolicy.replay). */
function replayPolicy(s: FixtureStore, ref: { id?: string; fullName?: string } | undefined, rules: ReturnType<typeof parsePolicy>["rules"], sampleSize: number) {
  const repo = s.repo(ref);
  const current = s.rules(repo);
  const sample = s.crs
    .filter((c) => c.repo === repo.name && c.state !== "CHANGE_STATE_CLOSED")
    .sort((a, b) => b.createdAt - a.createdAt)
    .slice(0, Math.max(1, Math.min(50, sampleSize)));
  return sample.map((c) => {
    // Landed changes are judged as they were when they landed: ignore their "landed" state.
    const saved = c.state;
    c.state = "CHANGE_STATE_OPEN";
    const before = s.evaluate(c, current);
    const after = s.evaluate(c, rules);
    c.state = saved;
    return { changeRequestId: c.id, title: c.title + (saved === "CHANGE_STATE_LANDED" ? " (landed)" : ""), diffClass: after.diffClass, currentAllowed: before.allowed, proposedAllowed: after.allowed, missing: after.missing };
  });
}

const policy = defineService(PolicyService, {
  getEffectivePolicy: (s, req) => {
    const repo = s.repo(req.repo);
    const tip = repo.refs.get(repo.repo.defaultRef)?.target;
    const source = tip?.files.get(POLICY_PATH) ?? "";
    return {
      repo: { fullName: repo.repo.fullName },
      ref: repo.repo.defaultRef,
      epoch: s.policyEpoch(source),
      normalizedJson: normalizedPolicy(parsePolicy(source).rules),
      sources: source && tip ? [{ kind: "POLICY_SOURCE_KIND_REPO_FILE" as const, path: POLICY_PATH, changeset: tip.id }] : [],
    };
  },
  validatePolicy: (s, req) => {
    const toml = req.toml ?? "";
    const parsed = parsePolicy(toml);
    const replay = parsed.errors.length || !req.replaySample ? undefined : replayPolicy(s, req.repo, parsed.rules, req.replaySample);
    return {
      valid: parsed.errors.length === 0,
      issues: parsed.errors.map((e) => ({ severity: "POLICY_ISSUE_SEVERITY_ERROR" as const, file: POLICY_PATH, line: e.line, message: e.message })),
      normalizedJson: parsed.errors.length ? "" : normalizedPolicy(parsed.rules),
      resultingEpoch: parsed.errors.length ? "" : s.policyEpoch(toml),
      ruleSummaries: replay ? describeRules(parsed.rules) : [],
      replay: replay ?? [],
    };
  },
});

// ---------------------------------------------------------------- audit

function auditFiltered(s: FixtureStore, req: { org?: string; filter?: { actorId?: string; actionPrefixes?: string[]; repo?: { id?: string; fullName?: string }; deniedOnly?: boolean } }) {
  if (req.org && req.org !== s.org.name) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no org " + req.org });
  const f = req.filter ?? {};
  const prefixes = f.actionPrefixes ?? [];
  const repo = f.repo?.fullName ?? (f.repo?.id ? s.repo(f.repo).repo.fullName : "");
  return s.audit
    .filter((e) => !f.actorId || e.actor?.id === f.actorId)
    .filter((e) => !prefixes.length || prefixes.some((p) => e.action.startsWith(p)))
    .filter((e) => !repo || e.repo === repo)
    .filter((e) => !f.deniedOnly || !e.allowed)
    .slice()
    .reverse();
}

const audit = defineService(AuditService, {
  queryAuditLog: (s, req) => {
    const pg = page(auditFiltered(s, req), req.page, 100);
    return { entries: pg.items.map((e) => auditEntry(s, e)), page: { nextPageToken: pg.next } };
  },
  verifyAuditChain: (s) => {
    const ok = s.verifyAudit();
    const last = s.audit[s.audit.length - 1];
    return { ok, verifiedFromSeq: "1", verifiedToSeq: last?.seq ?? "0", failure: ok ? "" : "hash chain broken" };
  },
});

export const VIEW_SERVICES: ServiceDef[] = [agents, workflows, deployments, issues, projects, releases, security, policy, audit];
