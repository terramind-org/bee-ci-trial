/**
 * The status sentence (docs/spec/web-ui.md §3): one line that says where a change request stands
 * and why, e.g. "Ready to land: 2 approvals, all 14 checks pass". The Brief tab shows it under the
 * outcome and the review queue shows it on every row. A pure function of what the page loaded.
 */
import type { ChangeRequest, CheckRun, LandingEvaluation, QueueEntry, Review } from "./api/types";
import { checksClause, countWord, listing, tallyChecks } from "./checkwords";

export type StatusKind = "landed" | "closed" | "queued" | "ready" | "failing" | "checking" | "review" | "blocked";
export type StatusTone = "ok" | "warn" | "bad" | "neutral" | "accent";

export interface Status {
  kind: StatusKind;
  tone: StatusTone;
  /** "Ready to land". */
  headline: string;
  /** "2 approvals, all 14 checks pass"; may be empty. */
  detail: string;
  /** headline + ": " + detail, or the headline alone. */
  sentence: string;
}

/** A parsed `approval:<from>[:<scope>] (have of need)` entry from EvaluateLanding.missing. */
export interface ApprovalNeed {
  from: "human" | "any" | "owner";
  scope: string;
  have: number;
  need: number;
}

export function parseApproval(entry: string): ApprovalNeed | null {
  const m = /^approval:(human|any|owner)(?::([^ ]+))? \((\d+) of (\d+)\)$/.exec(entry.trim());
  if (!m) return null;
  return { from: m[1] as ApprovalNeed["from"], scope: m[2] ?? "", have: Number(m[3]), need: Number(m[4]) };
}

/** A required check that EvaluateLanding still waits for: `check:<name> (running|queued|missing)`. */
export function pendingCheck(entry: string): string | null {
  return /^check:(\S+) \((?:running|queued|missing)\)$/.exec(entry.trim())?.[1] ?? null;
}

/**
 * One missing landing rule as a predicate, so a list reads after "it": "needs one more approval
 * from a person", "failed the test check", "waits for “Add the cache” to merge first". Unknown
 * entries are returned as they are, so nothing is ever hidden.
 */
export function missingInWords(entry: string): string {
  const a = parseApproval(entry);
  if (a) {
    const left = Math.max(a.need - a.have, 1);
    const more = a.have > 0 ? " more" : "";
    const what = left === 1 ? "approval" : "approvals";
    const who = a.from === "human" ? " from a person" : a.from === "owner" ? " from an owner of " + a.scope : "";
    return "needs " + countWord(left) + more + " " + what + who;
  }
  // B10: the server also reports a bare `approval:<from>[:<scope>]` (no counts); say it in words too.
  const bare = /^approval:(human|any|owner)(?::(\S+))?$/.exec(entry.trim());
  if (bare) return "needs an approval" + (bare[1] === "human" ? " from a person" : bare[1] === "owner" ? " from an owner of " + (bare[2] ?? "the changed paths") : "");
  const check = /^check:(\S+) \((failed|running|queued|missing)\)$/.exec(entry.trim());
  if (check) return check[2] === "failed" ? "failed the " + check[1] + " check" : check[2] === "missing" ? "has not run the " + check[1] + " check yet" : "is waiting for the " + check[1] + " check";
  const lease = /^lease:exclusive:(.+)$/.exec(entry);
  if (lease) return "needs exclusive editing of " + lease[1];
  const stack = /^stack: parent (.+) must land first$/.exec(entry);
  if (stack) return "waits for “" + stack[1] + "” to merge first";
  if (entry.startsWith("state:")) return "is " + entry.slice(6);
  return entry;
}

export interface StatusInput {
  change: Pick<ChangeRequest, "state" | "revisions">;
  landing?: LandingEvaluation;
  checks: CheckRun[];
  reviews: Review[];
  /** The change's merge-queue entry, when it has one. */
  queue?: Pick<QueueEntry, "position" | "state"> | null;
}

function approvalsOnLatest(reviews: Review[], latest: number): number {
  const by = new Map<string, Review>();
  for (const r of reviews) {
    const who = r.reviewer?.id ?? r.id;
    const prev = by.get(who);
    if (!prev || r.revision >= prev.revision) by.set(who, r);
  }
  return [...by.values()].filter((r) => r.verdict === "VERDICT_APPROVE" && r.revision === latest).length;
}

function changesRequested(reviews: Review[], latest: number): boolean {
  const by = new Map<string, Review>();
  for (const r of reviews) {
    if (r.verdict === "VERDICT_COMMENT") continue;
    const who = r.reviewer?.id ?? r.id;
    const prev = by.get(who);
    if (!prev || r.revision >= prev.revision) by.set(who, r);
  }
  return [...by.values()].some((r) => r.verdict === "VERDICT_REQUEST_CHANGES" && r.revision === latest);
}

function make(kind: StatusKind, tone: StatusTone, headline: string, parts: string[]): Status {
  const detail = parts.filter(Boolean).join(", ");
  return { kind, tone, headline, detail, sentence: detail ? headline + ": " + detail : headline };
}

export function statusOf({ change, landing, checks, reviews, queue }: StatusInput): Status {
  const latest = change.revisions.at(-1)?.n ?? 0;
  if (change.state === "CHANGE_STATE_LANDED") return make("landed", "ok", "Merged", []);
  if (change.state === "CHANGE_STATE_CLOSED") return make("closed", "neutral", "Closed without merging", []);

  const approvals = approvalsOnLatest(reviews, latest);
  const approvalText = approvals ? approvals + (approvals === 1 ? " approval" : " approvals") : "";
  const tally = tallyChecks(checks);
  const checksText = checksClause(checks);

  if (queue) {
    const where = queue.state === "landing" ? "merging now" : queue.state === "testing" ? "testing at position " + queue.position : "position " + queue.position;
    return make("queued", "accent", "In the merge queue", [where]);
  }

  const missing = landing?.missing ?? [];
  const failedRequired = missing.filter((m) => /^check:\S+ \(failed\)$/.test(m));
  if (tally.failed.length || failedRequired.length) return make("failing", "bad", "Checks failing", [checksText]);

  const pendingRequired = missing.map(pendingCheck).filter((x): x is string => !!x);
  if (pendingRequired.length || tally.pending.length) {
    const waiting = pendingRequired.length && !tally.pending.length ? "waiting for " + listing(pendingRequired.map((n) => "the " + n + " check")) : checksText;
    return make("checking", "neutral", "Checks running", [waiting]);
  }

  if (landing?.allowed) return make("ready", "ok", "Ready to land", [approvalText, checksText]);

  const approvalsMissing = missing.filter((m) => parseApproval(m));
  const rules = missing.filter((m) => !parseApproval(m) && !m.startsWith("check:"));
  if (changesRequested(reviews, latest)) return make("review", "warn", "Changes requested", [listing(approvalsMissing.map(missingInWords)), checksText]);
  if (approvalsMissing.length && !rules.length) return make("review", "warn", "Needs review", [listing(approvalsMissing.map(missingInWords)), checksText]);
  if (!landing) return make("review", "neutral", "Open", [approvalText, checksText]);
  const why = [...rules, ...approvalsMissing].map(missingInWords);
  return make("blocked", "bad", "Blocked", why.length ? ["it " + listing(why)] : [checksText]);
}
