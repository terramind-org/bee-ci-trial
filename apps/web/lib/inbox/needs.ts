/**
 * What needs the signed-in person: a pure classification of one open change request from the
 * facts the pages load (landing evaluation, checks, reviews, path owners, merge queue), plus
 * agent-written posts on merged change requests that wait for their approval. Only work this
 * person can act on is returned; everything else is somebody else's to move.
 *
 * `needsOf` is the one entry point: the Inbox's "Needs you" list and the rail's Inbox count
 * both call it, so the two never disagree.
 */
import type { ChangeRequest, CheckRun, LandingEvaluation, Post, Principal, Review } from "@/lib/api/types";
import { checkState, isAgent, missingInWords, parseApproval } from "./words";

export interface ChangeFacts {
  repo: string;
  change: ChangeRequest;
  landing: LandingEvaluation;
  checks: CheckRun[];
  reviews: Review[];
  /** Owners (display names) of each path an `approval:owner:<path>` rule names. */
  owners: Record<string, string[]>;
  /** The change is already in the merge queue. */
  queued: boolean;
}

export type NeedKind = "review" | "failing" | "merge" | "blocked" | "post";

export interface Need {
  kind: NeedKind;
  repo: string;
  change: ChangeRequest;
  /** Who is asking: the change's author. */
  who?: Principal;
  /** The caller wrote the change. */
  mine: boolean;
  /** The caller asked for changes on an earlier revision and the author has since revised it. */
  revisedAfterYou: boolean;
  /** Names of failing checks, for "failing". */
  failing: string[];
  /** Outstanding landing rules, in words. */
  reasons: string[];
  /** When the latest revision was pushed; for "post", when the change merged. */
  at?: string;
  /** The agent's post waiting for approval, for "post". */
  post?: Post;
}

/** The order the inbox lists kinds in: what blocks an agent first, then what only waits on you. */
export const KIND_ORDER: Record<NeedKind, number> = { failing: 0, review: 1, blocked: 2, merge: 3, post: 4 };

export const ACTION: Record<NeedKind, string> = { review: "Review", failing: "Open", merge: "Merge", blocked: "Open", post: "Read post" };

export function classify(f: ChangeFacts, me: Principal): Need | null {
  const { change, landing, checks, reviews } = f;
  const mine = change.author?.id === me.id;
  const latest = change.revisions.at(-1);
  const base = { repo: f.repo, change, who: change.author, mine, at: latest?.createdAt ?? change.createdAt, reasons: landing.missing.map(missingInWords) };
  const failing = checks.filter((c) => checkState(c) === "failed").map((c) => c.name);
  // Only checks the policy requires hold a change back; an optional e2e run does not.
  const pending = requiredPending(landing);

  // A failing check stops an agent, or the caller's own change; a colleague fixes their own.
  if (failing.length) return mine || isAgent(change.author) ? { ...base, kind: "failing", failing, revisedAfterYou: false } : null;
  // Still testing: it is in progress, not waiting on anyone yet.
  if (pending) return null;

  const myReviews = reviews.filter((r) => r.reviewer?.id === me.id);
  const approvedLatest = myReviews.some((r) => r.verdict === "VERDICT_APPROVE" && r.revision === (latest?.n ?? 0));
  const askedEarlier = myReviews.some((r) => r.verdict === "VERDICT_REQUEST_CHANGES" && r.revision < (latest?.n ?? 0));
  const canApprove = landing.missing.some((m) => {
    const a = parseApproval(m);
    if (!a || mine) return false;
    if (a.from === "owner") return (f.owners[a.scope] ?? []).includes(me.display);
    return true;
  });
  if (canApprove && !approvedLatest) return { ...base, kind: "review", failing: [], revisedAfterYou: askedEarlier };

  if (landing.allowed) return isAgent(change.author) && !f.queued ? { ...base, kind: "merge", failing: [], revisedAfterYou: false } : null;

  // Blocked by a rule nobody can approve away (a lease, a stack parent): the author or the
  // agent's supervisor has to step in.
  const rules = landing.missing.filter((m) => !parseApproval(m) && !m.startsWith("check:"));
  if (rules.length && (mine || isAgent(change.author))) return { ...base, kind: "blocked", failing: [], revisedAfterYou: false, reasons: rules.map(missingInWords) };
  return null;
}

/** A required check has not finished: EvaluateLanding lists it as `check:<name> (running)`. */
export function requiredPending(landing: LandingEvaluation): boolean {
  return landing.missing.some((m) => /^check:\S+ \((running|queued|missing)\)$/.test(m));
}

export function sortNeeds(needs: Need[]): Need[] {
  return [...needs].sort((a, b) => KIND_ORDER[a.kind] - KIND_ORDER[b.kind] || Date.parse(b.at ?? "") - Date.parse(a.at ?? ""));
}

/**
 * A merged change whose agent-written post waits for a person, when this person may approve it.
 * The server decides who may (`viewer.canApprove`: a human with repo Write who did not write it);
 * this only reads that answer and the state, so a draft on an open change is not asked about yet.
 */
export function postNeed(repo: string, change: ChangeRequest, me: Principal): Need | null {
  const post = change.post;
  if (!post || post.state !== "POST_STATE_PENDING_APPROVAL" || !post.viewer?.canApprove || post.author?.id === me.id) return null;
  return { kind: "post", repo, change, post, who: post.author, mine: false, revisedAfterYou: false, failing: [], reasons: [], at: post.updatedAt ?? post.createdAt };
}

/** Everything that needs this person, in the order the Inbox lists it. */
export function needsOf(open: ChangeFacts[], landed: { repo: string; change: ChangeRequest }[], me: Principal): Need[] {
  const needs: Need[] = [];
  for (const f of open) {
    const n = classify(f, me);
    if (n) needs.push(n);
  }
  for (const l of landed) {
    const n = postNeed(l.repo, l.change, me);
    if (n) needs.push(n);
  }
  return sortNeeds(needs);
}
