/** Orgs, repos, change requests, checks, leases and events (forge.proto). */
import type { EventJson, MergeQueueEntryJson, ReviewCommentJson, ReviewThreadJson } from "../../gen/bee/v1/forge_pb";
import { ChangeService, CheckService, EventService, LeaseService, OrgService, RepoService } from "../../gen/bee/v1/forge_pb";
import type { CommentJson } from "../../gen/bee/v1/content_pb";
import { QueryService } from "../../gen/bee/v1/vcs_pb";
import { objectIdText, pathText } from "../ids";
import { principals } from "./agents";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { clean, collect, diffLines, list, mapLimit, num, pass, str, wire, type PartialOps, type StreamOp, type UnaryOp } from "./shared";

// ---------------------------------------------------------------- mapping helpers

export function threadComment(c: ReviewCommentJson | CommentJson): T.ThreadComment {
  const suggestion = "suggestion" in c ? (c.suggestion?.replacement ?? "") : "";
  return { id: str(c.id), author: c.author as T.Principal | undefined, body: str(c.body), suggestion, createdAt: c.createdAt };
}

export function threadView(t: ReviewThreadJson): T.ReviewThread {
  return {
    id: str(t.id),
    changeRequestId: str(t.changeRequestId),
    path: str(t.path),
    side: t.side === "old" ? "old" : "new",
    startLine: num(t.startLine),
    endLine: num(t.endLine),
    revision: num(t.revision),
    resolved: !!t.resolved,
    outdated: !!t.outdated,
    comments: list(t.comments).map(threadComment),
  };
}

const QUEUE_STATE: Record<string, T.QueueEntry["state"]> = {
  MERGE_QUEUE_STATE_CHECKING: "testing",
  MERGE_QUEUE_STATE_LANDING: "landing",
  MERGE_QUEUE_STATE_LANDED: "landing",
};

export function queueView(e: MergeQueueEntryJson, title: string): T.QueueEntry {
  return {
    changeRequestId: str(e.changeRequestId),
    title,
    position: num(e.position),
    state: QUEUE_STATE[str(e.state)] ?? "waiting",
    train: e.speculativeHead ? objectIdText(e.speculativeHead as T.ObjectId, true) : "",
    enqueuedAt: e.enqueuedAt,
  };
}

/** "acme/*" asks for every repo of the org (org-wide activity). */
export function orgWildcard(repo: T.RepoRef | undefined): string | null {
  const m = /^([\w.-]+)\/\*$/.exec(repo?.fullName ?? "");
  return m?.[1] ?? null;
}

/** The parts of `payload_json` bodies the activity stream reads (the subject resource or a summary). */
interface Payload {
  id?: string;
  number?: string | number;
  title?: string;
  tagName?: string;
  name?: string;
  displayTitle?: string;
  runNumber?: string | number;
  runId?: string;
  workflowName?: string;
  status?: string;
  conclusion?: string;
  reason?: string;
  path?: string;
  body?: string;
  package?: string;
  repo?: unknown;
  changeRequestId?: string;
  change_request_id?: string;
  secretTypeDisplayName?: string;
  manifests?: number;
  packages?: number;
  advisory?: { summary?: string };
  rule?: { name?: string };
  release?: { tagName?: string; name?: string };
  spec?: { title?: string };
  agent?: { display?: string };
  issue?: { number?: string | number; title?: string };
  subject?: { id?: string };
  findings?: { path?: string }[];
}

const NOTE_PREFIX: Record<string, string> = {
  SUBJECT_KIND_ISSUE: "issue",
  SUBJECT_KIND_WORKFLOW_RUN: "run",
  SUBJECT_KIND_RELEASE: "release",
  SUBJECT_KIND_CHANGE_REQUEST: "cr",
  SUBJECT_KIND_SECURITY_ALERT: "alert",
  SUBJECT_KIND_PACKAGE: "package",
  SUBJECT_KIND_SWARM: "swarm",
};

const words = (s: string) => s.replace(/_/g, " ");
const outcome = (p: Payload) => words(str(p.conclusion).replace("WORKFLOW_CONCLUSION_", "").replace("UNSPECIFIED", "").toLowerCase() || str(p.status).replace("WORKFLOW_STATUS_", "").toLowerCase());
const issueTitle = (n: unknown, title: string | undefined) => "#" + String(n ?? "") + (title ? " " + title : "");

/**
 * Verb, title and link target for events whose body rides in `payload_json` (SPEC §11, Event):
 * runs, jobs, issues, comments, threads, releases, alerts, packages and the like.
 */
export function noteOf(e: EventJson): T.EventNote | undefined {
  if (e.refUpdated || e.change || e.review || e.check || e.lease) return undefined;
  if (!e.subject && !e.payloadJson) return undefined;
  let p: Payload = {};
  try {
    p = e.payloadJson ? (JSON.parse(e.payloadJson) as Payload) : {};
  } catch {
    p = {};
  }
  const k = str(e.kind);
  const [area = "", ...restParts] = k.split(".");
  const rest = words(restParts.join("."));
  const kind = str(e.subject?.kind);
  const id = str(e.subject?.id);
  if (area === "workflow_run") return { verb: "run " + rest, title: (p.name || "run") + " #" + String(p.runNumber ?? "") + (rest === "completed" && outcome(p) ? ": " + outcome(p) : ""), target: "run:" + (p.id || id) };
  if (area === "workflow_job") return { verb: "job " + rest, title: [p.workflowName, p.name].filter(Boolean).join(" / ") + (rest === "completed" && outcome(p) ? ": " + outcome(p) : ""), target: "run:" + (p.runId || id) };
  if (area === "release") return { verb: rest, title: p.release?.tagName || p.tagName || p.name || id, target: "release:" + (p.release?.tagName || p.tagName || id) };
  if (k === "issue.assigned_to_agent") return { verb: "assigned " + (p.agent?.display || "an agent") + " to", title: issueTitle(p.number, p.title), target: "issue:" + String(p.number ?? id) };
  if (k === "issue.agent_run_finished") return { verb: "agent run finished" + (p.reason ? " (" + words(p.reason) + ")" : "") + " on", title: issueTitle(p.number, undefined), target: "issue:" + String(p.number ?? id) };
  if (k === "issue.change_linked") return { verb: "linked a change to", title: issueTitle(p.number, undefined), target: "issue:" + String(p.number ?? id) };
  if (area === "issue_comment") return { verb: "commented on", title: issueTitle(p.issue?.number, p.issue?.title), target: "issue:" + String(p.issue?.number ?? "") };
  if (area === "issue") return { verb: rest, title: issueTitle(p.number ?? id, p.title), target: "issue:" + String(p.number ?? id) };
  // Evidence events (BriefService): the subject is the change request, the payload the item.
  if (area === "evidence") return { verb: rest === "added" ? "added evidence" : rest === "updated" ? "updated evidence" : "removed evidence", title: p.spec?.title || "to a change", target: "cr:" + id };
  if (area === "review_thread") return { verb: "started a thread on", title: p.path ? pathText(p.path) : "a change", target: "cr:" + (p.changeRequestId || id) };
  if (area === "cr_comment") return { verb: "commented on a change:", title: str(p.body).slice(0, 80), target: "cr:" + (p.subject?.id || id) };
  if (/(code_scanning|secret_scanning|dependabot|dependency)_alert$/.test(area)) return { verb: words(area.replace(/_alert$/, "")) + " alert " + rest, title: p.rule?.name || p.advisory?.summary || p.secretTypeDisplayName || "#" + id, target: "alert:" + id };
  if (area === "package") return { verb: "package " + rest, title: str(p.package) || p.name || id, target: "package:" + (str(p.package) || id) };
  if (area === "pages") return { verb: "pages " + rest, title: typeof p.repo === "string" ? p.repo : "", target: "pages:" };
  if (k === "push_protection.blocked") return { verb: "push blocked: secret in", title: list(p.findings).map((f) => str(f.path)).filter(Boolean).join(", ") || "a pushed file", target: "alert:" };
  if (k === "dependency_graph.updated") return { verb: "dependency graph updated:", title: String(p.manifests ?? 0) + " manifests, " + String(p.packages ?? 0) + " packages", target: "" };
  const title = p.title || p.tagName || p.advisory?.summary || p.rule?.name || p.secretTypeDisplayName || p.name || p.displayTitle || id;
  const prefix = NOTE_PREFIX[kind] ?? "subject";
  return { verb: rest || words(area), title, target: id ? prefix + ":" + (kind === "SUBJECT_KIND_RELEASE" ? (p.tagName ?? id) : id) : "" };
}

export function eventView(e: EventJson): T.Event {
  const note = noteOf(e);
  return { ...(e as unknown as T.Event), ...(note ? { note } : {}) };
}

const RETRYABLE = new Set(["unavailable", "data_loss", "deadline_exceeded", "unknown", "internal"]);

/**
 * Subscribe that survives dropped connections: it reconnects from the last `seq` it saw, so the
 * caller gets no gaps and no duplicates. Gives up after five failed attempts in a row.
 */
async function* subscribe(rpc: Rpc, req: T.SubscribeRequest, opts?: CallOptions): AsyncGenerator<T.Event> {
  let last = BigInt(req.fromSeq || "0");
  let failures = 0;
  for (;;) {
    try {
      for await (const e of rpc.stream(EventService.method.subscribe, subscribeReq({ ...req, fromSeq: String(last) }), opts)) {
        const seq = BigInt(e.seq ?? "0");
        if (seq <= last) continue;
        last = seq;
        failures = 0;
        yield eventView(e);
      }
      return;
    } catch (err) {
      if (opts?.signal?.aborted) return;
      if (!(err instanceof BeeError) || !RETRYABLE.has(err.code) || ++failures >= 5) throw err;
      await new Promise((r) => setTimeout(r, 250 * 2 ** failures));
      if (opts?.signal?.aborted) return;
    }
  }
}

/** The repo's log, or the org's when the scope is every repo (the same rule Subscribe follows). */
function logScope(repo: T.RepoRef | undefined): { repo: T.RepoRef } | { org: string } {
  const org = orgWildcard(repo);
  return org ? { org } : { repo: repo as T.RepoRef };
}

function subscribeReq(req: T.SubscribeRequest) {
  const org = orgWildcard(req.repo);
  return clean(org ? { org, fromSeq: req.fromSeq, kinds: req.kinds } : { repo: req.repo, fromSeq: req.fromSeq, kinds: req.kinds });
}

// ---------------------------------------------------------------- ops

export const forgeOps = {
  OrgService: {
    GetOrg: pass(OrgService.method.getOrg),
    ListOrgs: pass(OrgService.method.listOrgs),
    ListMembers: pass(OrgService.method.listMembers),
    SetMember: pass(OrgService.method.setMember),
    CreateOrgInvitation: pass(OrgService.method.createOrgInvitation),
  },
  RepoService: {
    GetRepo: pass(RepoService.method.getRepo),
    UpdateRepo: pass(RepoService.method.updateRepo),
    ListCollaborators: pass(RepoService.method.listCollaborators),
    ListDeployKeys: {
      wire: (r) => wire(RepoService.method.listDeployKeys, r),
      run: async (rpc, r, opts) => {
        const result = await rpc.call(RepoService.method.listDeployKeys, r, opts);
        return {
          keys: list(result.keys).map((key) => ({
            id: str(key.id),
            title: str(key.title),
            readOnly: !!key.readOnly,
            principalId: str(key.principalId),
          })),
        };
      },
    },
    ListRepos: pass(RepoService.method.listRepos),
    ForkRepo: pass(RepoService.method.forkRepo),
    ListForks: pass(RepoService.method.listForks),
    SyncFork: pass(RepoService.method.syncFork),
    GetContext: {
      wire: (r) => wire(RepoService.method.getContext, r),
      run: async (rpc, r, opts) => {
        const ctx = (await rpc.call(RepoService.method.getContext, clean(r), opts)) as unknown as T.RepoContext;
        return { ...ctx, activeLeases: await withHolders(rpc, ctx.activeLeases, opts) };
      },
    } satisfies UnaryOp<T.GetContextRequest, T.RepoContext>,
  },
  ChangeService: {
    OpenChange: pass(ChangeService.method.openChange),
    GetChange: pass(ChangeService.method.getChange),
    GetChangeByNumber: pass(ChangeService.method.getChangeByNumber),
    ListChanges: pass(ChangeService.method.listChanges),
    CloseChange: pass(ChangeService.method.closeChange),
    SubmitReview: pass(ChangeService.method.submitReview),
    ListReviews: pass(ChangeService.method.listReviews),
    EvaluateLanding: pass(ChangeService.method.evaluateLanding),
    GetChangeDiff: {
      wire: (r) =>
        r.sinceRevision
          ? wire(ChangeService.method.diffChangeRevisions, { repo: r.repo, changeRequestId: r.changeRequestId, fromRevision: r.sinceRevision, toRevision: r.revision, includeHunks: true })
          : wire(QueryService.method.diff, { repo: r.repo, from: { expr: r.base ? objectIdText(r.base) : "<base of r" + r.revision + ">" }, to: { expr: r.head ? objectIdText(r.head) : "<head of r" + r.revision + ">" }, includeHunks: true, classify: true }),
      run: async (rpc, r, opts) => {
        if (r.sinceRevision) {
          const items = await collect(rpc.stream(ChangeService.method.diffChangeRevisions, clean({ repo: r.repo, changeRequestId: r.changeRequestId, fromRevision: r.sinceRevision, toRevision: r.revision, includeHunks: true }), opts));
          return { items: items.map(diffLines) as unknown as T.DiffItem[] };
        }
        let { base, head } = r;
        if (!base || !head) {
          const cr = await rpc.call(ChangeService.method.getChange, { repo: r.repo, id: r.changeRequestId }, opts);
          const revs = list(cr.revisions);
          const rev = revs.find((x) => num(x.n) === (r.revision || revs.length));
          if (!rev) throw new BeeError("not_found", { reason: "NOT_FOUND", message: "no revision r" + r.revision });
          base = rev.base as T.ObjectId | undefined;
          head = rev.head as T.ObjectId | undefined;
        }
        const items = await collect(rpc.stream(QueryService.method.diff, { repo: r.repo, from: { expr: objectIdText(base) }, to: { expr: objectIdText(head) }, includeHunks: true, classify: true }, opts));
        return { items: items.map(diffLines) as unknown as T.DiffItem[] };
      },
    } satisfies UnaryOp<T.GetChangeDiffRequest, T.GetChangeDiffResponse>,
    ListThreads: {
      wire: (r) => wire(ChangeService.method.listReviewThreads, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(ChangeService.method.listReviewThreads, { repo: r.repo, changeRequestId: r.changeRequestId }, opts);
        return { threads: list(res.threads).map(threadView) };
      },
    } satisfies UnaryOp<T.ListThreadsRequest, T.ListThreadsResponse>,
    CreateThread: {
      wire: (r) => wire(ChangeService.method.createChangeComment, createCommentReq(r)),
      run: async (rpc, r, opts) => threadComment(await rpc.call(ChangeService.method.createChangeComment, createCommentReq(r), opts)),
    } satisfies UnaryOp<T.CreateThreadRequest, T.ThreadComment>,
    ReplyThread: {
      wire: (r) => wire(ChangeService.method.replyToReviewThread, r),
      run: async (rpc, r, opts) => threadComment(await rpc.call(ChangeService.method.replyToReviewThread, { repo: r.repo, threadId: r.threadId, body: r.body }, opts)),
    } satisfies UnaryOp<T.ReplyThreadRequest, T.ThreadComment>,
    ResolveThread: {
      wire: (r) => wire(r.resolved ? ChangeService.method.resolveReviewThread : ChangeService.method.unresolveReviewThread, { repo: r.repo, threadId: r.threadId }),
      run: async (rpc, r, opts) => {
        const req = { repo: r.repo, threadId: r.threadId };
        return threadView(r.resolved ? await rpc.call(ChangeService.method.resolveReviewThread, req, opts) : await rpc.call(ChangeService.method.unresolveReviewThread, req, opts));
      },
    } satisfies UnaryOp<T.ResolveThreadRequest, T.ReviewThread>,
    ApplySuggestions: pass(ChangeService.method.applySuggestions, (r: T.ApplySuggestionsRequest) => ({ repo: r.repo, changeRequestId: r.changeRequestId, commentIds: r.commentIds })),
    ListQueue: {
      wire: (r) => wire(ChangeService.method.getMergeQueue, r),
      run: async (rpc, r, opts) => {
        const q = await rpc.call(ChangeService.method.getMergeQueue, { repo: r.repo, ref: r.ref }, opts);
        return { entries: list(q.entries).map((e) => queueView(e, str(e.title))), landsPerMinute: num(q.landsPerMinute) };
      },
    } satisfies UnaryOp<T.ListQueueRequest, T.ListQueueResponse>,
    Enqueue: {
      wire: (r) => wire(ChangeService.method.enqueueChange, { repo: r.repo, id: r.changeRequestId }),
      run: async (rpc, r, opts) => {
        const e = await rpc.call(ChangeService.method.enqueueChange, { repo: r.repo, id: r.changeRequestId }, opts);
        return queueView(e, str(e.title));
      },
    } satisfies UnaryOp<T.EnqueueRequest, T.QueueEntry>,
  },
  CheckService: {
    ListChecks: pass(CheckService.method.listChecks),
  },
  LeaseService: {
    AcquireLease: pass(LeaseService.method.acquireLease),
    ReleaseLease: pass(LeaseService.method.releaseLease),
    ListLeases: {
      wire: (r) => {
        const org = orgWildcard(r.repo);
        return wire(LeaseService.method.listLeases, org ? { org, paths: r.paths } : r);
      },
      run: async (rpc, r, opts) => ({ leases: await withHolders(rpc, await leasesOf(rpc, r, opts), opts) }),
    } satisfies UnaryOp<T.ListLeasesRequest, T.ListLeasesResponse>,
  },
  EventService: {
    Subscribe: {
      wire: (r) => wire(EventService.method.subscribe, subscribeReq(r)),
      run: (rpc, r, opts) => subscribe(rpc, r, opts),
    } satisfies StreamOp<T.SubscribeRequest, T.Event>,
    ListEvents: {
      wire: (r) => wire(EventService.method.listEvents, clean({ ...logScope(r.repo), fromSeq: r.fromSeq, kinds: r.kinds, limit: r.limit })),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(EventService.method.listEvents, clean({ ...logScope(r.repo), fromSeq: r.fromSeq, kinds: r.kinds, limit: r.limit }), opts);
        return { events: list(res.events).map(eventView), nextSeq: String(res.nextSeq ?? "0") };
      },
    } satisfies UnaryOp<T.ListEventsRequest, T.ListEventsResponse>,
    GetEventLogInfo: {
      wire: (r) => wire(EventService.method.getEventLogInfo, clean(logScope(r.repo))),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(EventService.method.getEventLogInfo, clean(logScope(r.repo)), opts);
        return { oldestSeq: String(res.oldestSeq ?? "0"), latestSeq: String(res.latestSeq ?? "0"), retentionDays: Number(res.retentionDays ?? 0) };
      },
    } satisfies UnaryOp<T.GetEventLogInfoRequest, T.EventLogInfo>,
  },
} satisfies PartialOps<"OrgService" | "RepoService" | "ChangeService" | "CheckService" | "LeaseService" | "EventService">;

async function leasesOf(rpc: Rpc, r: T.ListLeasesRequest, opts?: CallOptions): Promise<T.Lease[]> {
  const org = orgWildcard(r.repo);
  if (!org) return list((await rpc.call(LeaseService.method.listLeases, clean({ repo: r.repo, paths: r.paths }), opts)).leases) as unknown as T.Lease[];
  try {
    return list((await rpc.call(LeaseService.method.listLeases, clean({ org, paths: r.paths }), opts)).leases) as unknown as T.Lease[];
  } catch (err) {
    // A server without ListLeasesRequest.org (G10) insists on `repo`: ask each repo instead.
    if (!(err instanceof BeeError) || err.code !== "invalid_argument") throw err;
  }
  // The organisation roster needs leases from every repository page on older servers too.
  const repos = [];
  let pageToken = "";
  do {
    const response = await rpc.call(RepoService.method.listRepos, { org, page: { pageSize: 200, pageToken } }, opts);
    repos.push(...list(response.repos));
    pageToken = str(response.page?.nextPageToken);
  } while (pageToken);
  const per = await mapLimit(repos, 8, (repo) => rpc.call(LeaseService.method.listLeases, clean({ repo: { fullName: repo.fullName }, paths: r.paths }), opts));
  return per.flatMap((x) => list(x.leases)) as unknown as T.Lease[];
}

/** Lease holders may arrive as bare ids; name them (AuthService.GetPrincipal) for the screens. */
export async function withHolders(rpc: Rpc, leases: T.Lease[], opts?: CallOptions): Promise<T.Lease[]> {
  const bare = leases.filter((l) => l.holder?.id && !l.holder.display).map((l) => l.holder?.id ?? "");
  if (!bare.length) return leases;
  const names = await principals(rpc, bare, opts);
  return leases.map((l) => (l.holder && !l.holder.display && names.has(l.holder.id) ? { ...l, holder: names.get(l.holder.id) } : l));
}

function createCommentReq(r: T.CreateThreadRequest) {
  return clean({
    repo: r.repo,
    changeRequestId: r.changeRequestId,
    body: r.body,
    path: r.path,
    side: r.side,
    startLine: r.startLine,
    endLine: r.endLine || r.startLine,
    revision: r.revision,
    suggestion: r.suggestion ? { replacement: r.suggestion } : undefined,
  });
}
