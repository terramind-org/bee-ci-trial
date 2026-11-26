/**
 * Small, fixed sample objects for the brief libraries' tests (status, queue, team, changemd, …).
 * Not imported by any page. Every timestamp is a literal, so tests never depend on the clock.
 */
import type { BriefChecklist, ChangeBrief, ChangeRequest, CheckRun, CheckStatus, Comment, DiffItem, Evidence, LandingEvaluation, Principal, Review, Verdict } from "./api/types";
import { pathBytes } from "./api/ids";

export const AGENT: Principal = { id: "p-agent", kind: "PRINCIPAL_KIND_AGENT", display: "triage", ownerId: "p-mara", runtime: "claude-code", externalIds: [] };
export const MARA: Principal = { id: "p-mara", kind: "PRINCIPAL_KIND_HUMAN", display: "Mara Okafor", ownerId: "", runtime: "", externalIds: [] };
export const LIN: Principal = { id: "p-lin", kind: "PRINCIPAL_KIND_HUMAN", display: "Lin Chen", ownerId: "", runtime: "", externalIds: [] };

export function brief(over: Partial<ChangeBrief> = {}): ChangeBrief {
  return { outcome: "", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "", ...over };
}

export function change(over: Partial<ChangeRequest> = {}): ChangeRequest {
  return {
    id: "cr-1",
    repoId: "r-1",
    targetRef: "main",
    changeIds: [],
    state: "CHANGE_STATE_OPEN",
    author: AGENT,
    title: "Rotate session tokens on role change",
    body: "",
    revisions: [
      { n: 1, diffClass: "logic", createdAt: "2026-10-27T10:00:00Z" },
      { n: 2, diffClass: "sensitive", createdAt: "2026-10-27T12:00:00Z" },
    ],
    dependsOn: "",
    createdAt: "2026-10-27T09:00:00Z",
    number: "12",
    ...over,
  };
}

export function check(name: string, status: CheckStatus, over: Partial<CheckRun> = {}): CheckRun {
  return { id: "ck-" + name, repoId: "r-1", name, status, url: "", summary: "", postLand: false, ...over };
}

export function review(reviewer: Principal, verdict: Verdict, revision: number, createdAt = "2026-10-27T13:00:00Z", body = ""): Review {
  return { id: "rv-" + reviewer.id + "-" + revision + "-" + verdict, changeId: "cr-1", revision, reviewer, verdict, body, comments: [], createdAt };
}

export function comment(author: Principal, body: string, createdAt: string): Comment {
  return { id: "c-" + createdAt, author, body, createdAt, edited: false };
}

export function landing(allowed: boolean, missing: string[] = [], satisfied: string[] = []): LandingEvaluation {
  return { allowed, missing, satisfied, diffClass: "logic" };
}

export function checklist(complete: boolean, items: BriefChecklist["items"] = []): BriefChecklist {
  return { revision: 2, complete, items };
}

export function diffItem(path: string, diffClass: string, lines: string[], kind: "modified" | "added" | "deleted" = "modified"): DiffItem {
  const side = { mode: "MODE_FILE" as const };
  const old = lines.filter((l) => l[0] !== "+").length;
  const neu = lines.filter((l) => l[0] !== "-").length;
  return {
    path: pathBytes(path),
    old: kind === "added" ? undefined : side,
    new: kind === "deleted" ? undefined : side,
    binary: false,
    diffClass,
    hunks: lines.length ? [{ oldStart: 1, oldLen: old, newStart: 1, newLen: neu, lines }] : [],
  };
}

export function evidence(id: string, over: Omit<Partial<Evidence>, "spec"> & { spec?: Partial<NonNullable<Evidence["spec"]>> } = {}): Evidence {
  const { spec, ...rest } = over;
  return {
    id,
    repoId: "r-1",
    changeRequestId: "cr-1",
    revision: 2,
    assets: [],
    source: "EVIDENCE_SOURCE_UPLOADED",
    checkRunId: "",
    workflowArtifactId: "",
    uploader: AGENT,
    state: "EVIDENCE_STATE_READY",
    stale: false,
    createdAt: "2026-10-27T12:30:00Z",
    ...rest,
    spec: { kind: "EVIDENCE_KIND_SCREENSHOT", title: "", caption: "", benchmark: [], url: "", paths: [], hero: false, order: 0, ...spec },
  };
}

export function image(role: "MAIN" | "BEFORE" | "AFTER" | "POSTER", url: string, alt: string): Evidence["assets"][number] {
  return { role: ("EVIDENCE_ASSET_ROLE_" + role) as Evidence["assets"][number]["role"], contentType: "image/png", digest: "sha256:" + "a".repeat(64), sizeBytes: "100", width: 640, height: 400, durationMs: "0", alt, url };
}
