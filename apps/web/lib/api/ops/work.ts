/** Runs (workflows.proto), issues (issues.proto) and the board (projects.proto). */
import type { CommentJson } from "../../gen/bee/v1/content_pb";
import { DeploymentService } from "../../gen/bee/v1/deployments_pb";
import { ChangeService, CheckService } from "../../gen/bee/v1/forge_pb";
import { IssueService, type IssueJson } from "../../gen/bee/v1/issues_pb";
import { ProjectService, type ProjectFieldJson, type ProjectItemJson } from "../../gen/bee/v1/projects_pb";
import {
  WorkflowService,
  type WorkflowConclusionJson,
  type WorkflowJobJson,
  type WorkflowJson,
  type WorkflowRunJson,
  type WorkflowStatusJson,
} from "../../gen/bee/v1/workflows_pb";
import { base64ToBytes, pathText } from "../ids";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { namer } from "./agents";
import { clean, list, mapLimit, notFound, num, pageOf, str, wire, type PartialOps } from "./shared";

// ---------------------------------------------------------------- runs

export function runStatus(status: string | undefined, conclusion: string | undefined): T.RunStatus {
  switch (status as WorkflowStatusJson | undefined) {
    case "WORKFLOW_STATUS_WAITING":
      return "protection";
    case "WORKFLOW_STATUS_ACTION_REQUIRED":
      return "waiting";
    case "WORKFLOW_STATUS_IN_PROGRESS":
      return "running";
    case "WORKFLOW_STATUS_COMPLETED":
      switch (conclusion as WorkflowConclusionJson | undefined) {
        case "WORKFLOW_CONCLUSION_ACTION_REQUIRED":
          return "waiting";
        case "WORKFLOW_CONCLUSION_FAILURE":
        case "WORKFLOW_CONCLUSION_TIMED_OUT":
        case "WORKFLOW_CONCLUSION_STARTUP_FAILURE":
          return "failed";
        case "WORKFLOW_CONCLUSION_CANCELLED":
          return "cancelled";
        case "WORKFLOW_CONCLUSION_SKIPPED":
          return "skipped";
        case "WORKFLOW_CONCLUSION_NEUTRAL":
          return "neutral";
        case "WORKFLOW_CONCLUSION_STALE":
          return "stale";
        case "WORKFLOW_CONCLUSION_SUCCESS":
          return "passed";
        default:
          return "completed";
      }
    default:
      return "queued";
  }
}

/** The status filter of the runs list as WorkflowStatus and WorkflowConclusion. */
export function runFilter(s: T.RunStatus | "" | undefined): { status?: WorkflowStatusJson; conclusion?: WorkflowConclusionJson } {
  switch (s) {
    case "running":
      return { status: "WORKFLOW_STATUS_IN_PROGRESS" };
    case "queued":
      return { status: "WORKFLOW_STATUS_QUEUED" };
    case "passed":
      return { status: "WORKFLOW_STATUS_COMPLETED", conclusion: "WORKFLOW_CONCLUSION_SUCCESS" };
    case "failed":
      return { status: "WORKFLOW_STATUS_COMPLETED", conclusion: "WORKFLOW_CONCLUSION_FAILURE" };
    case "cancelled":
      return { status: "WORKFLOW_STATUS_COMPLETED", conclusion: "WORKFLOW_CONCLUSION_CANCELLED" };
    default:
      return {};
  }
}

const ms = (t: string | undefined) => (t ? Date.parse(t) : NaN);

export function jobView(j: WorkflowJobJson): T.RunJob {
  return {
    id: str(j.id),
    name: str(j.name),
    status: runStatus(j.status, j.conclusion),
    steps: list(j.steps).map((s) => {
      const d = ms(s.completedAt) - ms(s.startedAt);
      return { name: str(s.name), status: runStatus(s.status, s.conclusion), durationMs: String(Number.isFinite(d) ? d : 0) };
    }),
    startedAt: j.startedAt,
    finishedAt: j.completedAt,
    runner: str(j.runnerName),
  };
}

export function runView(r: WorkflowRunJson, jobs: WorkflowJobJson[], crId: string, annotations: T.Annotation[] = []): T.Run {
  return {
    id: str(r.id),
    repoId: str(r.repo?.id),
    workflow: str(r.name),
    number: num(r.runNumber),
    trigger: str(r.event),
    ref: str(r.headRef),
    head: r.headChangeset as T.ObjectId | undefined,
    changeRequestId: crId,
    actor: r.actor as T.Principal | undefined,
    status: runStatus(r.status, r.conclusion),
    jobs: jobs.map(jobView),
    annotations,
    attempt: num(r.runAttempt),
    changeRequestNumber: list(r.changeRequestNumbers)[0],
    triggeringActor: r.triggeringActor as T.Principal | undefined,
    startedAt: r.runStartedAt,
    createdAt: r.createdAt,
    finishedAt: r.completedAt,
  };
}

async function jobsOf(rpc: Rpc, repo: T.RepoRef, runId: string, opts?: CallOptions): Promise<WorkflowJobJson[]> {
  return list((await rpc.call(WorkflowService.method.listWorkflowJobs, { repo, runId, filter: "WORKFLOW_JOB_ATTEMPT_FILTER_LATEST", page: { pageSize: 100 } }, opts)).jobs);
}

const LEVEL: Record<string, T.Annotation["level"]> = { ANNOTATION_LEVEL_FAILURE: "failure", ANNOTATION_LEVEL_WARNING: "warning", ANNOTATION_LEVEL_NOTICE: "notice" };

async function annotationsOf(rpc: Rpc, repo: T.RepoRef, jobs: WorkflowJobJson[], opts?: CallOptions): Promise<T.Annotation[]> {
  const withChecks = jobs.filter((j) => j.checkRunId);
  const per = await mapLimit(withChecks, 8, async (j) => ({
    jobId: str(j.id),
    res: await rpc.call(CheckService.method.listCheckAnnotations, { repo, checkRunId: str(j.checkRunId), page: { pageSize: 200 } }, opts),
  }));
  return per.flatMap(({ jobId, res }) =>
    list(res.annotations).map((a) => ({ jobId, path: pathText(str(a.path)), line: num(a.startLine), level: LEVEL[str(a.level)] ?? "notice", title: str(a.title), message: str(a.message) })),
  );
}

async function fullRun(rpc: Rpc, repo: T.RepoRef, run: WorkflowRunJson, withAnnotations: boolean, opts?: CallOptions): Promise<T.Run> {
  const jobs = await jobsOf(rpc, repo, str(run.id), opts);
  const annotations = withAnnotations ? await annotationsOf(rpc, repo, jobs, opts) : [];
  const name = await namer(rpc, [run.actor, run.triggeringActor], opts);
  const v = runView(run, jobs, str(list(run.changeRequestIds)[0]), annotations);
  return { ...v, actor: name(v.actor), triggeringActor: name(v.triggeringActor) };
}

const MARK = /^##\[(group|error|warning)\](.*)$/;
/** GitHub-style workflow commands: `::error file=a.ts,line=3,title=t::message`. */
const COMMAND = /^::(error|warning|notice)(?: ([^:]*))?::(.*)$/;

/** Splits a job log byte stream into lines; `##[group]` style markers set the level. */
async function* logLines(chunks: AsyncIterable<{ offset?: string; data?: string }>, jobId: string, from: number): AsyncGenerator<T.LogLine> {
  const dec = new TextDecoder();
  let buf = new Uint8Array(0);
  let pos = from; // absolute offset of buf[0]
  let n = 0;
  const line = (bytes: Uint8Array, end: number): T.LogLine => {
    const text = dec.decode(bytes).replace(/\r$/, "");
    const m = MARK.exec(text);
    if (m) return { n: ++n, jobId, text: m[2] ?? "", level: m[1] as T.LogLine["level"], end: String(end) };
    const c = COMMAND.exec(text);
    if (c) {
      const where = /file=([^,]*)(?:,line=(\d+))?/.exec(c[2] ?? "");
      const at = where ? (where[1] ?? "") + (where[2] ? ":" + where[2] : "") + ": " : "";
      return { n: ++n, jobId, text: at + (c[3] ?? ""), level: c[1] === "notice" ? "info" : (c[1] as "error" | "warning"), end: String(end) };
    }
    return { n: ++n, jobId, text, level: "info", end: String(end) };
  };
  for await (const c of chunks) {
    const data = base64ToBytes(str(c.data));
    const at = num(c.offset);
    if (at + data.length <= pos + buf.length) continue; // already seen
    const fresh = data.subarray(Math.max(0, pos + buf.length - at));
    const merged = new Uint8Array(buf.length + fresh.length);
    merged.set(buf);
    merged.set(fresh, buf.length);
    buf = merged;
    let nl: number;
    while ((nl = buf.indexOf(10)) >= 0) {
      yield line(buf.subarray(0, nl), pos + nl + 1);
      pos += nl + 1;
      buf = buf.slice(nl + 1);
    }
  }
  if (buf.length) yield line(buf, pos + buf.length);
}

// ---------------------------------------------------------------- issues

export function issueView(i: IssueJson, comments: CommentJson[] = [], subIssues: number[] = []): T.Issue {
  return {
    id: str(i.id),
    repoId: str(i.repoId),
    number: num(i.number),
    title: str(i.title),
    body: str(i.body),
    state: i.state === "ISSUE_STATE_CLOSED" ? "closed" : "open",
    type: str(i.issueType?.name).toLowerCase(),
    labels: list(i.labels).map((l) => str(l.name)),
    milestone: str(i.milestone?.title),
    assignees: list(i.assignees) as T.Principal[],
    author: i.author as T.Principal | undefined,
    parentNumber: num(i.parent?.number),
    subIssues,
    subIssuesTotal: num(i.subIssues?.total),
    subIssuesDone: num(i.subIssues?.completed),
    linkedChanges: list(i.linkedChangeRequestIds),
    comments: comments.map((c) => ({ id: str(c.id), author: c.author as T.Principal | undefined, body: str(c.body), createdAt: c.createdAt })),
    commentCount: num(i.commentCount),
    createdAt: i.createdAt,
    ...(i.inspiredBy?.postId ? { inspiredBy: i.inspiredBy as T.PostRef } : {}),
  };
}

async function fullIssue(rpc: Rpc, repo: T.RepoRef, number: number, opts?: CallOptions): Promise<T.Issue> {
  const n = String(number);
  const [issue, comments, subs] = await Promise.all([
    rpc.call(IssueService.method.getIssueByNumber, { repo, number: n }, opts),
    rpc.call(IssueService.method.listIssueComments, { repo, number: n, page: { pageSize: 200 } }, opts),
    rpc.call(IssueService.method.listIssueSubIssues, { repo, number: n, page: { pageSize: 200 } }, opts),
  ]);
  return issueView(issue, list(comments.comments), list(subs.subIssues).map((s) => num(s.number)));
}

// ---------------------------------------------------------------- board

const STATUS_FIELD = "Status";

async function board(rpc: Rpc, repo: T.RepoRef, opts?: CallOptions): Promise<{ project: T.Project; field: ProjectFieldJson; items: ProjectItemJson[] }> {
  const projects = list((await rpc.call(ProjectService.method.listProjects, { repo, page: { pageSize: 1 } }, opts)).projects);
  const p = projects[0];
  if (!p) throw notFound("NOT_FOUND", "no project is linked to this repository");
  const projectId = str(p.id);
  const [fields, items] = await Promise.all([
    rpc.call(ProjectService.method.listProjectFields, { projectId, page: { pageSize: 100 } }, opts),
    rpc.call(ProjectService.method.listProjectItems, { projectId, page: { pageSize: 200 } }, opts),
  ]);
  const field = list(fields.fields).find((f) => f.kind === "PROJECT_FIELD_KIND_SINGLE_SELECT" && f.name === STATUS_FIELD);
  if (!field) throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "the project has no Status field" });
  const all = list(items.items)
    .filter((i) => i.kind === "PROJECT_ITEM_KIND_ISSUE" && !i.archived)
    .sort((a, b) => (str(a.position) < str(b.position) ? -1 : str(a.position) > str(b.position) ? 1 : 0));
  const columnOf = (i: ProjectItemJson) => list(i.fieldValues).find((v) => v.fieldId === field.id)?.optionId ?? "";
  const project: T.Project = {
    id: projectId,
    name: str(p.title),
    repoId: str(list(p.linkedRepoIds)[0]),
    columns: list(field.options).map((o) => ({ id: str(o.id), name: str(o.name), issueNumbers: all.filter((i) => columnOf(i) === o.id).map((i) => num(i.number)) })),
  };
  return { project, field, items: all };
}

// ---------------------------------------------------------------- ops

function workflowView(w: WorkflowJson): T.WorkflowDefinition {
  return { id: str(w.id), name: str(w.name), path: str(w.path), state: w.state };
}

export const workOps = {
  WorkflowService: {
    ListRuns: {
      wire: (r) => wire(WorkflowService.method.listWorkflowRuns, { repo: r.repo, ...runFilter(r.status), includeJobs: true, page: r.page }),
      run: async (rpc, r, opts) => {
        const changeRequestNumber = r.changeRequestId ? str((await rpc.call(ChangeService.method.getChange, { repo: r.repo, id: r.changeRequestId }, opts)).number) : undefined;
        if (changeRequestNumber === "0") return { runs: [], page: { nextPageToken: "" } };
        const res = await rpc.call(WorkflowService.method.listWorkflowRuns, { repo: r.repo, ...runFilter(r.status), changeRequestNumber, includeJobs: true, page: r.page }, opts);
        const runs = list(res.runs).map((run) => runView(run, list(run.jobs), str(list(run.changeRequestIds)[0])));
        const name = await namer(rpc, runs.flatMap((x) => [x.actor, x.triggeringActor]), opts);
        return { runs: runs.map((x) => ({ ...x, actor: name(x.actor), triggeringActor: name(x.triggeringActor) })), page: pageOf(res.page) };
      },
    },
    GetRun: {
      wire: (r) => wire(WorkflowService.method.getWorkflowRun, { repo: r.repo, runId: r.id }),
      run: async (rpc, r, opts) => fullRun(rpc, r.repo, await rpc.call(WorkflowService.method.getWorkflowRun, { repo: r.repo, runId: r.id }, opts), true, opts),
    },
    Rerun: {
      wire: (r) => wire(r.failedOnly ? WorkflowService.method.rerunFailedWorkflowJobs : WorkflowService.method.rerunWorkflowRun, { repo: r.repo, runId: r.id }),
      run: async (rpc, r, opts) => {
        const req = { repo: r.repo, runId: r.id };
        const run = r.failedOnly ? await rpc.call(WorkflowService.method.rerunFailedWorkflowJobs, req, opts) : await rpc.call(WorkflowService.method.rerunWorkflowRun, req, opts);
        return fullRun(rpc, r.repo, run, false, opts);
      },
    },
    CancelRun: {
      wire: (r) => wire(WorkflowService.method.cancelWorkflowRun, { repo: r.repo, runId: r.id }),
      run: (rpc, r, opts) => rpc.call(WorkflowService.method.cancelWorkflowRun, { repo: r.repo, runId: r.id }, opts),
    },
    ListWorkflows: {
      wire: (r) => wire(WorkflowService.method.listWorkflows, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(WorkflowService.method.listWorkflows, r, opts);
        return { workflows: list(res.workflows).map(workflowView), page: pageOf(res.page) };
      },
    },
    EnableWorkflow: {
      wire: (r) => wire(WorkflowService.method.enableWorkflow, r),
      run: async (rpc, r, opts) => workflowView(await rpc.call(WorkflowService.method.enableWorkflow, r, opts)),
    },
    DisableWorkflow: {
      wire: (r) => wire(WorkflowService.method.disableWorkflow, r),
      run: async (rpc, r, opts) => workflowView(await rpc.call(WorkflowService.method.disableWorkflow, r, opts)),
    },
    GetWorkflowBadge: {
      wire: (r) => wire(WorkflowService.method.getWorkflowBadge, r),
      run: async (rpc, r, opts) => {
        const badge = await rpc.call(WorkflowService.method.getWorkflowBadge, r, opts);
        return { message: str(badge.message), svg: str(badge.svg) };
      },
    },
    StreamLogs: {
      wire: (r) => wire(WorkflowService.method.tailWorkflowJobLogs, { repo: r.repo, jobId: r.jobId, offset: r.fromOffset, follow: true }),
      run: (rpc, r, opts) => logLines(rpc.stream(WorkflowService.method.tailWorkflowJobLogs, { repo: r.repo, jobId: r.jobId, offset: r.fromOffset, follow: true }, opts), r.jobId, num(r.fromOffset)),
    },
  },
  DeploymentService: {
    ListDeployments: {
      wire: (r) => wire(DeploymentService.method.listDeployments, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(DeploymentService.method.listDeployments, r, opts);
        return {
          deployments: list(res.deployments).map((d) => ({ id: str(d.id), environment: str(d.environment), ref: str(d.ref), commit: d.changeset as T.ObjectId | undefined, state: d.state })),
          page: pageOf(res.page),
        };
      },
    },
  },
  IssueService: {
    ListIssues: {
      wire: (r) => wire(IssueService.method.listIssues, listIssuesReq(r)),
      run: async (rpc, r, opts) => ({ issues: list((await rpc.call(IssueService.method.listIssues, listIssuesReq(r), opts)).issues).map((i) => issueView(i)) }),
    },
    GetIssue: {
      wire: (r) => wire(IssueService.method.getIssueByNumber, { repo: r.repo, number: String(r.number) }),
      run: (rpc, r, opts) => fullIssue(rpc, r.repo, r.number, opts),
    },
    CreateIssue: {
      wire: (r) => wire(IssueService.method.createIssue, r),
      run: async (rpc, r, opts) => {
        const made = await rpc.call(IssueService.method.createIssue, clean(r) as never, opts);
        return fullIssue(rpc, r.repo, num(made.number), opts);
      },
    },
    AssignIssue: {
      wire: (r) => wire(IssueService.method.addIssueAssignees, { repo: r.repo, number: String(r.number), assignees: r.assigneeIds }),
      run: async (rpc, r, opts) => {
        const number = String(r.number);
        const current = list((await rpc.call(IssueService.method.getIssueByNumber, { repo: r.repo, number }, opts)).assignees).map((a) => str(a.id));
        const add = r.assigneeIds.filter((id) => !current.includes(id));
        const remove = current.filter((id) => !r.assigneeIds.includes(id));
        if (remove.length) await rpc.call(IssueService.method.removeIssueAssignees, { repo: r.repo, number, assignees: remove }, opts);
        if (add.length) await rpc.call(IssueService.method.addIssueAssignees, { repo: r.repo, number, assignees: add }, opts);
        return fullIssue(rpc, r.repo, r.number, opts);
      },
    },
    SetIssueState: {
      wire: (r) => wire(r.state === "closed" ? IssueService.method.closeIssue : IssueService.method.reopenIssue, { repo: r.repo, number: String(r.number) }),
      run: async (rpc, r, opts) => {
        const req = { repo: r.repo, number: String(r.number) };
        if (r.state === "closed") await rpc.call(IssueService.method.closeIssue, { ...req, reason: "ISSUE_STATE_REASON_COMPLETED" }, opts);
        else await rpc.call(IssueService.method.reopenIssue, req, opts);
        return fullIssue(rpc, r.repo, r.number, opts);
      },
    },
    CommentIssue: {
      wire: (r) => wire(IssueService.method.createIssueComment, { repo: r.repo, number: String(r.number), body: r.body }),
      run: async (rpc, r, opts) => {
        await rpc.call(IssueService.method.createIssueComment, { repo: r.repo, number: String(r.number), body: r.body }, opts);
        return fullIssue(rpc, r.repo, r.number, opts);
      },
    },
  },
  ProjectService: {
    GetProject: {
      wire: (r) => wire(ProjectService.method.listProjects, { repo: r.repo }),
      run: async (rpc, r, opts) => (await board(rpc, r.repo, opts)).project,
    },
    MoveItem: {
      wire: (r) => wire(ProjectService.method.setProjectItemFieldValue, { projectId: r.projectId, itemId: "<item of #" + r.issueNumber + ">", value: { optionId: r.toColumnId } }),
      run: async (rpc, r, opts) => {
        const { field, items } = await board(rpc, r.repo, opts);
        const item = items.find((i) => num(i.number) === r.issueNumber);
        if (!item) throw notFound("NOT_FOUND", "#" + r.issueNumber + " is not on this board");
        if (!list(field.options).some((o) => o.id === r.toColumnId)) throw new BeeError("invalid_argument", { reason: "INVALID_ARGUMENT", message: "no column " + r.toColumnId });
        const itemId = str(item.id);
        await rpc.call(ProjectService.method.setProjectItemFieldValue, { projectId: r.projectId, itemId, value: { fieldId: field.id, optionId: r.toColumnId } }, opts);
        await rpc.call(ProjectService.method.moveProjectItem, { projectId: r.projectId, itemId, afterItemId: "" }, opts);
        return (await board(rpc, r.repo, opts)).project;
      },
    },
  },
} satisfies PartialOps<"DeploymentService" | "WorkflowService" | "IssueService" | "ProjectService">;

function listIssuesReq(r: T.ListIssuesRequest) {
  return {
    repo: r.repo,
    ...(r.state === "open" ? { state: "ISSUE_STATE_OPEN" as const } : r.state === "closed" ? { state: "ISSUE_STATE_CLOSED" as const } : { allStates: true }),
    labels: r.label ? [r.label] : [],
    assignee: r.assigneeId || undefined,
    page: { pageSize: 200 },
  };
}
