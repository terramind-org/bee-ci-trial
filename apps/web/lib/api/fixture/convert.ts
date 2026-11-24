/**
 * Store records as bee.v1 messages (proto3 JSON). The store keeps the shapes it was built
 * with; these functions are the only place that knows how they map onto the wire.
 */
import type { AuditEntryJson } from "../../gen/bee/v1/audit_pb";
import type { EventJson, MergeQueueEntryJson, ReviewCommentJson, ReviewThreadJson } from "../../gen/bee/v1/forge_pb";
import type { IssueJson } from "../../gen/bee/v1/issues_pb";
import type { ReleaseJson } from "../../gen/bee/v1/releases_pb";
import type { CodeScanningAlertJson, DependencyAlertJson, SecretScanningAlertJson, SecuritySeverityJson } from "../../gen/bee/v1/security_pb";
import type { WorkflowConclusionJson, WorkflowJobJson, WorkflowRunJson, WorkflowStatusJson, WorkflowStepJson } from "../../gen/bee/v1/workflows_pb";
import { bytesToBase64, fromHex, pathBytes } from "../ids";
import type * as T from "../types";
import { objectId } from "./seed";
import type { FixtureStore, IssueRow } from "./store";

// ---------------------------------------------------------------- review threads and queue

export function reviewComment(t: T.ReviewThread, c: T.ThreadComment): ReviewCommentJson {
  return {
    id: c.id,
    path: t.path,
    side: t.side,
    startLine: t.startLine,
    endLine: t.endLine,
    body: c.body,
    revision: t.revision,
    threadId: t.id,
    suggestion: c.suggestion ? { replacement: c.suggestion } : undefined,
    outdated: t.outdated,
    author: c.author,
    createdAt: c.createdAt,
  };
}

export function reviewThread(t: T.ReviewThread): ReviewThreadJson {
  return {
    id: t.id,
    changeRequestId: t.changeRequestId,
    path: t.path,
    side: t.side,
    startLine: t.startLine,
    endLine: t.endLine,
    revision: t.revision,
    resolved: t.resolved,
    outdated: t.outdated,
    comments: t.comments.map((c) => reviewComment(t, c)),
  };
}

const QUEUE_STATE = { waiting: "MERGE_QUEUE_STATE_QUEUED", testing: "MERGE_QUEUE_STATE_CHECKING", landing: "MERGE_QUEUE_STATE_LANDING" } as const;

export function queueEntry(s: FixtureStore, q: { crId: string; state: T.QueueEntry["state"]; enqueuedAt: number; train: string }, index: number): MergeQueueEntryJson {
  const c = s.crs.find((x) => x.id === q.crId);
  return {
    changeRequestId: q.crId,
    changeNumber: String(c?.number ?? 0),
    title: c?.title ?? "",
    ref: "main",
    position: index + 1,
    state: QUEUE_STATE[q.state],
    enqueuedAt: s.iso(q.enqueuedAt),
    // Entries checked together share one speculative head (the fixture's "train").
    speculativeHead: objectId("changeset", q.train),
  };
}

// ---------------------------------------------------------------- runs

export function workflowState(s: T.RunStatus): { status: WorkflowStatusJson; conclusion?: WorkflowConclusionJson } {
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
  }
}

export function workflowRun(s: FixtureStore, r: T.Run): WorkflowRunJson {
  const cr = r.changeRequestId ? s.crs.find((c) => c.id === r.changeRequestId) : undefined;
  return {
    id: r.id,
    repo: { id: r.repoId },
    workflowId: "wf_" + r.workflow,
    workflowPath: ".bee/workflows/" + r.workflow + ".yml",
    name: r.workflow,
    displayTitle: r.workflow + " #" + r.number,
    runNumber: String(r.number),
    runAttempt: r.attempt,
    event: r.trigger,
    ...workflowState(r.status),
    headRef: r.ref,
    headChangeset: r.head,
    changeRequestNumbers: cr ? [String(cr.number)] : [],
    changeRequestIds: cr ? [cr.id] : [],
    actor: r.actor,
    createdAt: r.startedAt,
    runStartedAt: r.startedAt,
    completedAt: r.finishedAt,
  };
}

export function workflowJob(s: FixtureStore, r: T.Run, j: T.RunJob): WorkflowJobJson {
  const check = [...s.checks.values()].flat().find((ch) => ch.url.includes(j.id));
  let t = j.startedAt ? Date.parse(j.startedAt) : NaN;
  const steps: WorkflowStepJson[] = j.steps.map((st, i) => {
    const d = Number(st.durationMs);
    const timed = Number.isFinite(t) && d > 0;
    const step: WorkflowStepJson = { number: i + 1, name: st.name, ...workflowState(st.status), startedAt: timed ? s.iso(t) : undefined, completedAt: timed ? s.iso(t + d) : undefined };
    if (timed) t += d;
    return step;
  });
  return {
    id: j.id,
    runId: r.id,
    runAttempt: r.attempt,
    name: j.name,
    workflowName: r.workflow,
    ...workflowState(j.status),
    steps,
    runnerName: j.runner,
    checkRunId: check?.id ?? "",
    headRef: r.ref,
    headChangeset: r.head,
    startedAt: j.startedAt,
    completedAt: j.finishedAt,
  };
}

/** A job's log as bytes. Levels travel as GitHub-style `##[group]` markers. */
export function logBytes(lines: { text: string; level: T.LogLine["level"] }[]): Uint8Array {
  return new TextEncoder().encode(lines.map((l) => (l.level === "info" ? "" : "##[" + l.level + "]") + l.text + "\n").join(""));
}

// ---------------------------------------------------------------- issues

export function issueJson(s: FixtureStore, i: IssueRow, viewer?: string): IssueJson {
  const inspiredBy = s.posts.refFor(s, i.inspiredByPostId, viewer);
  const repo = [...s.repos.values()].find((r) => r.repo.id === i.repoId);
  const subs = s.issues.filter((x) => x.repoId === i.repoId && x.parentNumber === i.number);
  const done = subs.filter((x) => x.state === "closed").length;
  return {
    id: i.id,
    repoId: i.repoId,
    repoFullName: repo?.repo.fullName ?? "",
    number: String(i.number),
    title: i.title,
    body: i.body,
    state: i.state === "closed" ? "ISSUE_STATE_CLOSED" : "ISSUE_STATE_OPEN",
    author: i.author,
    assignees: i.assignees,
    labels: i.labels.map((name) => ({ id: "label_" + name, name })),
    milestone: i.milestone ? { id: "ms_" + i.milestone.replace(/\W+/g, "_").toLowerCase(), title: i.milestone, state: "MILESTONE_STATE_OPEN" } : undefined,
    issueType: i.type ? { id: "type_" + i.type, name: i.type.charAt(0).toUpperCase() + i.type.slice(1) } : undefined,
    parent: i.parentNumber ? { number: String(i.parentNumber), title: s.issues.find((x) => x.repoId === i.repoId && x.number === i.parentNumber)?.title ?? "" } : undefined,
    subIssues: subs.length ? { total: subs.length, completed: done, percentCompleted: Math.round((done * 100) / subs.length) } : undefined,
    linkedChangeRequestIds: i.linkedChanges,
    commentCount: i.comments.length,
    createdAt: i.createdAt,
    ...(inspiredBy ? { inspiredBy } : {}),
  };
}

// ---------------------------------------------------------------- releases

export function releaseJson(r: T.Release): ReleaseJson {
  return {
    id: r.id,
    repo: { id: r.repoId },
    tagName: r.tag,
    target: r.target,
    targetRef: "main",
    name: r.name,
    body: r.notes,
    prerelease: r.prerelease,
    immutable: r.immutable,
    author: r.author,
    createdAt: r.createdAt,
    publishedAt: r.createdAt,
    assets: r.assets.map((a, i) => ({ id: r.id + ":" + i, releaseId: r.id, name: a.name, size: a.size, downloadCount: String(a.downloads), state: "RELEASE_ASSET_STATE_UPLOADED" })),
  };
}

// ---------------------------------------------------------------- security

const SEVERITY: Record<T.SecurityAlert["severity"], SecuritySeverityJson> = {
  critical: "SECURITY_SEVERITY_CRITICAL",
  high: "SECURITY_SEVERITY_HIGH",
  medium: "SECURITY_SEVERITY_MEDIUM",
  low: "SECURITY_SEVERITY_LOW",
};

/** Alert numbers are per kind, in the order the store holds them. */
export function alertNumber(s: FixtureStore, a: T.SecurityAlert): number {
  return s.alerts.filter((x) => x.kind === a.kind).indexOf(a) + 1;
}

export function dependencyAlert(s: FixtureStore, a: T.SecurityAlert): DependencyAlertJson {
  const [manifest = "", pkg = ""] = a.location.split(" · ");
  const [name = "", version = ""] = pkg.split(" ");
  return {
    number: String(alertNumber(s, a)),
    repo: { id: a.repoId },
    state: a.state === "open" ? "DEPENDENCY_ALERT_STATE_OPEN" : a.state === "fixed" ? "DEPENDENCY_ALERT_STATE_FIXED" : "DEPENDENCY_ALERT_STATE_DISMISSED",
    dependency: { ecosystem: "npm", name, version, manifestPath: manifest, packageUrl: "pkg:npm/" + name + "@" + version },
    advisory: { ghsaId: a.advisory, summary: a.title, description: a.detail, severity: SEVERITY[a.severity] },
    severity: SEVERITY[a.severity],
    dismissedComment: a.dismissedReason,
    dismissedReason: a.state === "dismissed" ? "DEPENDENCY_ALERT_DISMISS_REASON_TOLERABLE_RISK" : undefined,
    updateChangeRequest: a.fixChangeRequestId,
    createdAt: a.createdAt,
  };
}

/** The store's secret titles read "<type> in <path>"; the path is a location on the wire. */
export function secretParts(a: T.SecurityAlert): { display: string; path: string; line: number } {
  const [path = "", line = "0"] = a.location.split(":");
  const display = a.title.endsWith(" in " + path) ? a.title.slice(0, -(" in " + path).length) : a.title;
  return { display, path, line: Number(line) || 0 };
}

export function secretAlert(s: FixtureStore, a: T.SecurityAlert): SecretScanningAlertJson {
  const { display } = secretParts(a);
  const type = a.advisory.replace(/^secret:/, "");
  return {
    number: String(alertNumber(s, a)),
    repo: { id: a.repoId },
    state: a.state === "open" ? "SECRET_SCANNING_ALERT_STATE_OPEN" : "SECRET_SCANNING_ALERT_STATE_RESOLVED",
    secretType: type,
    secretTypeDisplayName: display,
    secretPreview: type === "aws_access_key" ? "AKIA…EXAMPLE" : "",
    validity: a.severity === "critical" ? "SECRET_SCANNING_VALIDITY_ACTIVE" : "SECRET_SCANNING_VALIDITY_INACTIVE",
    severity: SEVERITY[a.severity],
    resolution: a.state === "fixed" ? "SECRET_SCANNING_RESOLUTION_REVOKED" : a.state === "dismissed" ? "SECRET_SCANNING_RESOLUTION_FALSE_POSITIVE" : undefined,
    resolutionComment: a.dismissedReason,
    createdAt: a.createdAt,
  };
}

export function codeAlert(s: FixtureStore, a: T.SecurityAlert): CodeScanningAlertJson {
  const [path = "", line = "0"] = a.location.split(":");
  return {
    number: String(alertNumber(s, a)),
    repo: { id: a.repoId },
    state: a.state === "open" ? "CODE_SCANNING_ALERT_STATE_OPEN" : a.state === "fixed" ? "CODE_SCANNING_ALERT_STATE_FIXED" : "CODE_SCANNING_ALERT_STATE_DISMISSED",
    rule: { id: "bee/" + a.title.toLowerCase().replace(/\W+/g, "-"), name: a.title, description: a.detail, securitySeverity: SEVERITY[a.severity], tags: [a.advisory] },
    toolName: "bee-scan",
    mostRecentInstance: { ref: "main", state: "CODE_SCANNING_ALERT_STATE_OPEN", message: a.detail, location: { path: pathBytes(path), startLine: Number(line) || 0, endLine: Number(line) || 0 } },
    dismissedComment: a.dismissedReason,
    dismissedReason: a.state === "dismissed" ? "CODE_SCANNING_DISMISS_REASON_WONT_FIX" : undefined,
    fixChangeRequest: a.fixChangeRequestId,
    createdAt: a.createdAt,
  };
}

export function alertJson(s: FixtureStore, a: T.SecurityAlert): DependencyAlertJson | SecretScanningAlertJson | CodeScanningAlertJson {
  return a.kind === "dependency" ? dependencyAlert(s, a) : a.kind === "secret" ? secretAlert(s, a) : codeAlert(s, a);
}

// ---------------------------------------------------------------- audit

export function auditEntry(s: FixtureStore, e: T.AuditEntry): AuditEntryJson {
  const actor = e.actor;
  const chain = actor ? [actor.id, ...(actor.ownerId ? [actor.ownerId] : [])] : [];
  const [agent = "", ip = ""] = e.source.split(" · ");
  const human = actor?.kind === "PRINCIPAL_KIND_HUMAN";
  return {
    seq: e.seq,
    id: "audit_" + e.seq,
    prevHash: bytesToBase64(fromHex(e.prevHash)),
    hash: bytesToBase64(fromHex(e.hash)),
    time: e.time,
    actor,
    delegationChain: chain,
    onBehalfOf: chain[chain.length - 1] ?? "",
    action: e.action,
    repo: e.repo ? { fullName: e.repo } : undefined,
    org: s.org.name,
    userAgent: human ? agent : e.source,
    ip: human ? ip : "",
    result: e.allowed ? "ok" : e.rule.startsWith("push_protection") ? "SECRET_DETECTED" : "POLICY_DENIED",
    targetDisplay: e.target,
    policyRule: e.rule,
  };
}

// ---------------------------------------------------------------- events

const SUBJECT = { issue: "SUBJECT_KIND_ISSUE", run: "SUBJECT_KIND_WORKFLOW_RUN", release: "SUBJECT_KIND_RELEASE", cr: "SUBJECT_KIND_CHANGE_REQUEST", alert: "SUBJECT_KIND_SECURITY_ALERT" } as const;

/**
 * Store events carry a note (title, target) for kinds without a typed body. On the wire such
 * an event names its subject and carries the subject resource as proto3 JSON in payload_json.
 */
export function eventJson(s: FixtureStore, e: T.Event): EventJson {
  const { note, ...rest } = e;
  if (!note) return rest;
  const [kind = "", id = ""] = note.target.split(":");
  const repo = [...s.repos.values()].find((r) => r.repo.id === e.repoId);
  const subject = { kind: SUBJECT[kind as keyof typeof SUBJECT] ?? "SUBJECT_KIND_UNSPECIFIED", repo: repo ? { fullName: repo.repo.fullName } : undefined, id };
  let payload: unknown = {};
  if (kind === "issue") {
    const i = s.issues.find((x) => x.repoId === e.repoId && String(x.number) === id);
    if (i) payload = issueJson(s, i);
  } else if (kind === "run") {
    const r = s.runs.find((x) => x.id === id);
    if (r) payload = workflowRun(s, r);
  } else if (kind === "release") {
    const r = s.releases.find((x) => x.tag === id && x.repoId === e.repoId);
    if (r) {
      payload = releaseJson(r);
      subject.id = r.id;
    }
  } else if (kind === "cr") {
    const c = s.crs.find((x) => x.id === id);
    if (c) payload = { id: c.id, title: c.title, number: String(c.number) };
  } else if (kind === "evidence") {
    // evidence.*: the subject is the change request, the payload is the Evidence without signed URLs.
    const ev = s.briefs.byId(id);
    if (ev) {
      subject.kind = SUBJECT.cr;
      subject.id = ev.crId;
      payload = s.briefs.payload(s, ev);
    }
  } else if (kind === "alert") {
    const a = s.alerts.find((x) => x.id === id);
    if (a) {
      payload = alertJson(s, a);
      subject.id = a.kind + ":" + alertNumber(s, a);
    }
  }
  return { ...rest, subject, payloadJson: JSON.stringify(payload) };
}
