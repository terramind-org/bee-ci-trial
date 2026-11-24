/**
 * The review queue (docs/spec/web-ui.md §3, "Changes list"): open change requests grouped, in this
 * order, into Needs you, Ready to land, Waiting on agents and Blocked, plus Recently landed (the
 * last seven days). Each change appears once, in the first group that takes it. A pure function
 * of the changes, their landing results, checks, reviews, checklists and asks, the viewer, and
 * `now` (passed in, so the page and the tests agree).
 */
import type { BriefChecklist, ChangeRequest, CheckRun, Comment, LandingEvaluation, Principal, QueueEntry, Review } from "./api/types";
import { openAsks } from "./asks";
import { checkState } from "./checkwords";
import { parseApproval, pendingCheck, statusOf, type Status } from "./status";

export type QueueGroupKey = "needs-you" | "ready" | "waiting" | "blocked" | "landed";

export const QUEUE_GROUPS: { key: QueueGroupKey; label: string; empty: string }[] = [
  { key: "needs-you", label: "Needs you", empty: "Nothing is waiting on you." },
  { key: "ready", label: "Ready to land", empty: "Nothing is ready to land." },
  { key: "waiting", label: "Waiting on agents", empty: "No agent owes anything." },
  { key: "blocked", label: "Blocked", empty: "Nothing is blocked." },
  { key: "landed", label: "Recently landed", empty: "Nothing landed in the last seven days." },
];

export interface QueueFacts {
  change: ChangeRequest;
  landing?: LandingEvaluation;
  checks: CheckRun[];
  reviews: Review[];
  checklist?: BriefChecklist;
  /** The conversation, for unanswered asks. */
  comments?: Comment[];
  queue?: QueueEntry | null;
  /** When it landed; defaults to its latest revision's time. */
  landedAt?: string;
  /** Owners (display names) of each path an `approval:owner:<path>` rule names. */
  owners?: Record<string, string[]>;
}

export interface QueueRow {
  facts: QueueFacts;
  status: Status;
  /** Why it is in its group, in words: "You can approve it", "The agent still owes a screenshot". */
  why: string;
}

export interface QueueGroup {
  key: QueueGroupKey;
  label: string;
  empty: string;
  rows: QueueRow[];
}

const WEEK = 7 * 86_400_000;

function latestRevision(c: ChangeRequest): number {
  return c.revisions.at(-1)?.n ?? 0;
}

function lastActivity(f: QueueFacts): string {
  return f.landedAt ?? f.change.revisions.at(-1)?.createdAt ?? f.change.createdAt ?? "";
}

/** Why the viewer is needed, or null. */
function needsViewer(f: QueueFacts, viewer: Principal | null): string | null {
  if (!viewer) return null;
  const { change, landing } = f;
  const mine = change.author?.id === viewer.id;
  const asks = f.change.brief ? openAsks(f.change.brief.asks, f.comments ?? []) : [];
  if (asks.length && !mine) return asks.length === 1 ? "The author asks you to decide: " + asks[0]?.question : "The author asks " + asks.length + " questions";
  if (mine || !landing) return null;
  const latest = latestRevision(change);
  const approvedLatest = f.reviews.some((r) => r.reviewer?.id === viewer.id && r.verdict === "VERDICT_APPROVE" && r.revision === latest);
  if (approvedLatest) return null;
  const canApprove = landing.missing.some((m) => {
    const a = parseApproval(m);
    if (!a) return false;
    if (a.from === "owner") return (f.owners?.[a.scope] ?? []).includes(viewer.display);
    if (a.from === "human") return viewer.kind === "PRINCIPAL_KIND_HUMAN";
    return true;
  });
  return canApprove ? "Waiting for your review" : null;
}

function waitingOnAgent(f: QueueFacts): string | null {
  if (f.change.author?.kind !== "PRINCIPAL_KIND_AGENT") return null;
  const owed = f.checklist && !f.checklist.complete ? f.checklist.items.filter((i) => !i.satisfied) : [];
  if (owed.length) return "The agent still owes " + owed.map((i) => i.detail).join("; ");
  const running = f.checks.some((c) => ["running", "queued"].includes(checkState(c))) || (f.landing?.missing ?? []).some((m) => pendingCheck(m));
  return running ? "Checks are still running" : null;
}

export function groupQueue(items: QueueFacts[], viewer: Principal | null, now: number): QueueGroup[] {
  const groups = new Map<QueueGroupKey, QueueRow[]>(QUEUE_GROUPS.map((g) => [g.key, []]));
  for (const f of items) {
    const status = statusOf({ change: f.change, landing: f.landing, checks: f.checks, reviews: f.reviews, queue: f.queue });
    const put = (key: QueueGroupKey, why: string) => groups.get(key)?.push({ facts: f, status, why });
    if (f.change.state === "CHANGE_STATE_LANDED") {
      const at = Date.parse(lastActivity(f));
      if (Number.isFinite(at) && now - at <= WEEK) put("landed", "Merged");
      continue;
    }
    if (f.change.state !== "CHANGE_STATE_OPEN") continue;
    const need = needsViewer(f, viewer);
    if (need) {
      put("needs-you", need);
      continue;
    }
    if (f.landing?.allowed || f.queue) {
      put("ready", f.queue ? "In the merge queue" : "Policy allows landing");
      continue;
    }
    const waiting = waitingOnAgent(f);
    if (waiting) {
      put("waiting", waiting);
      continue;
    }
    put("blocked", status.sentence);
  }
  const newestFirst = (a: QueueRow, b: QueueRow) => lastActivity(b.facts).localeCompare(lastActivity(a.facts)) || (a.facts.change.id < b.facts.change.id ? -1 : a.facts.change.id > b.facts.change.id ? 1 : 0);
  return QUEUE_GROUPS.map((g) => ({ ...g, rows: (groups.get(g.key) ?? []).sort(newestFirst) }));
}
